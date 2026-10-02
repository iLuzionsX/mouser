//! macOS input capture and injection.
//!
//! Capture uses a `CGEventTap` serviced by a run loop on a dedicated thread;
//! injection posts `CGEvent`s. Both require Accessibility permission, which
//! macOS grants per-application and refuses without a signed binary in some
//! cases. There is no workaround: see the README.
//!
//! Key identity comes from the virtual keycode, which is positional and so
//! layout-independent, matching what the Windows backend does with scan codes.
//! Injected events are recognized by a marker written to
//! `kCGEventSourceUserData`, the equivalent of Windows' `dwExtraInfo` tag, so
//! that an event relayed from the peer is never captured again.
//!
//! Two macOS quirks drive the shape of this file:
//!
//! * Modifier keys do **not** arrive as `KeyDown`/`KeyUp`; the tap only sees
//!   them as `FlagsChanged`. They are decoded back into discrete presses and
//!   releases here so that the wire format stays the same on both platforms.
//! * Pointer motion has to be posted as a *drag* event while a button is held,
//!   or the receiving application does not see a drag at all. The held buttons
//!   are tracked so the right event type is chosen.
//!
//! The tap is also re-enabled from inside the callback when the system disables
//! it, which is the documented recovery for a tap that was slow to service.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use core_foundation::base::TCFType;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventField, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};
use mouser_core::layout::Rect;
use mouser_core::protocol::{Button, HidKey, InputEvent, Modifiers};

use crate::events::CapturedEvent;
use crate::keymap;
use crate::{InputBackend, InputError, ScreenInfo};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Enable or disable an event tap.
    ///
    /// `core-graphics` keeps this binding private, but it is the only way to
    /// recover a tap the system disabled: the callback is not handed the tap,
    /// so the Mach port is stashed in shared state instead.
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
}

/// Value written to `kCGEventSourceUserData` on every event we create.
///
/// Any value other than zero makes an event look like a synthetic one to other
/// taps, so this doubles as our "do not capture me" marker. It is ASCII
/// `mouser`, which makes stray events readable in a debugger.
const INJECT_TAG: i64 = 0x6D6F_7573_6572;

/// `kCGEventKeycode`.
///
/// `core-graphics` names every other event field we need but not this one, so
/// the value is spelled out here. It is fixed by the Carbon event API and has
/// not changed since macOS 10.5.
const FIELD_KEYCODE: CGEventField = 9;

/// Device-independent modifier bits within [`CGEventFlags`].
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALT: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

/// Bits tracking which mouse buttons are currently held.
///
/// Needed only to pick the event type for pointer motion: macOS has distinct
/// drag event types and an application ignores motion posted as a plain move
/// while a button is down.
const HELD_LEFT: u8 = 1 << 0;
const HELD_RIGHT: u8 = 1 << 1;
const HELD_MIDDLE: u8 = 1 << 2;
const HELD_BACK: u8 = 1 << 3;
const HELD_FORWARD: u8 = 1 << 4;

/// The event types the tap asks for.
///
/// `FlagsChanged` is required even though modifiers are also reflected in
/// every other event's flags: on macOS that is the *only* notification a
/// modifier key generates, so without it a modifier press is never forwarded
/// to the peer as a key event of its own.
fn events_of_interest() -> Vec<CGEventType> {
    vec![
        CGEventType::FlagsChanged,
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
    ]
}

pub struct MacosBackend {
    /// Set while a tap thread is live, so start/stop are idempotent.
    active: AtomicBool,
    /// The tap thread's run loop, kept so [`Self::stop_capture`] can stop it.
    ///
    /// A `CGEventTap` is only serviced while its run loop runs, so the only way
    /// out of `CFRunLoop::run_current` is a `stop` from another thread.
    loop_ref: Mutex<Option<CFRunLoop>>,
    /// The tap thread, joined on stop so the tap is dropped before we return.
    thread: Mutex<Option<JoinHandle<()>>>,
    /// Mouse buttons currently held, as [`HELD_LEFT`] and friends.
    held: AtomicU8,
    /// Whether the cursor is hidden, so hide/show stay balanced.
    ///
    /// `CGDisplayHideCursor` maintains a count rather than a boolean, so an
    /// extra show would decrement below zero and a later hide/show pair would
    /// no longer match up.
    cursor_hidden: AtomicBool,
}

