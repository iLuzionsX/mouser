//! Platform input capture and injection.
//!
//! The crate exposes one interface, [`InputBackend`], with a Windows and a
//! macOS implementation. Only one is compiled per target; `compile_error!`
//! guards the rest so an unsupported build fails loudly instead of silently
//! doing nothing.
//!
//! Captured events keep their original key identity by normalizing to USB HID
//! usage codes ([`mouser_core::protocol::HidKey`]). Windows reports virtual
//! key codes and macOS reports virtual keycodes, and the two disagree, so
//! mapping each to HID at the boundary is what lets one machine drive the
//! other.

pub mod events;
mod keymap;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

use std::sync::mpsc::Receiver;

use events::CapturedEvent;
use mouser_core::layout::Rect;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!(
    "mouser-input supports Windows and macOS. \
     Linux input capture requires X11/XTest hooks that are not implemented yet."
);

#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("failed to install the input hook: {0}")]
    Hook(String),
    #[error("failed to inject input: {0}")]
    Inject(String),
    #[error("missing accessibility permission (macOS)")]
    AccessibilityDenied,
    #[error("platform backend unavailable: {0}")]
    Unavailable(&'static str),
}

// Hook callbacks cannot lock application state. Movement stays local so Windows
// continues reporting positions; clicks, scrolling and typing belong to the peer.
static FORWARDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_forwarding(active: bool) {
    FORWARDING.store(active, std::sync::atomic::Ordering::Relaxed);
}

fn suppress_local(event: &CapturedEvent) -> bool {
    if !FORWARDING.load(std::sync::atomic::Ordering::Relaxed) {
        return false;
    }
    // A key or button held while crossing went down locally, so its release
    // must reach the local OS too or it stays stuck (Cmd, a window drag).
    if matches!(
        event,
        CapturedEvent::Key { pressed: false, .. } | CapturedEvent::Button { pressed: false, .. }
    ) {
        return false;
    }
    // Windows derives relative mouse motion from absolute cursor movement,
    // so motion must stay local for the hook to keep receiving updates.
    // On macOS, motion deltas come directly from hardware and suppressing
    // the local move keeps the pointer parked and hidden at the seam.
    #[cfg(windows)]
    {
        !matches!(event, CapturedEvent::Move { .. })
    }
    #[cfg(not(windows))]
    {
        let _ = event;
        true
    }
}

/// Bounds and scaling of the machine's screens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenInfo {
    /// Bounding box of all displays in virtual-desktop coordinates.
    pub bounds: Rect,
}

impl Default for ScreenInfo {
    fn default() -> Self {
        ScreenInfo {
            bounds: Rect::new(0.0, 0.0, 1920.0, 1080.0),
        }
    }
}

/// Capture global input and inject input locally.
///
/// Implementations must not consume input they are not asked to: `capture`
/// and `inject` are independent so the peer machine can mirror without
/// eating the local user's events.
pub trait InputBackend: Send + Sync {
    /// Start forwarding global input to `sink`.
    fn start_capture(&self, sink: std::sync::mpsc::Sender<CapturedEvent>)
    -> Result<(), InputError>;

    /// Stop capturing. Safe to call when not capturing.
    fn stop_capture(&self);

    /// Replay an event that arrived from the peer.
    fn inject(&self, event: &mouser_core::protocol::InputEvent) -> Result<(), InputError>;

    /// Current screen layout.
    fn screen_info(&self) -> ScreenInfo;

    /// Absolute cursor position, if the platform will report one.
    fn cursor_position(&self) -> Option<(f64, f64)>;

    /// Move the local cursor to an absolute point without producing a captured
    /// event.
    ///
    /// Used to keep the pointer away from a screen edge while the peer owns
    /// it: once pinned there the OS stops reporting motion, which would starve
    /// the peer of deltas. Implementations must post the move as synthetic so
    /// the local capture hook ignores it.
    fn warp_cursor(&self, x: f64, y: f64) -> Result<(), InputError>;

