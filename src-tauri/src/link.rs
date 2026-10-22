//! The link driver: owns the encrypted channel and pumps messages.
//!
//! One task owns the [`mouser_net::Channel`] outright. Reading and writing
//! share it because the Noise transport keeps per-direction nonce counters
//! that must not interleave incorrectly.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mouser_core::config::{Config, Role};
use mouser_core::layout::Edge;
use mouser_core::protocol::{Message, PROTOCOL_VERSION, PeerHello};
use mouser_net::Channel;
use tokio::net::TcpListener;

use crate::bridge::Bridge;
use crate::state::{AppHandle, AppState, Level};

/// Keepalive cadence: short enough to notice a dead peer, long enough to be
/// invisible on a LAN.
const KEEPALIVE: Duration = Duration::from_secs(2);

/// Bound a stalled write or silent peer so local control always returns.
const LINK_TIMEOUT: Duration = Duration::from_secs(10);

/// Upper bound on the handshake, so a silent peer cannot stall the session.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay between reconnection attempts.
const RETRY: Duration = Duration::from_secs(2);

/// How long a farewell `Bye` may take before the link is dropped anyway.
const BYE_TIMEOUT: Duration = Duration::from_millis(250);

/// Spawn the link for the configured role.
pub async fn run(state: Arc<AppState>) {
    let config = state.config();
    let handle = state.handle();

    match config.role {
        Role::Host => host_loop(state, handle, config).await,
        Role::Client => client_loop(state, handle, config).await,
    }
}

fn local_hello(state: &Arc<AppState>, secret: &str) -> PeerHello {
    let snap = state.snapshot();
    PeerHello {
        protocol_version: PROTOCOL_VERSION,
        device_name: snap.device_name,
        secret_fingerprint: mouser_core::config::Config::secret_fingerprint(secret),
        screen_width: snap.screen_width,
        screen_height: snap.screen_height,
        edge: snap.edge,
    }
}

/// Accept connections until the process exits.
async fn host_loop(state: Arc<AppState>, handle: AppHandle, config: Config) {
    let addr = match config.bind_addr() {
        Ok(a) => a,
        Err(e) => {
            handle.log(Level::Error, format!("bad bind address: {e}"));
            return;
        }
    };

    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            handle.log(
                Level::Error,
                format!("cannot listen on {addr}: {e}\nis the port already in use?"),
            );
            return;
        }
    };

    let bound = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    handle.listening(bound);

    loop {
        let (stream, peer_addr) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                handle.log(Level::Warn, format!("accept failed: {e}; retrying"));
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        let peer = peer_addr.to_string();
        handle.log(Level::Info, format!("incoming connection from {peer}"));
        session(
            state.clone(),
            handle.clone(),
            Connection::Stream(stream),
            state.secret_input(),
            peer,
            true,
        )
        .await;
    }
}

/// Keep dialling until the peer answers.
async fn client_loop(state: Arc<AppState>, handle: AppHandle, config: Config) {
    let addr = match config.peer_addr() {
        Ok(a) => a,
        Err(e) => {
            handle.log(Level::Error, format!("no peer address configured: {e}"));
            return;
        }
    };
    handle.set_status("connecting");
    handle.log(Level::Info, format!("connecting to {addr}"));

    loop {
        // The connect covers the TCP dial and the Noise handshake, and both
        // are bounded: a peer that accepts the socket but never speaks — one
        // that is restarting, wedged, or not mouser at all — must not be able
        // to stall the reconnect loop for good.
        let secret = state.secret_input();
        match tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            // Resolved afresh on every attempt, so a hostname follows the
            // peer to whatever address DHCP gives it next.
            Channel::connect(addr.as_str(), &secret, config.require_private_network),
        )
        .await
        {
            Ok(Ok(channel)) => {
                session(
                    state.clone(),
                    handle.clone(),
                    Connection::Ready(Box::new(channel)),
                    secret,
                    addr.clone(),
                    false,
                )
                .await;
                handle.log(Level::Warn, "reconnecting shortly".into());
                tokio::time::sleep(RETRY).await;
            }
            Ok(Err(e)) => {
                handle.log(Level::Warn, format!("connect failed: {e}"));
                tokio::time::sleep(RETRY).await;
            }
            Err(_) => {
                handle.log(
                    Level::Warn,
                    format!("connect to {addr} timed out; retrying"),
                );
                tokio::time::sleep(RETRY).await;
            }
        }
    }
}