impl MacosBackend {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            loop_ref: Mutex::new(None),
            thread: Mutex::new(None),
            held: AtomicU8::new(0),
            cursor_hidden: AtomicBool::new(false),
        }
    }
}

impl Default for MacosBackend {
    fn default() -> Self {
        Self::new()
    }
}

/// Build the tap callback.
///
/// `CGEventTap` takes a safe Rust closure rather than a bare function pointer,
/// so the sink is captured directly and no process-global slot is needed. The
/// tap's Mach port is captured the same way, initially zero until the tap
/// exists, so `TapDisabled` can be answered with a re-enable.
fn callback(
    sink: Sender<CapturedEvent>,
    tap_port: Arc<AtomicUsize>,
) -> impl Fn(CGEventTapProxy, CGEventType, &CGEvent) -> CallbackResult {
    move |_proxy, event_type, event| {
        // Our own events, reflected back at us. Ignoring them is what stops a
        // relayed event from being relayed again.
        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECT_TAG {
            return CallbackResult::Keep;
        }

        match event_type {
            // The system disables a tap whose callback blocks. Apple's
            // recovery is to re-enable it, which the callback may do even
            // though it is not handed the tap: the port is published by the
            // tap thread before the run loop starts.
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                let port = tap_port.load(Ordering::SeqCst);
                if port != 0 {
                    // SAFETY: the port belongs to this thread's tap and stays
                    // valid until the run loop stops, which cannot happen while
                    // a callback is executing.
                    unsafe { CGEventTapEnable(port as CFMachPortRef, true) };
                    tracing::warn!("macOS disabled the event tap; re-enabled it");
                } else {
                    tracing::warn!("macOS disabled the event tap before its port was known");
                }
                return CallbackResult::Keep;
            }
            _ => {}
        }

        if let Some(captured) = translate(event_type, event) {
            // A full queue means the link task has fallen behind. Dropping is
            // the right call: blocking inside a tap callback is what gets the
            // tap disabled in the first place.
            let _ = sink.send(captured);
        }

        // Never consume: the local user keeps their input either way.
        CallbackResult::Keep
    }
}

/// Run the tap and its run loop until the loop is stopped.
///
/// Returns without sending on `loop_tx` when the tap cannot be created, which
/// is how [`MacosBackend::start_capture`] tells "no permission" apart from
/// "slow start".
fn run_tap(sink: Sender<CapturedEvent>, tap_port: Arc<AtomicUsize>, loop_tx: Sender<CFRunLoop>) {
    let tap = match CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events_of_interest(),
        callback(sink, Arc::clone(&tap_port)),
    ) {
        Ok(tap) => tap,
        Err(()) => return,
    };

    // Publish before the loop runs so a disable notification arriving at any
    // point can be answered.
    tap_port.store(
        tap.mach_port().as_concrete_TypeRef() as usize,
        Ordering::SeqCst,
    );

    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        return;
    };
    let run_loop = CFRunLoop::get_current();
    run_loop.add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();

    if loop_tx.send(run_loop.clone()).is_err() {
        // The parent gave up waiting; do not run an unobserved loop forever.
        return;
    }
    CFRunLoop::run_current();
    // `tap` is dropped here, which invalidates its port and stops capture.
}

/// The integer value of a `CGEventType`.
///
/// `CGEventType` is a plain `#[repr(u32)]` enum with no `PartialEq`, so two
/// variants cannot be compared directly. Matching on the discriminant keeps
/// that out of the translator's arms.
fn event_code(ty: CGEventType) -> u32 {
    ty as u32
}

/// The virtual keycode carried by a keyboard event.
fn keycode(event: &CGEvent) -> u32 {
    event.get_integer_value_field(FIELD_KEYCODE) as u32
}

/// Build the event source that injected events are attributed to.
///
/// A private state keeps injected events out of the user's real input history.
fn event_source() -> Result<CGEventSource, InputError> {
    CGEventSource::new(CGEventSourceStateID::Private).map_err(|()| {
        InputError::Inject("could not create a CGEventSource; is Accessibility granted?".into())
    })
}

/// Stamp an event so this process recognizes it again on capture.
fn tag_injected(event: &CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECT_TAG);
}

