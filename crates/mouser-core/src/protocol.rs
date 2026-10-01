//! The wire protocol spoken between the two machines.
//!
//! Messages are encoded with `postcard` (compact, self-delimiting) and then
//! sent over the encrypted, mutually authenticated channel provided by
//! `mouser-net`. Nothing here performs I/O.

use serde::{Deserialize, Serialize};

use crate::layout::Edge;

/// Bumped whenever the message set changes incompatibly.
pub const PROTOCOL_VERSION: u16 = 1;

/// Maximum size of a single encoded message, to bound allocation from a
/// hostile or desynchronized peer.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// A mouse button, in a platform-independent encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Right = 1,
    Middle = 2,
    Back = 3,
    Forward = 4,
}

impl Button {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Button::Left,
            1 => Button::Right,
            2 => Button::Middle,
            3 => Button::Back,
            4 => Button::Forward,
            _ => return None,
        })
    }
}

/// Modifier keys held while an event was produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// Command on macOS, Windows key elsewhere.
    pub meta: bool,
}

/// A keyboard key, identified by USB HID usage code.
///
/// HID usage IDs are layout-independent, which is exactly what we need: the
/// same physical key maps to the same code on both Windows and macOS even
/// though their native key codes differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HidKey(pub u8);

impl HidKey {
    pub const NONE: HidKey = HidKey(0);

    pub fn is_modifier(self) -> bool {
        // Modifier usages live in 0xE0..=0xE7.
        (0xE0..=0xE7).contains(&self.0)
    }
}

/// A single input event travelling from the machine that owns the physical
/// hardware to the machine that mirrors it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum InputEvent {
    /// Relative pointer motion in pixels.
    Move {
        dx: i32,
        dy: i32,
    },
    Scroll {
        dx: i32,
        dy: i32,
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

/// Identity and capabilities announced during connection setup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerHello {
    pub protocol_version: u16,
    /// Human-readable device name shown in the UI.
    pub device_name: String,
    /// Short hash of the shared secret. Both sides display this so users can
    /// confirm out of band that they are talking to the intended machine.
    pub secret_fingerprint: String,
    pub screen_width: u32,
    pub screen_height: u32,
    /// Edge of *this* machine's screen that the peer sits on, from the
    /// peer's point of view.
    pub edge: Edge,
}

/// Everything that can cross the link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    /// First message from the initiator.
    Hello(PeerHello),
    /// Reply from the responder.
    HelloAck(PeerHello),
    /// Confirm the pairing secret matches; carries no secret material.
    PairingConfirmed,
    /// Hand the cursor over to the peer at `fraction` along the shared edge.
    TakeControl {
        edge: Edge,
        fraction: f64,
    },
    /// Give control back to the machine that owns the hardware.
    ReleaseControl,
    /// A batch of input events, to keep ordering and reduce overhead.
    Input(Vec<InputEvent>),
    /// Liveness probe; the peer answers with `Pong` echoing the id.
    Ping(u64),
    Pong(u64),
    /// Graceful shutdown.
    Bye,
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("failed to encode message: {0}")]
    Encode(#[from] postcard::Error),
    #[error("message exceeds {MAX_MESSAGE_BYTES} bytes")]
    TooLarge,
}

impl Message {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let bytes = postcard::to_stdvec(self)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8]) -> Result<Message, ProtocolError> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        postcard::from_bytes(bytes).map_err(ProtocolError::Encode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_message() {
        let cases = vec![
            Message::Hello(PeerHello {
                protocol_version: PROTOCOL_VERSION,
                device_name: "studio-mac".into(),
                secret_fingerprint: "AB12CD34".into(),
                screen_width: 2560,
                screen_height: 1440,
                edge: Edge::Right,
            }),
            Message::PairingConfirmed,
            Message::TakeControl {
                edge: Edge::Left,
                fraction: 0.25,
            },
            Message::ReleaseControl,
            Message::Input(vec![
                InputEvent::Move { dx: 3, dy: -1 },
                InputEvent::Scroll { dx: 0, dy: 1 },
                InputEvent::Button {
                    button: Button::Left,
                    pressed: true,
                },
                InputEvent::Key {
                    key: HidKey(0x04),
                    pressed: true,
                    mods: Modifiers {
                        ctrl: true,
                        ..Default::default()
                    },
                },
            ]),
            Message::Ping(42),
            Message::Pong(42),
            Message::Bye,
        ];
        for case in cases {
            let bytes = case.encode().unwrap();
            assert_eq!(Message::decode(&bytes).unwrap(), case);
        }
    }

    #[test]
    fn decoding_garbage_fails_cleanly() {
        assert!(Message::decode(&[0xff, 0xff, 0xff]).is_err());
    }

    #[test]
    fn oversized_message_rejected_before_encoding() {
        let big = Message::Input(
            (0..40_000)
                .map(|i| InputEvent::Move { dx: i, dy: i })
                .collect(),
        );
        assert!(matches!(big.encode(), Err(ProtocolError::TooLarge)));
    }

    #[test]
    fn modifier_detection() {
        assert!(HidKey(0xE1).is_modifier());
        assert!(!HidKey(0x04).is_modifier());
    }
}