/// A live channel, or a stream still waiting for its handshake.
///
/// `Channel` is by far the larger variant (it owns working buffers), so it is
/// boxed to keep the enum small.
enum Connection {
    Ready(Box<Channel>),
    Stream(tokio::net::TcpStream),
}

/// Drive one connected session to its end.
async fn session(
    state: Arc<AppState>,
    handle: AppHandle,
    connection: Connection,
    secret: String,
    peer_label: String,
    accepted: bool,
) {
    let require_private = state.config().require_private_network;

    let mut channel = match connection {
        Connection::Ready(c) => *c,
        Connection::Stream(stream) => {
            match tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                Channel::accept(stream, &secret, require_private),
            )
            .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    handle.log(
                        Level::Warn,
                        format!("handshake with {peer_label} failed: {e}"),
                    );
                    return;
                }
                Err(_) => {
                    handle.log(Level::Warn, "peer stalled during the handshake".into());
                    return;
                }
            }
        }
    };

    let ours = local_hello(&state, &secret);

    // Initiator greets; responder waits to be greeted.
    if !accepted {
        if let Err(e) = send(&mut channel, &Message::Hello(ours.clone())).await {
            handle.log(Level::Error, format!("could not send hello: {e}"));
            return;
        }
    }

    let peer_hello = match tokio::time::timeout(HANDSHAKE_TIMEOUT, channel.recv()).await {
        Ok(Ok(Some(Message::Hello(hello)))) | Ok(Ok(Some(Message::HelloAck(hello)))) => hello,
        Ok(Ok(Some(Message::Bye))) => {
            handle.log(Level::Warn, "peer left before pairing".into());
            return;
        }
        Ok(Ok(None)) => {
            handle.log(Level::Warn, "peer closed before pairing".into());
            return;
        }
        Ok(Ok(Some(_))) => {
            handle.log(Level::Error, "peer sent an unexpected first message".into());
            return;
        }
        Ok(Err(e)) => {
            handle.log(Level::Error, format!("link error: {e}"));
            return;
        }
        Err(_) => {
            handle.log(Level::Warn, "timed out waiting for the peer's hello".into());
            return;
        }
    };

    if peer_hello.protocol_version != PROTOCOL_VERSION {
        handle.log(
            Level::Error,
            format!(
                "peer speaks protocol v{} but this build speaks v{PROTOCOL_VERSION}; \
                 update one of the two",
                peer_hello.protocol_version
            ),
        );
        return;
    }

    // Differing fingerprints mean the two machines hold different secrets.
    // Refusing here beats connecting and sending input that the other side
    // would decode as something else entirely.
    if peer_hello.secret_fingerprint != ours.secret_fingerprint {
        handle.log(
            Level::Error,
            format!(
                "pairing secret mismatch: peer shows {}, this machine shows {}. \
                 Check for a typo, and remember the secret is case sensitive.",
                peer_hello.secret_fingerprint, ours.secret_fingerprint
            ),
        );
        return;
    }

    if accepted {
        if let Err(e) = send(&mut channel, &Message::HelloAck(ours.clone())).await {
            handle.log(Level::Error, format!("could not send hello ack: {e}"));
            return;
        }
    }

    let (bridge, mut out_rx) = Bridge::channel();
    let bridge = Arc::new(bridge);
    state.install_bridge(bridge.clone());
    handle.link_up(peer_hello.clone(), peer_label.clone());

    if let Some(peer_edge) = conflicting_edge(&ours.edge, &peer_hello.edge) {
        handle.log(
            Level::Warn,
            format!(
                "edge settings disagree: this machine has the other screen on the {}, \
                 the peer has it on the {peer_edge}. Control will still hand over, \
                 but the cursor will cross in the wrong direction. Pick opposite sides.",
                ours.edge
            ),
        );
    }

    let mut ticks = tokio::time::interval(KEEPALIVE);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_received = Instant::now();
    let keepalive = !state.config().disable_keepalive;
    let mut outcome = "peer closed the connection".to_string();
    // Set when this side ends a healthy link, so the peer is told rather
    // than left waiting out LINK_TIMEOUT.
    let mut say_bye = false;

    loop {
        if !bridge.is_connected() || state.secret_input() != secret {
            outcome = "input queue failed or pairing secret changed".into();
            say_bye = true;
            break;
        }
        tokio::select! {
            Some(msg) = out_rx.recv() => {
                if let Err(e) = send(&mut channel, &msg).await {
                    outcome = format!("send failed: {e}");
                    break;
                }
            }

            inbound = channel.recv() => {
                match inbound {
                    Ok(Some(msg)) => {
                        last_received = Instant::now();
                        if let Message::Ping(id) = msg {
                            if let Err(e) = send(&mut channel, &Message::Pong(id)).await {
                                outcome = format!("pong failed: {e}");
                                break;
                            }
                        }
                        if apply(&handle, msg, &peer_label) {
                            outcome = "peer ended the session".into();
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        outcome = format!("link error: {e}");
                        break;
                    }
                }
            }

            _ = ticks.tick() => {
                if keepalive {
                    if last_received.elapsed() >= LINK_TIMEOUT {
                        outcome = "peer stopped responding".into();
                        break;
                    }
                    if let Err(e) = send(&mut channel, &Message::Ping(1)).await {
                        outcome = format!("keepalive failed: {e}");
                        break;
                    }
                }
            }
        }
    }

    if say_bye {
        let _ = tokio::time::timeout(BYE_TIMEOUT, send(&mut channel, &Message::Bye)).await;
    }

    handle.link_down(&outcome);
}

