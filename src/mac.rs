//! macOS: a `CGEventTap` for capture, posted `CGEvent`s for injection.
//!
//! The Mac keyboard is captured and forwarded while the PC has the cursor;
//! the PC never sends keys, so only the mouse is injected here. Both need
//! Accessibility and Input Monitoring permission. Injected events carry a
//! marker in `EVENT_SOURCE_USER_DATA` and are never recaptured.
//!
//! Modifiers arrive only as `FlagsChanged` and are decoded into presses and
//! releases. Motion is posted as a drag while a button is held, or apps do
//! not see the drag.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use core_foundation::base::{CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::mach_port::CFMachPortRef;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventField, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGMouseButton, CallbackResult, EventField, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::{CGPoint, CGRect};

use crate::Ev;
use crate::kvm::{CLAIMS, Captured, Io, Rect};
use crate::wire::{Msg, hid_from_mac};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Private in `core-graphics`, but the only way to revive a tap the
    /// system disabled.
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn _CGSDefaultConnection() -> u32;
    fn CGSSetConnectionProperty(cid: u32, target: u32, key: CFStringRef, value: CFTypeRef) -> i32;
}

/// Written to `EVENT_SOURCE_USER_DATA` on everything we post: ASCII "mouser".
const INJECT_TAG: i64 = 0x6D6F_7573_6572;
/// `kCGEventKeycode`, which `core-graphics` does not name.
const FIELD_KEYCODE: CGEventField = 9;

/// After a warp, Quartz reports the jump from the pre-warp position as the
/// next hardware event's delta; that spike is zeroed so it neither jerks
/// the peer nor reads as an instant return.
static WARPED_TARGET: Mutex<Option<CGPoint>> = Mutex::new(None);

pub struct Mac {
    /// Mouse buttons injected down, bit n for button n, to pick drag types.
    held: u8,
    hidden: bool,
    /// Where the last injected motion put the pointer, for clicks.
    position: Option<CGPoint>,
    /// Sub-line wheel travel not yet posted, in 1/120 lines.
    scroll: (i32, i32),
}

pub fn start(tx: Sender<Ev>) -> Result<Mac, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || run_tap(tx, ready_tx));
    // The thread drops the sender without sending when the tap cannot be
    // created, which in practice means missing permission.
    ready_rx.recv().map_err(|_| {
        "no event tap: grant Accessibility and Input Monitoring under \
         System Settings > Privacy & Security, then restart"
            .to_string()
    })?;
    Ok(Mac { held: 0, hidden: false, position: None, scroll: (0, 0) })
}

fn run_tap(sink: Sender<Ev>, ready: Sender<()>) {
    use CGEventType::*;
    let port = Arc::new(AtomicUsize::new(0));
    let tap_port = Arc::clone(&port);
    let events = vec![
        FlagsChanged,
        KeyDown,
        KeyUp,
        LeftMouseDown,
        LeftMouseUp,
        RightMouseDown,
        RightMouseUp,
        OtherMouseDown,
        OtherMouseUp,
        MouseMoved,
        LeftMouseDragged,
        RightMouseDragged,
        OtherMouseDragged,
        ScrollWheel,
    ];
    let callback = move |_: CGEventTapProxy, ty: CGEventType, event: &CGEvent| {
        if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECT_TAG {
            return CallbackResult::Keep;
        }
        if let TapDisabledByTimeout | TapDisabledByUserInput = ty {
            let p = tap_port.load(Ordering::SeqCst);
            if p != 0 {
                // SAFETY: the port is this tap's, valid while its loop runs.
                unsafe { CGEventTapEnable(p as CFMachPortRef, true) };
                log!("macOS disabled the event tap; re-enabled it");
            }
            return CallbackResult::Keep;
        }
        let swallow = match translate(ty, event) {
            Some(Captured::Move { x, y, mut dx, mut dy }) => {
                if let Ok(mut target) = WARPED_TARGET.lock() {
                    if target.is_some_and(|t| (x - t.x).abs() <= 150.0 && (y - t.y).abs() <= 150.0)
                    {
                        *target = None;
                        (dx, dy) = (0.0, 0.0);
                    }
                }
                let _ = sink.send(Ev::Local(Captured::Move { x, y, dx, dy }));
                // Swallowing motion while forwarding keeps the hidden
                // pointer parked at the seam.
                CLAIMS.forwarding()
            }
            Some(Captured::Input(m)) => {
                CLAIMS.claim(m) && sink.send(Ev::Local(Captured::Input(m))).is_ok()
            }
            None => false,
        };
        if swallow { CallbackResult::Drop } else { CallbackResult::Keep }
    };
    let Ok(tap) = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events,
        callback,
    ) else {
        return;
    };
    port.store(tap.mach_port().as_concrete_TypeRef() as usize, Ordering::SeqCst);
    let Ok(source) = tap.mach_port().create_runloop_source(0) else {
        return;
    };
    // SAFETY: reading an immutable CoreFoundation constant.
    CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
    tap.enable();
    if ready.send(()).is_ok() {
        CFRunLoop::run_current();
    }
}

