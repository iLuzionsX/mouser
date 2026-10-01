//! Captured input events and their conversion to the wire format.

use mouser_core::protocol::{Button, HidKey, InputEvent, Modifiers};

/// An event observed from the local device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CapturedEvent {
    /// Absolute cursor position plus the movement that produced it.
    ///
    /// Both are needed: position drives edge detection, delta drives the
    /// peer, because the two screens may have different sizes.
    Move {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
    },
    Scroll {
        dx: f64,
        dy: f64,
    },
    Button {
        button: Button,
        pressed: bool,
    },
    Key {
        key: HidKey,
        pressed: bool,
        mods: Modifiers,
    },
}

impl From<CapturedEvent> for InputEvent {
    fn from(event: CapturedEvent) -> Self {
        match event {
            CapturedEvent::Move { dx, dy, .. } => InputEvent::Move {
                dx: dx.round() as i32,
                dy: dy.round() as i32,
            },
            CapturedEvent::Scroll { dx, dy } => InputEvent::Scroll {
                dx: dx.round() as i32,
                dy: dy.round() as i32,
            },
            CapturedEvent::Button { button, pressed } => InputEvent::Button { button, pressed },
            CapturedEvent::Key { key, pressed, mods } => InputEvent::Key { key, pressed, mods },
        }
    }
}