// A timed-out write may have sent a partial frame; callers must drop the channel.
async fn send(channel: &mut Channel, msg: &Message) -> anyhow::Result<()> {
    tokio::time::timeout(LINK_TIMEOUT, channel.send(msg)).await??;
    Ok(())
}

/// The two machines must name opposite sides, or the handoff crosses wrongly.
fn conflicting_edge(ours: &Edge, theirs: &Edge) -> Option<Edge> {
    if ours.opposite() == *theirs {
        None
    } else {
        Some(*theirs)
    }
}

/// Apply one inbound message. Returns true when the session should end.
fn apply(handle: &AppHandle, msg: Message, _peer: &str) -> bool {
    match msg {
        Message::Ping(_) => false,
        Message::Pong(_) => false,
        Message::Input(events) => {
            for event in events {
                handle.inject(event);
            }
            false
        }
        Message::TakeControl { edge, fraction } => {
            handle.take_control(edge, fraction);
            false
        }
        Message::ReleaseControl => {
            handle.release_control();
            false
        }
        Message::Bye => true,
        Message::Hello(_) | Message::HelloAck(_) | Message::PairingConfirmed => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pairing_replies_to_ping_and_reconnects_with_a_changed_secret() {
        tokio::time::timeout(Duration::from_secs(8), async {
            let (state, _, _) = crate::state::tests::state_at(Edge::Right);
            let state = Arc::new(state);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            for secret in ["test-pairing-secret-one", "test-pairing-secret-two"] {
                state.set_secret(secret.into());
                let connecting = tokio::spawn(Channel::connect(
                    listener.local_addr().unwrap(),
                    secret,
                    true,
                ));
                let (stream, addr) = listener.accept().await.unwrap();
                let driver = tokio::spawn(session(
                    state.clone(),
                    state.handle(),
                    Connection::Stream(stream),
                    secret.into(),
                    addr.to_string(),
                    true,
                ));
                let mut peer = connecting.await.unwrap().unwrap();
                peer.send(&Message::Hello(local_hello(&state, secret)))
                    .await
                    .unwrap();
                assert!(matches!(
                    peer.recv().await.unwrap(),
                    Some(Message::HelloAck(_))
                ));
                peer.send(&Message::Ping(42)).await.unwrap();
                loop {
                    match peer.recv().await.unwrap().unwrap() {
                        Message::Pong(42) => break,
                        Message::Ping(id) => peer.send(&Message::Pong(id)).await.unwrap(),
                        msg => panic!("unexpected message: {msg:?}"),
                    }
                }
                assert!(state.snapshot().peer_addr.is_some());
                state.set_secret("test-changed-secret".into());
                driver.await.unwrap();
                assert!(state.snapshot().peer_addr.is_none());
            }
        })
        .await
        .expect("pairing or reconnect stalled");
    }
}