    /// Hide the local cursor once control has moved to the peer.
    ///
    /// Used by the machine that just handed off: its pointer is parked at the
    /// shared seam and the cursor being driven is the peer's. Backends keep
    /// this idempotent and paired with [`InputBackend::show_cursor`]. On
    /// Windows the hide swaps the system cursors for a blank one, which is
    /// global, so the backend also restores them on startup, panic and drop.
    fn hide_cursor(&self) -> Result<(), InputError>;

    /// Undo [`InputBackend::hide_cursor`], on every path that ends a session
    /// or returns control.
    fn show_cursor(&self) -> Result<(), InputError>;
}

/// The backend for the current target.
#[cfg(windows)]
pub type Platform = windows::WindowsBackend;

#[cfg(target_os = "macos")]
pub type Platform = macos::MacosBackend;

#[cfg(windows)]
pub fn platform() -> Platform {
    Platform::new()
}

#[cfg(target_os = "macos")]
pub fn platform() -> Platform {
    Platform::new()
}

/// Convenience for callers holding a plain `Receiver`.
pub type EventStream = Receiver<CapturedEvent>;

#[cfg(test)]
mod tests {
    use super::*;
    use mouser_core::protocol::{Button, HidKey, InputEvent, Modifiers};

    // FORWARDING is process-global; tests that flip it must not interleave.
    static FORWARDING_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn backend_constructs() {
        let backend = platform();
        let info = backend.screen_info();
        assert!(info.bounds.width > 0.0, "screen width should be positive");
        assert!(info.bounds.height > 0.0);
    }

    #[test]
    fn hid_codes_round_trip_through_the_keymap() {
        // A representative slice of the keyboard, including modifiers and
        // keys that live at different virtual codes on each platform.
        for hid in [
            0x04u8, 0x05, 0x1E, 0x28, 0x29, 0x2B, 0x2C, 0x52, 0x4F, 0x50, 0xE1, 0xE2,
        ] {
            let on_this_platform = keymap::to_native(HidKey(hid)).and_then(keymap::from_native);
            if let Some(round_tripped) = on_this_platform {
                assert_eq!(
                    round_tripped,
                    HidKey(hid),
                    "hid {hid:#04x} did not round trip on this platform"
                );
            }
        }
    }

    #[test]
    fn modifier_keys_are_flagged() {
        assert!(HidKey(0xE0).is_modifier());
        assert!(HidKey(0xE7).is_modifier());
        assert!(!HidKey(0x04).is_modifier());
    }

    #[test]
    fn events_convert_to_wire_format() {
        let captured = CapturedEvent::Key {
            key: HidKey(0x04),
            pressed: true,
            mods: Modifiers {
                ctrl: true,
                ..Default::default()
            },
        };
        let wire: InputEvent = captured.into();
        assert_eq!(
            wire,
            InputEvent::Key {
                key: HidKey(0x04),
                pressed: true,
                mods: Modifiers {
                    ctrl: true,
                    ..Default::default()
                },
            }
        );
    }
    #[test]
    fn forwarding_consumes_typing_but_preserves_motion_for_capture() {
        let key = CapturedEvent::Key {
            key: HidKey(4),
            pressed: true,
            mods: Default::default(),
        };
        let motion = CapturedEvent::Move {
            x: 1.0,
            y: 1.0,
            dx: 1.0,
            dy: 1.0,
        };
        let _guard = FORWARDING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_forwarding(true);
        #[cfg(windows)]
        assert!(!suppress_local(&motion));
        #[cfg(not(windows))]
        assert!(suppress_local(&motion));
        set_forwarding(false);
        assert!(!suppress_local(&key));
    }

    #[test]
    fn forwarding_never_swallows_releases() {
        let key = |pressed| CapturedEvent::Key {
            key: HidKey(0xE3),
            pressed,
            mods: Default::default(),
        };
        let _guard = FORWARDING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_forwarding(true);
        assert!(suppress_local(&key(true)));
        assert!(!suppress_local(&key(false)));
        assert!(!suppress_local(&CapturedEvent::Button {
            button: Button::Left,
            pressed: false,
        }));
        set_forwarding(false);
    }
}
