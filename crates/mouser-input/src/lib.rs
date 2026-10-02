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

    /// Hide the local cursor while the peer drives.
    fn hide_cursor(&self) -> Result<(), InputError>;

    /// Undo [`InputBackend::hide_cursor`].
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
    use mouser_core::protocol::{HidKey, InputEvent, Modifiers};

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
}
