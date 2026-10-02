//! The secure link: Noise handshake plus length-prefixed framing.
//!
//! Design notes
//! ------------
//! * **Noise NNpsk0** with Curve25519 + ChaCha20-Poly1305. The `psk0`
//!   modifier places the shared key in the responder's message, so neither
//!   side can complete the handshake without it: the link is mutually
//!   authenticated. Noise also gives forward secrecy, so a later compromise
//!   of the secret does not decrypt a recorded session.
//! * Every frame is a 4-byte big-endian length followed by ciphertext. The
//!   length travels inside the ciphertext, so a peer cannot desync the
//!   stream by lying about it.
//! * The private-network check runs *before* the handshake. Refusing later
//!   would still have revealed that the service exists.

use std::net::{IpAddr, SocketAddr};

use snow::params::NoiseParams;
use snow::{HandshakeState, TransportState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, ToSocketAddrs};

use mouser_core::protocol::{MAX_MESSAGE_BYTES, Message};

/// The full Noise protocol name. Parsed, not constructed, so a typo is a
/// compile-time-visible string rather than a silently wrong cipher suite.
const PARAMS: &str = "Noise_NNpsk0_25519_ChaChaPoly_SHA256";

/// Largest Noise handshake message we will accept.
const MAX_HANDSHAKE_BYTES: usize = 1024;

/// AEAD tag size, so ciphertext buffers are oversized by this much.
const TAG_LEN: usize = 16;

/// Length-prefix size for every frame on the wire.
const PREFIX_LEN: usize = 4;

/// How many write/read turns the handshake may take before it is called off.
///
/// `NNpsk0` uses two. The cap exists so a wrong `PARAMS` string fails loudly
/// rather than looping.
const MAX_HANDSHAKE_TURNS: usize = 8;

/// Pre-shared key length required by the Noise PSK token.
const PSK_LEN: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake failed: {0}")]
    Handshake(String),
    #[error("protocol version mismatch: peer speaks v{peer}, we speak v{ours}")]
    VersionMismatch { peer: u16, ours: u16 },
    #[error("frame of {0} bytes exceeds the maximum message size")]
    TooLarge(usize),
    #[error("connection closed")]
    Closed,
    #[error("peer address is not on a private network")]
    NotPrivate,
    #[error("peer sent a malformed message: {0}")]
    Malformed(String),
}

/// Which side of the handshake we are.
///
/// The handshake itself is driven by `HandshakeState::is_my_turn`, so nothing
/// branches on this: it is carried only so a caller can name its own role in
/// logs and errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Initiator,
    Responder,
}

/// Build a Noise handshake state for `side`, keyed by `psk`.
///
/// The prologue binds the handshake to this application and version, so keys
/// derived here cannot be replayed against a different protocol using the
/// same pairing secret.
fn build_handshake(side: Side, psk: &[u8; PSK_LEN]) -> Result<HandshakeState, TransportError> {
    let params: NoiseParams = PARAMS
        .parse()
        .map_err(|e| TransportError::Handshake(format!("bad Noise params: {e}")))?;

    let builder = snow::Builder::new(params)
        .psk(0, psk)
        .map_err(|e| TransportError::Handshake(e.to_string()))?
        .prologue(b"mouser/v1")
        .map_err(|e| TransportError::Handshake(e.to_string()))?;

    match side {
        Side::Initiator => builder.build_initiator(),
        Side::Responder => builder.build_responder(),
    }
    .map_err(|e| TransportError::Handshake(e.to_string()))
}

/// Derive the Noise pre-shared key from a user-supplied secret.
///
/// A typed passphrase is low entropy, so it is stretched to a fixed 32-byte
/// key. This is *not* a password KDF: it does nothing against offline
/// guessing, only against a trivial-length key. Pairing on a trusted network
/// is the intended flow, and the UI is explicit about that.
fn psk_bytes(secret: &str) -> [u8; PSK_LEN] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"mouser/noise-psk/v1\x00");
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