/// The `CGEventFlags` bit a modifier key owns.
///
/// This is what lets a `FlagsChanged` notification be turned into a press or a
/// release: the keycode says which modifier moved, and its bit in the event's
/// flags says whether it went down or up.
fn modifier_flag(key: HidKey) -> Option<u64> {
    Some(match key.0 {
        0xE0 | 0xE4 => FLAG_CONTROL,
        0xE1 | 0xE5 => FLAG_SHIFT,
        0xE2 | 0xE6 => FLAG_ALT,
        0xE3 | 0xE7 => FLAG_COMMAND,
        _ => return None,
    })
}

/// Map a captured native event onto our own type.
fn translate(event_type: CGEventType, event: &CGEvent) -> Option<CapturedEvent> {
    let mods = modifiers(event.get_flags().bits());
    Some(match event_type {
        CGEventType::FlagsChanged => {
            // Only the eight real modifiers are forwarded. CapsLock also
            // arrives here, but it latches rather than pressing, and turning a
            // latched bit into a key press/release pair would leave the peer's
            // lock out of step.
            let key = keymap::from_native(keycode(event))?;
            let flag = modifier_flag(key)?;
            CapturedEvent::Key {
                key,
                pressed: event.get_flags().bits() & flag != 0,
                mods,
            }
        }
        CGEventType::LeftMouseDown => CapturedEvent::Button {
            button: Button::Left,
            pressed: true,
        },
        CGEventType::LeftMouseUp => CapturedEvent::Button {
            button: Button::Left,
            pressed: false,
        },
        CGEventType::RightMouseDown => CapturedEvent::Button {
            button: Button::Right,
            pressed: true,
        },
        CGEventType::RightMouseUp => CapturedEvent::Button {
            button: Button::Right,
            pressed: false,
        },
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            // Position drives edge detection; the deltas drive the peer,
            // because the two screens may differ in size.
            let point = event.location();
            CapturedEvent::Move {
                x: point.x,
                y: point.y,
                dx: event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64,
                dy: event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64,
            }
        }
        CGEventType::KeyDown => CapturedEvent::Key {
            key: keymap::from_native(keycode(event))?,
            pressed: true,
            mods,
        },
        CGEventType::KeyUp => CapturedEvent::Key {
            key: keymap::from_native(keycode(event))?,
            pressed: false,
            mods,
        },
        CGEventType::OtherMouseDown | CGEventType::OtherMouseUp => {
            // Every button past the primary two arrives as an "other" event
            // carrying its own number.
            let button = match event.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER)
            {
                2 => Button::Middle,
                3 => Button::Back,
                4 => Button::Forward,
                _ => return None,
            };
            CapturedEvent::Button {
                button,
                pressed: event_code(event_type) == event_code(CGEventType::OtherMouseDown),
            }
        }
        CGEventType::ScrollWheel => {
            // Axis 1 is vertical, axis 2 horizontal. A trackpad reports
            // fractional lines, which the wire format rounds.
            let dy = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            if dx == 0 && dy == 0 {
                return None;
            }
            CapturedEvent::Scroll {
                dx: dx as f64,
                dy: dy as f64,
            }
        }
        _ => return None,
    })
}

/// Decode [`CGEventFlags`] into the four modifiers the protocol carries.
fn modifiers(bits: u64) -> Modifiers {
    Modifiers {
        shift: bits & FLAG_SHIFT != 0,
        ctrl: bits & FLAG_CONTROL != 0,
        alt: bits & FLAG_ALT != 0,
        meta: bits & FLAG_COMMAND != 0,
    }
}

/// The bit to set in [`MacosBackend::held`] while `button` is down.
fn held_bit(button: Button) -> u8 {
    match button {
        Button::Left => HELD_LEFT,
        Button::Right => HELD_RIGHT,
        Button::Middle => HELD_MIDDLE,
        Button::Back => HELD_BACK,
        Button::Forward => HELD_FORWARD,
    }
}

/// Turn a `CGRect` from CoreGraphics into our own rectangle type.
fn rect_of(bounds: CGRect) -> Rect {
    Rect::new(
        bounds.origin.x,
        bounds.origin.y,
        bounds.size.width,
        bounds.size.height,
    )
}