/// The `CGEventFlags` bit a modifier key owns, which says whether a
/// `FlagsChanged` for it was a press or a release.
fn modifier_flag(hid: u8) -> Option<u64> {
    Some(match hid {
        0xE0 | 0xE4 => 0x0004_0000, // control
        0xE1 | 0xE5 => 0x0002_0000, // shift
        0xE2 | 0xE6 => 0x0008_0000, // option
        0xE3 | 0xE7 => 0x0010_0000, // command
        _ => return None,
    })
}

fn translate(ty: CGEventType, event: &CGEvent) -> Option<Captured> {
    use CGEventType::*;
    let field = |f| event.get_integer_value_field(f);
    let key = || hid_from_mac(field(FIELD_KEYCODE) as u16);
    let msg = match ty {
        // CapsLock also arrives here, but it latches; only the eight real
        // modifiers are forwarded.
        FlagsChanged => {
            let hid = key()?;
            Msg::Key(hid, event.get_flags().bits() & modifier_flag(hid)? != 0)
        }
        KeyDown | KeyUp => Msg::Key(key()?, matches!(ty, KeyDown)),
        LeftMouseDown | LeftMouseUp => Msg::Button(0, matches!(ty, LeftMouseDown)),
        RightMouseDown | RightMouseUp => Msg::Button(1, matches!(ty, RightMouseDown)),
        // Other buttons carry their number: 2 middle, 3 back, 4 forward.
        OtherMouseDown | OtherMouseUp => match field(EventField::MOUSE_EVENT_BUTTON_NUMBER) {
            n @ 2..=4 => Msg::Button(n as u8, matches!(ty, OtherMouseDown)),
            _ => return None,
        },
        MouseMoved | LeftMouseDragged | RightMouseDragged | OtherMouseDragged => {
            let p = event.location();
            return Some(Captured::Move {
                x: p.x,
                y: p.y,
                dx: field(EventField::MOUSE_EVENT_DELTA_X) as f64,
                dy: field(EventField::MOUSE_EVENT_DELTA_Y) as f64,
            });
        }
        // Axis 1 is vertical, axis 2 horizontal, in lines.
        ScrollWheel => {
            let lines = |f| (field(f) * 120).clamp(i16::MIN.into(), i16::MAX.into()) as i16;
            let dy = lines(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
            let dx = lines(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
            if dx == 0 && dy == 0 {
                return None;
            }
            Msg::Scroll(dx, dy)
        }
        _ => return None,
    };
    Some(Captured::Input(msg))
}

/// Injected events come from a private source, out of the user's own
/// input state.
fn source() -> Option<CGEventSource> {
    CGEventSource::new(CGEventSourceStateID::Private).ok()
}

fn post(event: CGEvent, number: i64) {
    if number != 0 {
        event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
    }
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECT_TAG);
    event.post(CGEventTapLocation::HID);
}

fn rect(b: CGRect) -> Rect {
    Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height)
}

/// SetsCursorInBackground lets a background process hide the cursor at all.
fn allow_background_cursor() {
    // SAFETY: plain CoreGraphics calls on our own connection.
    unsafe {
        let conn = _CGSDefaultConnection();
        let key = CFString::new("SetsCursorInBackground");
        CGSSetConnectionProperty(
            conn,
            conn,
            key.as_concrete_TypeRef(),
            CFBoolean::true_value().as_CFTypeRef(),
        );
    }
}

impl Mac {
    /// Which event type posts motion, given the buttons held: macOS has
    /// distinct drag types, and apps ignore a plain move during a drag.
    fn drag_kind(&self) -> (CGEventType, CGMouseButton, i64) {
        match self.held.trailing_zeros() {
            0 => (CGEventType::LeftMouseDragged, CGMouseButton::Left, 0),
            1 => (CGEventType::RightMouseDragged, CGMouseButton::Right, 0),
            n @ 2..=4 => (CGEventType::OtherMouseDragged, CGMouseButton::Center, n.into()),
            _ => (CGEventType::MouseMoved, CGMouseButton::Left, 0),
        }
    }

