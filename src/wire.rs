//! The secure link and what travels over it.
//!
//! * **Noise NNpsk0** (Curve25519, ChaCha20-Poly1305, SHA-256) via `snow`.
//!   The PSK is mixed into the first message, so neither side completes the
//!   handshake without the shared secret; ephemeral keys give forward secrecy.
//! * Every event is a fixed 8-byte plaintext, so every transport frame is a
//!   fixed 24 bytes of ciphertext: no length prefix to trust or desync.
//! * Nonces are implicit counters, one per direction, so a replayed, dropped
//!   or reordered frame fails authentication and ends the link.
//! * Keys travel as USB HID usage codes, which are positional on both sides.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;

use snow::StatelessTransportState;

use crate::kvm::Edge;

const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";
/// Binds keys to this wire format; v1 was the old length-prefixed postcard one,
/// v2 had no `Focus`.
const PROLOGUE: &[u8] = b"mouser/v3";
const TAG: usize = 16;
pub const FRAME: usize = 8;
const CIPHER: usize = FRAME + TAG;
/// Both NNpsk0 handshake messages are one ephemeral key plus an empty
/// encrypted payload.
const HANDSHAKE: usize = 32 + TAG;

/// One event or control message. Buttons are 0 left, 1 right, 2 middle,
/// 3 back, 4 forward; scroll is in Windows wheel units (120 per notch).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    /// Keepalive, so a quiet link is not mistaken for a dead one.
    Nop,
    Move(i16, i16),
    Button(u8, bool),
    Scroll(i16, i16),
    Key(u8, bool),
    /// The sender pushed the cursor off its `Edge`, this far along it (0..=65535).
    Take(Edge, u16),
    Release,
    Bye,
    /// The sender's own mouse is in use on its own screen, so the keyboard
    /// belongs there.
    Focus,
}

impl Msg {
    pub fn encode(self) -> [u8; FRAME] {
        let mut b = [0u8; FRAME];
        let (tag, x, y) = match self {
            Msg::Nop => (0, 0, 0),
            Msg::Move(dx, dy) => (1, dx as u16, dy as u16),
            Msg::Button(n, down) => (2, n.into(), down.into()),
            Msg::Scroll(dx, dy) => (3, dx as u16, dy as u16),
            Msg::Key(hid, down) => (4, hid.into(), down.into()),
            Msg::Take(edge, at) => (5, edge as u16, at),
            Msg::Release => (6, 0, 0),
            Msg::Bye => (7, 0, 0),
            Msg::Focus => (8, 0, 0),
        };
        b[0] = tag;
        b[1..3].copy_from_slice(&x.to_le_bytes());
        b[3..5].copy_from_slice(&y.to_le_bytes());
        b
    }

    pub fn decode(b: [u8; FRAME]) -> Option<Msg> {
        let x = u16::from_le_bytes([b[1], b[2]]);
        let y = u16::from_le_bytes([b[3], b[4]]);
        let flag = |v: u16| (v <= 1).then_some(v == 1);
        Some(match b[0] {
            0 => Msg::Nop,
            1 => Msg::Move(x as i16, y as i16),
            2 if x < 5 => Msg::Button(x as u8, flag(y)?),
            3 => Msg::Scroll(x as i16, y as i16),
            4 if x < 256 => Msg::Key(x as u8, flag(y)?),
            5 => Msg::Take(*Edge::ALL.get(x as usize)?, y),
            6 => Msg::Release,
            7 => Msg::Bye,
            8 => Msg::Focus,
            _ => return None,
        })
    }
}

/// Stretch the typed secret to the 32-byte Noise PSK. Not a password KDF:
/// pairing on a trusted LAN with a decent secret is the intended use.
pub fn psk(secret: &str) -> [u8; 32] {
    digest(b"mouser/noise-psk/v1\x00", secret)
}

/// Short tag both sides print, to confirm by eye they hold the same secret.
pub fn fingerprint(secret: &str) -> String {
    let d = digest(b"mouser/pairing-fingerprint/v1\x00", secret);
    d[..4].iter().map(|b| format!("{b:02X}")).collect()
}

fn digest(domain: &[u8], secret: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::new().chain_update(domain).chain_update(secret.as_bytes()).finalize().into()
}

/// Loopback, link-local, RFC 1918, CGNAT and unique-local addresses, so a
/// stray port forward does not turn into remote control.
pub fn is_private(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || ip.is_unicast_link_local()
        }
    }
}