/// The smallest rectangle containing every input.
///
/// Used to fold the active displays into the one bounding box the protocol
/// carries. Empty input yields `None` so the caller can fall back.
fn union_of(bounds: &[Rect]) -> Option<Rect> {
    let first = bounds.first()?;
    let (mut left, mut top) = (first.left(), first.top());
    let (mut right, mut bottom) = (first.right(), first.bottom());
    for b in &bounds[1..] {
        left = left.min(b.left());
        top = top.min(b.top());
        right = right.max(b.right());
        bottom = bottom.max(b.bottom());
    }
    Some(Rect::new(left, top, right - left, bottom - top))
}

impl InputBackend for MacosBackend {
    fn start_capture(&self, sink: Sender<CapturedEvent>) -> Result<(), InputError> {
        if self.active.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        // Handed from the tap thread once its run loop is live. If the tap
        // cannot be installed the thread returns without ever sending, which
        // drops the sender and makes the receive below fail.
        let (loop_tx, loop_rx) = channel::<CFRunLoop>();

        let handle = std::thread::Builder::new()
            .name("mouser-eventtap".into())
            .spawn({
                let tap_port = Arc::new(AtomicUsize::new(0));
                move || run_tap(sink, tap_port, loop_tx)
            })
            .map_err(|e| InputError::Hook(format!("could not start the tap thread: {e}")))?;

        match loop_rx.recv() {
            Ok(loop_ref) => {
                *self.loop_ref.lock().expect("run loop mutex poisoned") = Some(loop_ref);
                *self.thread.lock().expect("tap thread mutex poisoned") = Some(handle);
                Ok(())
            }
            Err(_) => {
                // The tap thread finished without running its loop, so
                // `CGEventTapCreate` returned null. In practice that means
                // Accessibility permission has not been granted.
                let _ = handle.join();
                self.active.store(false, Ordering::SeqCst);
                Err(InputError::AccessibilityDenied)
            }
        }
    }

    fn stop_capture(&self) {
        if !self.active.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Some(loop_ref) = self
            .loop_ref
            .lock()
            .expect("run loop mutex poisoned")
            .take()
        {
            loop_ref.stop();
        }
        if let Some(handle) = self
            .thread
            .lock()
            .expect("tap thread mutex poisoned")
            .take()
        {
            // Dropping the tap is what actually disables capture, so wait for
            // the thread rather than leaving it running unobserved.
            let _ = handle.join();
        }
    }

    fn inject(&self, event: &InputEvent) -> Result<(), InputError> {
        match *event {
            InputEvent::Move { dx, dy } => self.inject_move(dx, dy),
            InputEvent::Scroll { dx, dy } => self.inject_scroll(dx, dy),
            InputEvent::Button { button, pressed } => self.inject_button(button, pressed),
            InputEvent::Key { key, pressed, .. } => self.inject_key(key, pressed),
        }
    }

    fn screen_info(&self) -> ScreenInfo {
        // The protocol carries one bounding box, so every active display is
        // folded together. Using the main display alone would put the edge in
        // the wrong place as soon as a second monitor is attached.
        //
        // `CGDisplayBounds` and `CGEvent`'s location share one coordinate
        // space (origin at the main display's top-left, y down), so the union
        // needs no flipping.
        let displays: Vec<Rect> = CGDisplay::active_displays()
            .map(|ids| {
                ids.into_iter()
                    .map(|id| rect_of(CGDisplay::new(id).bounds()))
                    .collect()
            })
            .unwrap_or_default();

        let bounds = union_of(&displays).unwrap_or_else(|| rect_of(CGDisplay::main().bounds()));
        ScreenInfo { bounds }
    }

    fn cursor_position(&self) -> Option<(f64, f64)> {
        // A null event is created at the current pointer location.
        let event = CGEvent::new(event_source().ok()?).ok()?;
        let point = event.location();
        Some((point.x, point.y))
    }

    fn warp_cursor(&self, x: f64, y: f64) -> Result<(), InputError> {
        // Reuse the injection path: it posts a tagged mouse event, so the tap
        // that captures local input filters it out instead of forwarding the
        // recentering as if the user had moved the mouse.
        let (cx, cy) = self.cursor_position().unwrap_or((x, y));
        self.inject_move((x - cx).round() as i32, (y - cy).round() as i32)
    }