    fn cursor(&self) -> CGPoint {
        self.position
            .or_else(|| CGEvent::new(source()?).ok().map(|e| e.location()))
            .unwrap_or(CGPoint::new(0.0, 0.0))
    }
}

impl Io for Mac {
    const PARK: bool = false;

    /// Every active display folded into one box; event locations and
    /// display bounds share one space (origin top-left of the main display).
    fn screen(&self) -> Rect {
        let displays = CGDisplay::active_displays().unwrap_or_default();
        let union =
            displays.into_iter().map(|id| rect(CGDisplay::new(id).bounds())).reduce(|a, b| Rect {
                left: a.left.min(b.left),
                top: a.top.min(b.top),
                right: a.right.max(b.right),
                bottom: a.bottom.max(b.bottom),
            });
        union.unwrap_or_else(|| rect(CGDisplay::main().bounds()))
    }

    fn inject(&mut self, msg: Msg) {
        use CGEventType::*;
        match msg {
            Msg::Button(n, down) => {
                if down {
                    self.held |= 1 << n;
                } else {
                    self.held &= !(1 << n);
                }
                let (ty, button) = match (n, down) {
                    (0, true) => (LeftMouseDown, CGMouseButton::Left),
                    (0, false) => (LeftMouseUp, CGMouseButton::Left),
                    (1, true) => (RightMouseDown, CGMouseButton::Right),
                    (1, false) => (RightMouseUp, CGMouseButton::Right),
                    (_, true) => (OtherMouseDown, CGMouseButton::Center),
                    (_, false) => (OtherMouseUp, CGMouseButton::Center),
                };
                let at = self.cursor();
                if let Some(e) =
                    source().and_then(|s| CGEvent::new_mouse_event(s, ty, at, button).ok())
                {
                    post(e, if n >= 2 { n.into() } else { 0 });
                }
            }
            Msg::Scroll(dx, dy) => {
                self.scroll.0 += i32::from(dx);
                self.scroll.1 += i32::from(dy);
                let (lx, ly) = (self.scroll.0 / 120, self.scroll.1 / 120);
                self.scroll.0 -= lx * 120;
                self.scroll.1 -= ly * 120;
                if lx == 0 && ly == 0 {
                    return;
                }
                let e = source().and_then(|s| {
                    CGEvent::new_scroll_event(s, ScrollEventUnit::LINE, 2, ly, lx, 0).ok()
                });
                if let Some(e) = e {
                    post(e, 0);
                }
            }
            // The PC keyboard stays on the PC.
            _ => {}
        }
    }

    fn warp(&mut self, x: f64, y: f64) {
        let at = CGPoint::new(x, y);
        if let Ok(mut target) = WARPED_TARGET.lock() {
            *target = Some(at);
        }
        let (ty, button, number) = self.drag_kind();
        if let Some(e) = source().and_then(|s| CGEvent::new_mouse_event(s, ty, at, button).ok()) {
            self.position = Some(at);
            post(e, number);
        }
    }

    // CGDisplayHideCursor keeps a count, so hide and show stay balanced.
    fn hide(&mut self) {
        if std::mem::replace(&mut self.hidden, true) {
            return;
        }
        allow_background_cursor();
        for id in CGDisplay::active_displays().unwrap_or_default() {
            let _ = CGDisplay::new(id).hide_cursor();
        }
    }

    fn show(&mut self) {
        if !std::mem::replace(&mut self.hidden, false) {
            return;
        }
        allow_background_cursor();
        for id in CGDisplay::active_displays().unwrap_or_default() {
            let _ = CGDisplay::new(id).show_cursor();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifiers_own_one_flag_each() {
        for (l, r) in [(0xE0, 0xE4), (0xE1, 0xE5), (0xE2, 0xE6), (0xE3, 0xE7)] {
            assert_eq!(modifier_flag(l), modifier_flag(r));
            assert!(modifier_flag(l).is_some());
        }
        assert_eq!(modifier_flag(0x39), None); // CapsLock latches
    }

    #[test]
    fn drags_follow_the_lowest_held_button() {
        let mut mac = Mac { held: 0, hidden: false, position: None, scroll: (0, 0) };
        assert!(matches!(mac.drag_kind().0, CGEventType::MouseMoved));
        mac.held = 0b1010;
        assert!(matches!(mac.drag_kind().0, CGEventType::RightMouseDragged));
        mac.held = 0b1000;
        assert_eq!(mac.drag_kind().2, 3);
    }
}
