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
use tokio::sync::mpsc;

use crate::bridge::Bridge;
use crate::state::{AppHandle, AppState, Level};

/// Keepalive cadence: short enough to notice a dead peer, long enough to be
/// invisible on a LAN.
const KEEPALIVE: Duration = Duration::from_secs(2);

/// How long to wait before sending a keepalive when nothing else happened.
const POLL: Duration = Duration::from_millis(80);

/// Upper bound on the handshake, so a silent peer cannot stall the session.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay between reconnection attempts.
const RETRY: Duration = Duration::from_secs(2);

/// Spawn the link for the configured role.
pub async fn run(state: Arc<AppState>, secret: String) {
    let config = state.config();
    let handle = state.handle();

    // The UI produces messages on its own thread, so they cross into the
    // async world once here rather than per session. Sessions then borrow the
    // receiver, which is what lets the host accept repeatedly without
    // consuming the queue.
    let (bridge, outbound) = Bridge::channel();
    state.install_bridge(Arc::new(std::sync::Mutex::new(bridge)));

    let (out_tx, mut out_rx) = mpsc::channel::<Message>(OUTBOUND_DEPTH);
    tokio::spawn(async move {
        while let Ok(msg) = outbound.recv() {
            if out_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    match config.role {
        Role::Host => host_loop(state, handle, config, secret, &mut out_rx).await,
        Role::Client => client_loop(state, handle, config, secret, &mut out_rx).await,
    }
}

/// Depth of the async hand-off between the bridge and the link task.
const OUTBOUND_DEPTH: usize = 256;

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
async fn host_loop(
    state: Arc<AppState>,
    handle: AppHandle,
    config: Config,
    secret: String,
    out_rx: &mut mpsc::Receiver<Message>,
) {
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
                handle.log(Level::Warn, format!("accept failed: {e}"));
                break;
            }
        };
        let peer = peer_addr.to_string();
        handle.log(Level::Info, format!("incoming connection from {peer}"));
        session(
            state.clone(),
            handle.clone(),
            Connection::Stream(stream),
            secret.clone(),
            out_rx,
            peer,
            true,
        )
        .await;
    }
}

/// Keep dialling until the peer answers.
async fn client_loop(
    state: Arc<AppState>,
    handle: AppHandle,
    config: Config,
    secret: String,
    out_rx: &mut mpsc::Receiver<Message>,
) {
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
        match Channel::connect(addr, &secret, config.require_private_network).await {
            Ok(channel) => {
                session(
                    state.clone(),
                    handle.clone(),
                    Connection::Ready(Box::new(channel)),
                    secret.clone(),
                    out_rx,
                    addr.to_string(),
                    false,
                )
                .await;
                handle.log(Level::Warn, "reconnecting shortly".into());
                tokio::time::sleep(RETRY).await;
            }
            Err(e) => {
                handle.log(Level::Warn, format!("connect failed: {e}"));
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
    out_rx: &mut mpsc::Receiver<Message>,
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
        if let Err(e) = channel.send(&Message::Hello(ours.clone())).await {
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
        if let Err(e) = channel.send(&Message::HelloAck(ours.clone())).await {
            handle.log(Level::Error, format!("could not send hello ack: {e}"));
            return;
        }
    }

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

    // Move the blocking receiver onto the async world once.
    let mut last_ping = Instant::now();
    let keepalive = !state.config().disable_keepalive;
    let mut outcome = "peer closed the connection".to_string();

    loop {
        tokio::select! {
            // Outbound first: input must not queue behind a poll tick.
            Some(msg) = out_rx.recv() => {
                if let Err(e) = channel.send(&msg).await {
                    outcome = format!("send failed: {e}");
                    break;
                }
            }

            inbound = channel.recv() => {
                match inbound {
                    Ok(Some(msg)) => {
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

            _ = tokio::time::sleep(POLL) => {
                if keepalive && last_ping.elapsed() >= KEEPALIVE {
                    last_ping = Instant::now();
                    if let Err(e) = channel.send(&Message::Ping(1)).await {
                        outcome = format!("keepalive failed: {e}");
                        break;
                    }
                }
            }
        }
    }

    handle.link_down(&outcome);
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