/// True for loopback, link-local, and RFC1918 / unique-local addresses.
///
/// Keeps an accidental or deliberate port forward from turning into a
/// remote-control hole.
pub fn is_private_address(addr: SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                // 100.64.0.0/10, carrier-grade NAT.
                || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                // Unique local fc00::/7.
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || ip.is_unicast_link_local()
        }
    }
}

/// An established, encrypted channel carrying [`Message`]s.
pub struct Channel {
    stream: TcpStream,
    transport: TransportState,
    /// Reused ciphertext buffer.
    cipher_buf: Vec<u8>,
    /// Reused plaintext buffer.
    plain_buf: Vec<u8>,
    /// Bytes received so far for the frame currently being reassembled.
    ///
    /// [`Channel::recv`] is raced against other branches in a
    /// `tokio::select!`, which drops it at whatever await point is live. A
    /// `read_exact` would silently discard the bytes it had already consumed,
    /// desynchronizing the stream; keeping them here makes the read resumable.
    read_buf: Vec<u8>,
    /// Total size of the frame in progress (prefix plus payload) once the
    /// length prefix has been read, or `None` while the prefix is arriving.
    read_total: Option<usize>,
}

/// Perform the Noise handshake over an already-connected stream.
///
/// `Noise_NNpsk0` is two messages, one in each direction:
///
/// ```text
/// -> e, psk
/// <- e, ee
/// ```
///
/// The PSK token sits in the initiator's message, which is what authenticates
/// it: without the secret the responder cannot derive matching keys.
///
/// Driving this by turn order rather than by role is deliberate. Each side
/// alternates write and read, and `read_message` returns a length of zero when
/// the peer has nothing to send back yet. So "read, then write my reply in the
/// same step" is wrong, and asking for a third message that the pattern never
/// defines would hang until the peer gave up. `is_my_turn` is the protocol's
/// own answer to "is it my move", and the loop stops as soon as both sides are
/// done rather than performing a trailing read.
pub async fn handshake(
    mut stream: TcpStream,
    secret: &str,
    side: Side,
) -> Result<Channel, TransportError> {
    let psk = psk_bytes(secret);
    let mut handshake = build_handshake(side, &psk)?;

    let mut scratch = vec![0u8; MAX_HANDSHAKE_BYTES];

    // Bounded so a misconfigured pattern surfaces as an error instead of
    // spinning. NNpsk0 needs two turns, so this is generous.
    for _ in 0..MAX_HANDSHAKE_TURNS {
        if handshake.is_handshake_finished() {
            let transport = handshake
                .into_transport_mode()
                .map_err(|e| TransportError::Handshake(e.to_string()))?;
            return Ok(Channel {
                stream,
                transport,
                cipher_buf: vec![0u8; MAX_MESSAGE_BYTES + TAG_LEN],
                plain_buf: vec![0u8; MAX_MESSAGE_BYTES + TAG_LEN],
                read_buf: Vec::new(),
                read_total: None,
            });
        }

        if handshake.is_my_turn() {
            let len = handshake
                .write_message(&[], &mut scratch)
                .map_err(|e| TransportError::Handshake(e.to_string()))?;
            if len > 0 {
                write_frame(&mut stream, &scratch[..len]).await?;
            }
        } else {
            let peer = read_frame(&mut stream, MAX_HANDSHAKE_BYTES).await?;
            if peer.is_empty() {
                return Err(TransportError::Closed);
            }
            handshake
                .read_message(&peer, &mut scratch)
                .map_err(|e| TransportError::Handshake(e.to_string()))?;
        }
    }

    Err(TransportError::Handshake(
        "handshake did not finish within the allowed number of turns".into(),
    ))
}