    fn hide_cursor(&self) -> Result<(), InputError> {
        // `CGDisplayHideCursor` keeps a count, so make this idempotent rather
        // than letting a redundant call skew it.
        if self.cursor_hidden.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        // The display argument is documented as ignored: the hide count is
        // process-wide, not per display.
        CGDisplay::main()
            .hide_cursor()
            .map_err(|e| InputError::Inject(format!("could not hide the cursor: {e}")))
    }

    fn show_cursor(&self) -> Result<(), InputError> {
        if !self.cursor_hidden.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        CGDisplay::main()
            .show_cursor()
            .map_err(|e| InputError::Inject(format!("could not show the cursor: {e}")))
    }
}

impl MacosBackend {
    /// The mouse event type and button to use for injected pointer motion.
    ///
    /// macOS posts a distinct event type for a drag, and applications ignore
    /// motion posted as a plain move while a button is held.
    fn drag_kind(&self) -> (CGEventType, CGMouseButton, i64) {
        let held = self.held.load(Ordering::Relaxed);
        if held & HELD_LEFT != 0 {
            (CGEventType::LeftMouseDragged, CGMouseButton::Left, 0)
        } else if held & HELD_RIGHT != 0 {
            (CGEventType::RightMouseDragged, CGMouseButton::Right, 0)
        } else if held & (HELD_MIDDLE | HELD_BACK | HELD_FORWARD) != 0 {
            // "Other" buttons all share one event type and carry their number
            // in the event, exactly as they do on capture.
            let number = if held & HELD_MIDDLE != 0 {
                2
            } else if held & HELD_BACK != 0 {
                3
            } else {
                4
            };
            (
                CGEventType::OtherMouseDragged,
                CGMouseButton::Center,
                number,
            )
        } else {
            (CGEventType::MouseMoved, CGMouseButton::Left, 0)
        }
    }

