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
//! **This file has never been run on macOS hardware.** The API surface is
//! verified against the `core-graphics` 0.25 and `core-foundation` 0.10
//! sources, and CI checks that it compiles, but no `CGEventTap` callback has
//! ever fired. Treat it as untested until it has been.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;

use core_foundation::runloop::CFRunLoop;
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventField, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
    CGEventTapPlacement, CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGFloat, CGPoint};
use mouser_core::layout::Rect;
use mouser_core::protocol::{Button, HidKey, InputEvent, Modifiers};

use crate::events::CapturedEvent;
use crate::keymap;
use crate::{InputError, ScreenInfo};

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

/// The event types the tap asks for.
///
/// `FlagsChanged` is deliberately absent. The modifiers that matter are read
/// from each key event's own flags, so subscribing to it as well would
/// duplicate every modifier press and release.
fn events_of_interest() -> Vec<CGEventType> {
    vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
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
}

impl MacosBackend {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            loop_ref: Mutex::new(None),
            thread: Mutex::new(None),
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
/// so the sink is captured directly and no process-global slot is needed.
fn callback(
    sink: Sender<CapturedEvent>,
) -> impl Fn(CGEventTapProxy, CGEventType, &CGEvent) -> CallbackResult {
    move |_proxy, event_type, event| {
        // Our own events, reflected back at us. Ignoring them is what stops a
        // relayed event from being relayed again.
        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECT_TAG {
            return CallbackResult::Keep;
        }

        match event_type {
            // The system disables a tap whose callback blocks. Re-enabling it
            // needs the tap handle, which a callback is not given, so all that
            // can be done here is report it: recovery is a restart.
            CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput => {
                tracing::warn!("macOS disabled the event tap; capture is stalled until restart");
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

/// Map a captured native event onto our own type.
fn translate(event_type: CGEventType, event: &CGEvent) -> Option<CapturedEvent> {
    let mods = modifiers(event.get_flags().bits());
    Some(match event_type {
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
        | CGEventType::RightMouseDragged => {
            // Position drives edge detection; the deltas drive the peer,
            // because the two screens may differ in size.
            let point = event.location();
            CapturedEvent::Move {
                x: point.x as f64,
                y: point.y as f64,
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
                pressed: event_type == CGEventType::OtherMouseDown,
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

impl crate::InputBackend for MacosBackend {
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
            .spawn(move || {
                // `with_enabled` creates the tap, attaches it to this thread's
                // run loop, enables it, and only then runs `with_fn`. The tap
                // lives until `with_fn` returns.
                let _ = CGEventTap::with_enabled(
                    CGEventTapLocation::HID,
                    CGEventTapPlacement::HeadInsertEventTap,
                    CGEventTapOptions::Default,
                    events_of_interest(),
                    callback(sink),
                    move || {
                        // Announce before blocking, so a refused tap can be
                        // told apart from a slow start.
                        let _ = loop_tx.send(CFRunLoop::get_current());
                        CFRunLoop::run_current();
                    },
                );
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
        let bounds = CGDisplay::main().bounds();
        ScreenInfo {
            bounds: Rect::new(
                bounds.origin.x as f64,
                bounds.origin.y as f64,
                bounds.size.width as f64,
                bounds.size.height as f64,
            ),
        }
    }

    fn cursor_position(&self) -> Option<(f64, f64)> {
        let event = CGEvent::new(event_source().ok()?).ok()?;
        let point = event.location();
        Some((point.x as f64, point.y as f64))
    }

    fn hide_cursor(&self) -> Result<(), InputError> {
        CGDisplay::main()
            .hide_cursor()
            .map_err(|e| InputError::Inject(format!("could not hide the cursor: {e}")))
    }

    fn show_cursor(&self) -> Result<(), InputError> {
        CGDisplay::main()
            .show_cursor()
            .map_err(|e| InputError::Inject(format!("could not show the cursor: {e}")))
    }
}

impl MacosBackend {
    fn inject_move(&self, dx: i32, dy: i32) -> Result<(), InputError> {
        // macOS has no relative-movement primitive for injection, so the delta
        // is applied to where the cursor already is and posted as an absolute
        // move. The peer therefore tracks its own position, which is the same
        // model the Windows backend uses.
        let (x, y) = self.cursor_position().unwrap_or((0.0, 0.0));
        let target = CGPoint::new((x + dx as f64) as CGFloat, (y + dy as f64) as CGFloat);
        let event = CGEvent::new_mouse_event(
            event_source()?,
            CGEventType::MouseMoved,
            target,
            CGMouseButton::Left,
        )
        .map_err(|()| InputError::Inject("could not build a mouse-moved event".into()))?;
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
            .map_or(CGPoint::new(0.0, 0.0), |(x, y)| {
                CGPoint::new(x as CGFloat, y as CGFloat)
            });
        let event = CGEvent::new_mouse_event(event_source()?, event_type, point, mouse_button)
            .map_err(|()| InputError::Inject("could not build a mouse-button event".into()))?;
        if number != 0 {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number as i64);
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
        let bytes = INJECT_TAG.to_be_bytes();
        assert_eq!(&bytes, b"mouser");
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
    fn tap_subscribes_to_every_event_we_translate() {
        // A type the translator handles but the tap never asks for would be
        // silently dropped at runtime, which is invisible until a user
        // reports a key that "does nothing".
        let asked = events_of_interest();
        for needed in [
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::MouseMoved,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
        ] {
            assert!(
                asked.contains(&needed),
                "tap does not subscribe to {needed:?}"
            );
        }
    }
}