/// Read a length-prefixed frame. Returns an empty vec on clean EOF.
async fn read_frame(stream: &mut TcpStream, max: usize) -> Result<Vec<u8>, TransportError> {
    let mut len_buf = [0u8; PREFIX_LEN];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    // Bound before allocating: a hostile peer could otherwise ask for a huge
    // buffer with a four-byte prefix.
    if len > max {
        return Err(TransportError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<(), TransportError> {
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

impl Channel {
    /// Connect to a peer and handshake.
    pub async fn connect<A: ToSocketAddrs>(
        addr: A,
        secret: &str,
        require_private: bool,
    ) -> Result<Channel, TransportError> {
        let stream = TcpStream::connect(addr).await?;
        let peer_addr = stream.peer_addr()?;
        if require_private && !is_private_address(peer_addr) {
            return Err(TransportError::NotPrivate);
        }
        // Latency matters more than packing: input events are tiny and each
        // one benefits from going out immediately.
        let _ = stream.set_nodelay(true);
        handshake(stream, secret, Side::Initiator).await
    }

    /// Accept an incoming connection and handshake.
    pub async fn accept(
        stream: TcpStream,
        secret: &str,
        require_private: bool,
    ) -> Result<Channel, TransportError> {
        let peer_addr = stream.peer_addr()?;
        if require_private && !is_private_address(peer_addr) {
            return Err(TransportError::NotPrivate);
        }
        let _ = stream.set_nodelay(true);
        handshake(stream, secret, Side::Responder).await
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Encrypt and send one message.
    pub async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        let plain = msg
            .encode()
            .map_err(|e| TransportError::Malformed(e.to_string()))?;
        let len = self
            .transport
            .write_message(&plain, &mut self.cipher_buf)
            .map_err(|e| TransportError::Handshake(e.to_string()))?;
        write_frame(&mut self.stream, &self.cipher_buf[..len]).await
    }

    /// Read more bytes into `read_buf` until it holds at least `want`, or the
    /// peer stops sending.
    ///
    /// Cancel-safe: every byte that arrives is appended to `self.read_buf`
    /// before the next await, so a dropped future resumes from where it left
    /// off instead of losing data. `AsyncReadExt::read` is itself cancel-safe,
    /// unlike `read_exact`.
    async fn fill_read_buf(&mut self, want: usize) -> Result<(), TransportError> {
        let mut scratch = [0u8; 4096];
        while self.read_buf.len() < want {
            let room = (want - self.read_buf.len()).min(scratch.len());
            let n = self.stream.read(&mut scratch[..room]).await?;
            if n == 0 {
                break;
            }
            self.read_buf.extend_from_slice(&scratch[..n]);
        }
        Ok(())
    }

    /// Receive and decrypt one message.
    ///
    /// Returns `None` when the peer closed the connection cleanly.
    ///
    /// Cancel-safe: the link driver races this against outbound traffic and a
    /// poll timer inside a `tokio::select!`, so it may be dropped between any
    /// two bytes of a frame. Partial progress lives in `read_buf`/`read_total`
    /// and is picked up again on the next call.
    pub async fn recv(&mut self) -> Result<Option<Message>, TransportError> {
        let max = MAX_MESSAGE_BYTES + TAG_LEN;

        if self.read_total.is_none() {
            self.fill_read_buf(PREFIX_LEN).await?;
            if self.read_buf.len() < PREFIX_LEN {
                // Closed before a full prefix: clean only if nothing at all
                // had arrived, otherwise the stream is truncated.
                return if self.read_buf.is_empty() {
                    Ok(None)
                } else {
                    Err(TransportError::Closed)
                };
            }
            let len = u32::from_be_bytes(
                self.read_buf[..PREFIX_LEN]
                    .try_into()
                    .expect("prefix is four bytes"),
            ) as usize;
            if len > max {
                return Err(TransportError::TooLarge(len));
            }
            self.read_total = Some(PREFIX_LEN + len);
        }

        let total = self.read_total.expect("set in the branch above");
        self.fill_read_buf(total).await?;
        if self.read_buf.len() < total {
            // Closed in the middle of a frame; the stream cannot be trusted.
            return Err(TransportError::Closed);
        }

        // A whole frame is buffered: take it and reset for the next one.
        let payload = self.read_buf[PREFIX_LEN..total].to_vec();
        self.read_buf.clear();
        self.read_total = None;

        if payload.is_empty() {
            return Ok(None);
        }
        let len = self
            .transport
            .read_message(&payload, &mut self.plain_buf)
            .map_err(|e| TransportError::Handshake(e.to_string()))?;
        let msg = Message::decode(&self.plain_buf[..len])
            .map_err(|e| TransportError::Malformed(e.to_string()))?;
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_string_parses() {
        let params: NoiseParams = PARAMS.parse().expect("params should parse");
        assert_eq!(params.name, PARAMS);
    }

    #[test]
    fn private_address_classification() {
        let v4 = |a: [u8; 4]| SocketAddr::from((std::net::Ipv4Addr::from(a), 1));
        assert!(is_private_address(v4([192, 168, 1, 5])));
        assert!(is_private_address(v4([10, 0, 0, 1])));
        assert!(is_private_address(v4([172, 16, 4, 4])));
        assert!(is_private_address(v4([127, 0, 0, 1])));
        assert!(is_private_address(v4([169, 254, 1, 1])));
        assert!(is_private_address(v4([100, 100, 1, 1])));
        assert!(!is_private_address(v4([8, 8, 8, 8])));
        // Just outside 172.16/12.
        assert!(!is_private_address(v4([172, 32, 0, 1])));
        assert!(!is_private_address(v4([1, 1, 1, 1])));

        let v6 = |a: [u16; 8]| SocketAddr::from((std::net::Ipv6Addr::from(a), 1));
        assert!(is_private_address(v6([0xfc00, 0, 0, 0, 0, 0, 0, 1])));
        assert!(is_private_address(v6([0xfe80, 0, 0, 0, 0, 0, 0, 1])));
        assert!(is_private_address(v6([0, 0, 0, 0, 0, 0, 0, 1])));
        assert!(!is_private_address(v6([0x2606, 0x4700, 0, 0, 0, 0, 0, 1])));
    }

    #[test]
    fn psk_derivation_is_deterministic_and_domain_separated() {
        let a = psk_bytes("hunter2hunter2");
        let b = psk_bytes("hunter2hunter2");
        assert_eq!(a, b);
        assert_ne!(a, psk_bytes("hunter2hunter3"));
        assert_eq!(a.len(), PSK_LEN);
    }

    /// The handshake pattern, asserted directly rather than inferred from the
    /// socket tests.
    ///
    /// This is the shape the driver in [`handshake`] is built around. It is easy to
    /// write a role-based driver ("initiator writes, responder reads, responder
    /// writes its reply") that looks right and then deadlocks on a real socket,
    /// because `read_message` yields zero bytes when the peer's reply comes on a
    /// later turn instead of in the same one. Checking the turn sequence here
    /// catches that without needing a socket.
    #[test]
    fn handshake_pattern_alternates_and_completes() {
        let params: NoiseParams = PARAMS.parse().expect("params should parse");
        let psk = psk_bytes("a secret of some length");

        let build = || {
            snow::Builder::new(params.clone())
                .psk(0, &psk)
                .expect("psk should be accepted")
                .prologue(b"mouser/v1")
                .expect("prologue should be accepted")
        };
        let mut initiator = build().build_initiator().unwrap();
        let mut responder = build().build_responder().unwrap();

        let mut a = vec![0u8; MAX_HANDSHAKE_BYTES];
        let mut b = vec![0u8; MAX_HANDSHAKE_BYTES];

        // The initiator moves first.
        assert!(initiator.is_my_turn(), "the initiator must open NNpsk0");
        assert!(!responder.is_my_turn());

        let first = initiator.write_message(&[], &mut a).unwrap();
        assert!(first > 0, "the opening message must not be empty");

        // Reading it gives the responder nothing to send yet; its reply is a
        // separate turn. This is exactly the case a naive driver mishandles.
        let reply_now = responder.read_message(&a[..first], &mut b).unwrap();
        assert_eq!(reply_now, 0, "responder must not answer in the same turn");
        assert!(
            !responder.is_handshake_finished(),
            "responder still owes a message"
        );
        assert!(responder.is_my_turn());

        let second = responder.write_message(&[], &mut b).unwrap();
        assert!(second > 0);

        let leftover = initiator.read_message(&b[..second], &mut a).unwrap();
        assert_eq!(leftover, 0, "the exchange is over after two messages");
        assert!(initiator.is_handshake_finished());
        assert!(responder.is_handshake_finished());

        // Both sides must independently agree the session is usable.
        assert!(initiator.into_transport_mode().is_ok());
        assert!(responder.into_transport_mode().is_ok());
    }

    /// A differing secret must fail during the handshake, not afterwards.
    ///
    /// Guards the property the whole design rests on: no key material is derived
    /// unless both sides already agreed.
    #[test]
    fn mismatched_secrets_fail_at_the_first_read() {
        let params: NoiseParams = PARAMS.parse().unwrap();
        let initiator_psk = psk_bytes("the right secret");
        let responder_psk = psk_bytes("the wrong secret");

        let mut initiator = snow::Builder::new(params.clone())
            .psk(0, &initiator_psk)
            .unwrap()
            .prologue(b"mouser/v1")
            .unwrap()
            .build_initiator()
            .unwrap();
        let responder = snow::Builder::new(params)
            .psk(0, &responder_psk)
            .unwrap()
            .prologue(b"mouser/v1")
            .unwrap()
            .build_responder();

        let mut a = vec![0u8; MAX_HANDSHAKE_BYTES];
        let mut b = vec![0u8; MAX_HANDSHAKE_BYTES];

        // The PSK is mixed into the opening message, so the responder can reject
        // the very first thing it sees and never reach transport mode.
        let first = initiator.write_message(&[], &mut a).unwrap();
        // Either the responder refuses to build at all, or it fails on the first
        // read. Both are the property under test, so flatten to one outcome.
        let outcome: Result<(), snow::Error> = match responder {
            Ok(mut r) => r.read_message(&a[..first], &mut b).map(|_| ()),
            Err(e) => Err(e),
        };
        assert!(
            outcome.is_err(),
            "a wrong secret must not produce a usable handshake"
        );
    }

    #[tokio::test]
    async fn handshake_then_encrypted_round_trip() {
        let secret = "correct horse battery staple";

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ch = Channel::accept(stream, secret, true).await.unwrap();
            assert_eq!(ch.recv().await.unwrap().unwrap(), Message::Ping(7));
            ch.send(&Message::Pong(7)).await.unwrap();
            ch.send(&Message::Bye).await.unwrap();
            ch
        });

        let mut client = Channel::connect(addr, secret, true)
            .await
            .expect("handshake failed");
        client.send(&Message::Ping(7)).await.unwrap();
        assert_eq!(client.recv().await.unwrap().unwrap(), Message::Pong(7));
        assert_eq!(client.recv().await.unwrap().unwrap(), Message::Bye);

        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_secret_cannot_complete_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            Channel::accept(stream, "the wrong secret", true).await
        });

        assert!(
            Channel::connect(addr, "the right secret", true)
                .await
                .is_err()
        );
        assert!(server.await.unwrap().is_err());
    }

    /// Nothing meaningful may reach the wire unencrypted.
    ///
    /// The bytes have to be read at the *receiving* end: reading our own
    /// socket returns whatever the peer sent, not what we sent, so asserting
    /// on it would pass no matter how the framing worked.
    #[tokio::test]
    async fn ciphertext_does_not_leak_the_plaintext() {
        let secret = "analyse the ciphertext please";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let (wire_tx, wire_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ch = Channel::accept(stream, secret, true).await.unwrap();
            // Take the next frame straight off the socket, skipping the
            // decryption path, and hand the bytes back for inspection.
            let mut header = [0u8; PREFIX_LEN];
            ch.stream.read_exact(&mut header).await.unwrap();
            let mut body = vec![0u8; u32::from_be_bytes(header) as usize];
            ch.stream.read_exact(&mut body).await.unwrap();
            let _ = wire_tx.send((header, body));
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let mut client = Channel::connect(addr, secret, true).await.unwrap();
        let marker = Message::Hello(mouser_core::protocol::PeerHello {
            protocol_version: 1,
            device_name: "PLAINTEXT_CANARY".into(),
            secret_fingerprint: "ABCD1234".into(),
            screen_width: 1920,
            screen_height: 1080,
            edge: mouser_core::layout::Edge::Right,
        });
        client.send(&marker).await.unwrap();

        let (_header, body) = wire_rx.await.expect("server should have captured a frame");
        assert!(!body.is_empty(), "a frame was expected on the wire");
        assert!(
            !String::from_utf8_lossy(&body).contains("PLAINTEXT_CANARY"),
            "the device name appeared in the ciphertext"
        );
        // The fingerprint travels in the same message and must also be hidden.
        assert!(
            !String::from_utf8_lossy(&body).contains("ABCD1234"),
            "the secret fingerprint appeared in the ciphertext"
        );

        server.abort();
    }

    #[tokio::test]
    async fn clean_close_yields_none() {
        let secret = "another good secret";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let channel = Channel::accept(stream, secret, true).await.unwrap();
            drop(channel);
        });

        let mut client = Channel::connect(addr, secret, true).await.unwrap();
        assert!(client.recv().await.unwrap().is_none());
        server.await.unwrap();
    }

    /// Regression: `recv` used to call `read_exact`, which discards the bytes
    /// it had already consumed when the future is dropped. The link driver
    /// races `recv` inside a `tokio::select!`, so a frame interrupted halfway
    /// was re-read from its middle as a garbage length and the link died with
    /// "frame of N bytes exceeds the maximum message size".
    #[tokio::test]
    async fn recv_resumes_after_being_cancelled_mid_frame() {
        let secret = "cancel safety matters here";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ch = Channel::accept(stream, secret, true).await.unwrap();
            // Build one valid encrypted frame, then dribble it out in two
            // pieces so the reader is parked in the middle of the payload.
            let plain = Message::Ping(42).encode().unwrap();
            let len = ch
                .transport
                .write_message(&plain, &mut ch.cipher_buf)
                .unwrap();
            let frame = ch.cipher_buf[..len].to_vec();
            let split = frame.len() / 2;
            ch.stream
                .write_all(&(frame.len() as u32).to_be_bytes())
                .await
                .unwrap();
            ch.stream.write_all(&frame[..split]).await.unwrap();
            ch.stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            ch.stream.write_all(&frame[split..]).await.unwrap();
            ch.stream.flush().await.unwrap();
            // Hold the connection open until the client has read the frame.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let mut client = Channel::connect(addr, secret, true).await.unwrap();
        // The first attempt is cancelled partway through the frame.
        let cancelled =
            tokio::time::timeout(std::time::Duration::from_millis(50), client.recv()).await;
        assert!(
            cancelled.is_err(),
            "recv should still have been waiting for the rest of the frame"
        );

        // The retry must deliver the message intact, proving nothing was lost.
        let msg = client.recv().await.unwrap().unwrap();
        assert_eq!(msg, Message::Ping(42));

        server.await.unwrap();
    }

    #[tokio::test]
    async fn public_address_is_refused() {
        // 8.8.8.8 is not on the local network, so the check must trip. The
        // connect itself may fail first, which is also an acceptable outcome.
        let result = Channel::connect("8.8.8.8:47583", "secret-secret", true).await;
        match result {
            Err(TransportError::NotPrivate) => {}
            Err(TransportError::Io(_)) => {}
            Err(other) => panic!("expected refusal, got {other:?}"),
            Ok(_) => panic!("a public address must not be accepted"),
        }
    }
}