    fn inject_move(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        // macOS has no relative-movement primitive for injection, so the delta
        // is applied to where the cursor already is and posted as an absolute
        // move. The peer therefore tracks its own position, which is the same
        // model the Windows backend uses.
        let (x, y) = self.cursor_position().unwrap_or((0.0, 0.0));
        let target = CGPoint::new(x + dx as f64, y + dy as f64);
        let (event_type, mouse_button, number) = self.drag_kind();
        let event = CGEvent::new_mouse_event(event_source()?, event_type, target, mouse_button)
            .map_err(|()| InputError::Inject("could not build a mouse-moved event".into()))?;
        if number != 0 {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        tag_injected(&event);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn inject_scroll(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        // Two wheels: axis 1 vertical, axis 2 horizontal. Line units keep a
        // notch feeling like one notch.
        let event = CGEvent::new_scroll_event(event_source()?, ScrollEventUnit::LINE, 2, dy, dx, 0)
            .map_err(|()| InputError::Inject("could not build a scroll event".into()))?;
        tag_injected(&event);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn inject_button(&self, button: Button, pressed: bool) -> Result<(), InputError> {
        // Record the state first: `inject_move` reads it to decide whether the
        // next motion is a drag.
        let bit = held_bit(button);
        let previous = self.held.load(Ordering::Relaxed);
        let updated = if pressed {
            previous | bit
        } else {
            previous & !bit
        };
        self.held.store(updated, Ordering::Relaxed);

        // `CGMouseButton` only names left, right and centre, so the middle,
        // back and forward buttons are posted as "other" events that carry
        // their own button number. This is how the system itself reports them.
        let (event_type, mouse_button, number) = match (button, pressed) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, 0),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, 0),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, 0),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, 0),
            (Button::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 2),
            (Button::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 2),
            (Button::Back, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 3),
            (Button::Back, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 3),
            (Button::Forward, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, 4),
            (Button::Forward, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, 4),
        };
        let point = self
            .cursor_position()
            .map_or(CGPoint::new(0.0, 0.0), |(x, y)| CGPoint::new(x, y));
        let event = CGEvent::new_mouse_event(event_source()?, event_type, point, mouse_button)
            .map_err(|()| InputError::Inject("could not build a mouse-button event".into()))?;
        if number != 0 {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        tag_injected(&event);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    fn inject_key(&self, key: HidKey, pressed: bool) -> Result<(), InputError> {
        let Some(native) = keymap::to_native(key) else {
            return Ok(());
        };
        let event = CGEvent::new_keyboard_event(event_source()?, native as u16, pressed)
            .map_err(|()| InputError::Inject("could not build a keyboard event".into()))?;
        tag_injected(&event);
        event.post(CGEventTapLocation::HID);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_tag_is_readable_ascii() {
        // An event carrying this is ours. Keep it non-zero: zero means
        // "synthetic" is already true for ordinary events, which would make
        // the marker meaningless.
        assert_ne!(INJECT_TAG, 0);
        // Big-endian, so the most significant bytes read as the word first.
        assert_eq!(&INJECT_TAG.to_be_bytes()[2..], b"mouser");
    }

    #[test]
    fn modifier_bits_decode_independently() {
        assert_eq!(
            modifiers(FLAG_CONTROL | FLAG_COMMAND),
            Modifiers {
                ctrl: true,
                meta: true,
                ..Default::default()
            }
        );
        assert_eq!(
            modifiers(FLAG_SHIFT | FLAG_ALT),
            Modifiers {
                shift: true,
                alt: true,
                ..Default::default()
            }
        );
        assert_eq!(modifiers(0), Modifiers::default());
    }

    #[test]
    fn each_modifier_key_owns_one_flag() {
        // A `FlagsChanged` event is decoded by asking which bit belongs to the
        // keycode that moved, so every modifier must have exactly one.
        for (hid, flag) in [
            (0xE0u8, FLAG_CONTROL),
            (0xE4, FLAG_CONTROL),
            (0xE1, FLAG_SHIFT),
            (0xE5, FLAG_SHIFT),
            (0xE2, FLAG_ALT),
            (0xE6, FLAG_ALT),
            (0xE3, FLAG_COMMAND),
            (0xE7, FLAG_COMMAND),
        ] {
            assert_eq!(modifier_flag(HidKey(hid)), Some(flag), "{hid:#04x}");
        }
        // A real key that is not a modifier must not resolve to a flag, or a
        // stray `FlagsChanged` would masquerade as a modifier press.
        assert_eq!(modifier_flag(HidKey(0x04)), None);
        assert_eq!(modifier_flag(HidKey(0x39)), None);
    }

    #[test]
    fn drag_event_type_follows_the_held_button() {
        let backend = MacosBackend::new();
        assert!(matches!(backend.drag_kind().0, CGEventType::MouseMoved));

        backend.held.store(HELD_LEFT, Ordering::Relaxed);
        let (ty, _, number) = backend.drag_kind();
        assert!(matches!(ty, CGEventType::LeftMouseDragged));
        assert_eq!(number, 0);

        backend.held.store(HELD_RIGHT, Ordering::Relaxed);
        assert!(matches!(
            backend.drag_kind().0,
            CGEventType::RightMouseDragged
        ));

        backend.held.store(HELD_BACK, Ordering::Relaxed);
        let (ty, _, number) = backend.drag_kind();
        assert!(matches!(ty, CGEventType::OtherMouseDragged));
        assert_eq!(number, 3, "an other-button drag must carry its number");
    }

    #[test]
    fn union_of_covers_every_display() {
        let displays = [
            Rect::new(0.0, 0.0, 3440.0, 1440.0),
            Rect::new(615.0, 1440.0, 2056.0, 1329.0),
        ];
        let union = union_of(&displays).expect("non-empty");
        assert_eq!(union.left(), 0.0);
        assert_eq!(union.top(), 0.0);
        assert_eq!(union.right(), 3440.0);
        assert_eq!(union.bottom(), 2769.0);
        assert_eq!(union_of(&[]), None);
    }

    #[test]
    fn tap_subscribes_to_every_event_we_translate() {
        // A type the translator handles but the tap never asks for would be
        // silently dropped at runtime, which is invisible until a user
        // reports a key that "does nothing".
        // Compared as discriminants because `CGEventType` has no `PartialEq`.
        let asked: Vec<u32> = events_of_interest().into_iter().map(event_code).collect();
        for needed in [
            CGEventType::FlagsChanged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
        ] {
            assert!(
                asked.contains(&event_code(needed)),
                "tap does not subscribe to {needed:?}"
            );
        }
    }
}