/// The sending half, owned by the state machine's thread.
pub struct Link {
    stream: TcpStream,
    noise: Arc<StatelessTransportState>,
    nonce: u64,
}

/// The receiving half, owned by the connection's reader thread.
pub struct Reader {
    stream: TcpStream,
    noise: Arc<StatelessTransportState>,
    nonce: u64,
}

fn noise_err(e: snow::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Run the two-message NNpsk0 handshake over a connected stream.
///
/// The caller sets the stream's timeouts first, so a silent peer cannot hold
/// this forever.
pub fn handshake(
    mut stream: TcpStream,
    psk: &[u8; 32],
    initiator: bool,
) -> io::Result<(Link, Reader)> {
    let builder = snow::Builder::new(PARAMS.parse().map_err(noise_err)?)
        .psk(0, psk)
        .and_then(|b| b.prologue(PROLOGUE))
        .map_err(noise_err)?;
    let mut hs = if initiator { builder.build_initiator() } else { builder.build_responder() }
        .map_err(noise_err)?;

    let mut msg = [0u8; HANDSHAKE];
    let mut payload = [0u8; HANDSHAKE];
    while !hs.is_handshake_finished() {
        if hs.is_my_turn() {
            let n = hs.write_message(&[], &mut msg).map_err(noise_err)?;
            debug_assert_eq!(n, HANDSHAKE);
            stream.write_all(&msg[..n])?;
        } else {
            stream.read_exact(&mut msg)?;
            hs.read_message(&msg, &mut payload).map_err(noise_err)?;
        }
    }
    let noise = Arc::new(hs.into_stateless_transport_mode().map_err(noise_err)?);
    let reader = Reader { stream: stream.try_clone()?, noise: Arc::clone(&noise), nonce: 0 };
    let link = Link { stream, noise, nonce: 0 };
    Ok((link, reader))
}

impl Link {
    pub fn send(&mut self, msg: Msg) -> io::Result<()> {
        let mut out = [0u8; CIPHER];
        self.noise.write_message(self.nonce, &msg.encode(), &mut out).map_err(noise_err)?;
        self.nonce += 1;
        self.stream.write_all(&out)
    }

    /// Unblock the reader too, which shares this socket.
    pub fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

impl Reader {
    pub fn recv(&mut self) -> io::Result<Msg> {
        let mut frame = [0u8; CIPHER];
        self.stream.read_exact(&mut frame)?;
        let mut plain = [0u8; CIPHER];
        self.noise.read_message(self.nonce, &frame, &mut plain).map_err(noise_err)?;
        self.nonce += 1;
        let mut b = [0u8; FRAME];
        b.copy_from_slice(&plain[..FRAME]);
        Msg::decode(b).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown frame"))
    }
}

/// `(hid, mac keycode, windows scan code)`; `0xE0__` marks an extended scan.
/// Only the Mac keyboard is forwarded, so these are the only two directions.
#[rustfmt::skip]
const KEYS: &[(u8, u8, u16)] = &[
    (0x04, 0x00, 0x1E), (0x05, 0x0B, 0x30), (0x06, 0x08, 0x2E), (0x07, 0x02, 0x20), // A B C D
    (0x08, 0x0E, 0x12), (0x09, 0x03, 0x21), (0x0A, 0x05, 0x22), (0x0B, 0x04, 0x23), // E F G H
    (0x0C, 0x22, 0x17), (0x0D, 0x26, 0x24), (0x0E, 0x28, 0x25), (0x0F, 0x25, 0x26), // I J K L
    (0x10, 0x2E, 0x32), (0x11, 0x2D, 0x31), (0x12, 0x1F, 0x18), (0x13, 0x23, 0x19), // M N O P
    (0x14, 0x0C, 0x10), (0x15, 0x0F, 0x13), (0x16, 0x01, 0x1F), (0x17, 0x11, 0x14), // Q R S T
    (0x18, 0x20, 0x16), (0x19, 0x09, 0x2F), (0x1A, 0x0D, 0x11), (0x1B, 0x07, 0x2D), // U V W X
    (0x1C, 0x10, 0x15), (0x1D, 0x06, 0x2C),                                         // Y Z
    (0x1E, 0x12, 0x02), (0x1F, 0x13, 0x03), (0x20, 0x14, 0x04), (0x21, 0x15, 0x05), // 1 2 3 4
    (0x22, 0x17, 0x06), (0x23, 0x16, 0x07), (0x24, 0x1A, 0x08), (0x25, 0x1C, 0x09), // 5 6 7 8
    (0x26, 0x19, 0x0A), (0x27, 0x1D, 0x0B),                                         // 9 0
    (0x28, 0x24, 0x1C), (0x29, 0x35, 0x01), (0x2A, 0x33, 0x0E), (0x2B, 0x30, 0x0F), // Enter Esc Bksp Tab
    (0x2C, 0x31, 0x39), (0x2D, 0x1B, 0x0C), (0x2E, 0x18, 0x0D), (0x2F, 0x21, 0x1A), // Space - = [
    (0x30, 0x1E, 0x1B), (0x31, 0x2A, 0x2B), (0x33, 0x29, 0x27), (0x34, 0x27, 0x28), // ] \ ; '
    (0x35, 0x32, 0x29), (0x36, 0x2B, 0x33), (0x37, 0x2F, 0x34), (0x38, 0x2C, 0x35), // ` , . /
    (0x39, 0x39, 0x3A), (0x64, 0x0A, 0x56),                                         // Caps, ISO §
    (0x3A, 0x7A, 0x3B), (0x3B, 0x78, 0x3C), (0x3C, 0x63, 0x3D), (0x3D, 0x76, 0x3E), // F1-F4
    (0x3E, 0x60, 0x3F), (0x3F, 0x61, 0x40), (0x40, 0x62, 0x41), (0x41, 0x64, 0x42), // F5-F8
    (0x42, 0x65, 0x43), (0x43, 0x6D, 0x44), (0x44, 0x67, 0x57), (0x45, 0x6F, 0x58), // F9-F12
    (0x46, 0x69, 0xE037), (0x47, 0x6B, 0x46),                                       // F13=PrtSc F14=ScrLk
    (0x49, 0x72, 0xE052), (0x4A, 0x73, 0xE047), (0x4B, 0x74, 0xE049), (0x4C, 0x75, 0xE053), // Help=Ins Home PgUp Del
    (0x4D, 0x77, 0xE04F), (0x4E, 0x79, 0xE051),                                     // End PgDn
    (0x4F, 0x7C, 0xE04D), (0x50, 0x7B, 0xE04B), (0x51, 0x7D, 0xE050), (0x52, 0x7E, 0xE048), // arrows R L D U
    (0x53, 0x47, 0xE045), (0x54, 0x4B, 0xE035), (0x55, 0x43, 0x37), (0x56, 0x4E, 0x4A), // Clear=NumLk / * -
    (0x57, 0x45, 0x4E), (0x58, 0x4C, 0xE01C), (0x63, 0x41, 0x53), (0x62, 0x52, 0x52), // + Enter . 0
    (0x59, 0x53, 0x4F), (0x5A, 0x54, 0x50), (0x5B, 0x55, 0x51), (0x5C, 0x56, 0x4B), // keypad 1-4
    (0x5D, 0x57, 0x4C), (0x5E, 0x58, 0x4D), (0x5F, 0x59, 0x47), (0x60, 0x5B, 0x48), // keypad 5-8
    (0x61, 0x5C, 0x49),                                                             // keypad 9
    (0xE0, 0x3B, 0x1D), (0xE1, 0x38, 0x2A), (0xE2, 0x3A, 0x38), (0xE3, 0x37, 0xE05B), // L Ctrl Shift Opt Cmd
    (0xE4, 0x3E, 0xE01D), (0xE5, 0x3C, 0x36), (0xE6, 0x3D, 0xE038), (0xE7, 0x36, 0xE05C), // R Ctrl Shift Opt Cmd
];

#[cfg(any(target_os = "macos", test))]
pub fn hid_from_mac(keycode: u16) -> Option<u8> {
    KEYS.iter().find(|k| u16::from(k.1) == keycode).map(|k| k.0)
}

#[cfg(any(windows, test))]
pub fn scan_from_hid(hid: u8) -> Option<u16> {
    KEYS.iter().find(|k| k.0 == hid).map(|k| k.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn every_frame_round_trips() {
        for msg in [
            Msg::Nop,
            Msg::Move(-32768, 32767),
            Msg::Move(-3, 7),
            Msg::Button(4, true),
            Msg::Button(0, false),
            Msg::Scroll(-120, 240),
            Msg::Key(0xE7, true),
            Msg::Key(0x04, false),
            Msg::Take(Edge::Bottom, 65535),
            Msg::Take(Edge::Left, 0),
            Msg::Release,
            Msg::Bye,
            Msg::Focus,
        ] {
            assert_eq!(Msg::decode(msg.encode()), Some(msg), "{msg:?}");
        }
    }

    #[test]
    fn malformed_frames_are_rejected() {
        for bad in [
            [9, 0, 0, 0, 0, 0, 0, 0],
            [2, 5, 0, 1, 0, 0, 0, 0],
            [4, 4, 0, 2, 0, 0, 0, 0],
            [5, 4, 0, 0, 0, 0, 0, 0],
        ] {
            assert_eq!(Msg::decode(bad), None, "{bad:?}");
        }
    }

    type Ends = io::Result<(Link, Reader)>;

    fn pair(a: &str, b: &str) -> (Ends, Ends) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let psk_b = psk(b);
        // A failed handshake drops the socket, which is what fails the client.
        let server = std::thread::spawn(move || handshake(listener.accept()?.0, &psk_b, false));
        let client = TcpStream::connect(addr).unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let c = handshake(client, &psk(a), true);
        (c, server.join().unwrap())
    }

    #[test]
    fn matching_secrets_talk_both_ways() {
        let (c, s) = pair("correct horse", "correct horse");
        let ((mut c_link, mut c_read), (mut s_link, mut s_read)) = (c.unwrap(), s.unwrap());
        for i in 0..3 {
            c_link.send(Msg::Move(i, -i)).unwrap();
            assert_eq!(s_read.recv().unwrap(), Msg::Move(i, -i));
            s_link.send(Msg::Key(4, true)).unwrap();
            assert_eq!(c_read.recv().unwrap(), Msg::Key(4, true));
        }
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let (c, s) = pair("correct horse", "battery staple");
        assert!(s.is_err(), "responder accepted the wrong secret");
        assert!(c.is_err(), "initiator finished without the responder");
    }

    #[test]
    fn private_address_classification() {
        let v4 = |a: [u8; 4]| SocketAddr::from((a, 1));
        for ok in
            [[192, 168, 68, 65], [10, 0, 0, 1], [172, 16, 4, 4], [127, 0, 0, 1], [169, 254, 1, 1]]
        {
            assert!(is_private(v4(ok)), "{ok:?}");
        }
        assert!(is_private(v4([100, 100, 1, 1])));
        for bad in [[8, 8, 8, 8], [172, 32, 0, 1], [1, 1, 1, 1]] {
            assert!(!is_private(v4(bad)), "{bad:?}");
        }
        let v6 = |a: [u16; 8]| SocketAddr::from((a, 1));
        assert!(is_private(v6([0xfc00, 0, 0, 0, 0, 0, 0, 1])));
        assert!(is_private(v6([0xfe80, 0, 0, 0, 0, 0, 0, 1])));
        assert!(!is_private(v6([0x2606, 0x4700, 0, 0, 0, 0, 0, 1])));
    }

    #[test]
    fn secrets_derive_distinct_keys() {
        assert_eq!(psk("hunter22"), psk("hunter22"));
        assert_ne!(psk("hunter22"), psk("hunter23"));
        assert_eq!(fingerprint("hunter22").len(), 8);
        assert_ne!(fingerprint("hunter22"), fingerprint("hunter23"));
    }

    #[test]
    fn key_columns_are_unique() {
        for col in 0..3 {
            let mut seen = std::collections::HashSet::new();
            for k in KEYS {
                let v = [u16::from(k.0), u16::from(k.1), k.2][col];
                assert!(seen.insert(v), "column {col} repeats {v:#x}");
            }
        }
    }

    #[test]
    fn mac_keys_reach_the_right_windows_scan() {
        // (mac keycode, hid, scan) from Apple's kVK_* and Microsoft's set-1 tables.
        for (mac, hid, scan) in [
            (0x00, 0x04, 0x1E),   // A
            (0x0C, 0x14, 0x10),   // Q
            (0x24, 0x28, 0x1C),   // Return
            (0x33, 0x2A, 0x0E),   // Delete -> Backspace
            (0x75, 0x4C, 0xE053), // Forward delete
            (0x7B, 0x50, 0xE04B), // Left arrow
            (0x37, 0xE3, 0xE05B), // Command -> Win
            (0x3A, 0xE2, 0x38),   // Option -> Alt
            (0x3D, 0xE6, 0xE038), // Right option -> AltGr
            (0x0A, 0x64, 0x56),   // ISO section key
            (0x4C, 0x58, 0xE01C), // Keypad enter
        ] {
            assert_eq!(hid_from_mac(mac), Some(hid), "mac {mac:#x}");
            assert_eq!(scan_from_hid(hid), Some(scan), "hid {hid:#x}");
        }
        assert_eq!(hid_from_mac(0x3F), None); // fn
    }
}
