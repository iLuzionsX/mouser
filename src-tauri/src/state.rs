//! Shared state bridging input capture, the encrypted link, and the UI.
//!
//! The webview calls in through Tauri commands, the link runs on a tokio
//! task, and input capture pushes events from the platform hook thread. All
//! three meet here behind one mutex, which is deliberately simple: the state
//! is tiny and every critical section is microseconds.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mouser_core::config::{Config, Role};
use mouser_core::layout::{Edge, Rect};
use mouser_core::protocol::{InputEvent, Message, PeerHello};
use mouser_core::session::{Session, SessionEvent};
use mouser_input::events::CapturedEvent;
use mouser_input::{InputBackend, Platform};
use serde::Serialize;

use crate::bridge::Bridge;

/// Line levels the UI renders in different colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// One console line.
#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    pub seq: u64,
    pub level: Level,
    pub text: String,
}

/// Everything the UI renders, as one serializable snapshot.
///
/// A single snapshot rather than a stream of partial updates: the UI polls,
/// and one coherent object per poll cannot show a half-applied transition.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub device_name: String,
    pub peer_name: String,
    pub fingerprint: String,
    pub status: String,
    pub role: Role,
    pub edge: Edge,
    pub listening: bool,
    pub bind_addr: String,
    pub peer_addr: Option<String>,
    pub round_trip_ms: Option<f64>,
    pub remote_active: bool,
    pub has_secret: bool,
    /// Why input capture is not running, if it is not.
    ///
    /// Kept so the window can open and explain the fix instead of the process
    /// exiting before there is anything to look at.
    pub capture_error: Option<String>,
    pub screen_width: u32,
    pub screen_height: u32,
    pub log: Vec<LogLine>,
    pub next_seq: u64,
}

/// Bounds of the local screen.
fn screen_of(backend: &Platform) -> Rect {
    backend.screen_info().bounds
}

/// Minimum gap between two handoffs.
///
/// Without this the cursor ping-pongs across the seam: one event crosses,
/// the mirrored move crosses straight back, and control never settles.
const HANDOFF_DEBOUNCE: Duration = Duration::from_millis(250);

/// Scrollback length.
const LOG_CAPACITY: usize = 250;

pub struct AppState {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    config: Config,
    session: Session,
    backend: Platform,
    events_rx: Receiver<CapturedEvent>,
    /// The peer is driving this machine: mirrored input arrives over the link
    /// and the local cursor is hidden.
    remote_active: AtomicBool,
    /// This machine owns the hardware but has handed the visible cursor to the
    /// peer. Captured input is forwarded, and the peer decides when it comes
    /// back.
    forwarding: AtomicBool,
    /// The edge the mirrored cursor entered through, while [`Self::remote_active`].
    ///
    /// The return crossing is tested against this rather than the saved config,
    /// so a peer that announces a different edge than expected still behaves.
    return_edge: Option<Edge>,
    last_cursor: (f64, f64),
    last_handoff: Option<Instant>,
    bridge: Option<Arc<Mutex<Bridge>>>,
    log: Vec<LogLine>,
    next_seq: u64,
    status: String,
    peer_name: String,
    fingerprint: String,
    peer_addr: Option<String>,
    round_trip_ms: Option<f64>,
    listening: bool,
    bind_addr: String,
    /// Held so the UI can hand over a new secret without a restart.
    secret_input: String,
    has_secret: bool,
    /// Set when [`mouser_input::InputBackend::start_capture`] failed, so the UI
    /// can tell the user what is missing instead of the process dying.
    capture_error: Option<String>,
}

impl AppState {
    pub fn new(config: Config, secret: &str) -> anyhow::Result<Self> {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let backend = Platform::new();

        // Missing permission must not stop the window from opening: the user
        // has to see the app to be told what to fix, and the link and UI work
        // without capture. The error is carried in the snapshot instead.
        let capture_error = backend.start_capture(events_tx).err().map(|e| {
            format!(
                "could not capture input: {e}. On macOS grant Accessibility \
                 and Input Recording under System Settings > Privacy & \
                 Security, then restart mouser."
            )
        });

        let fingerprint = Config::secret_fingerprint(secret);
        let bind_addr = config.bind_addr.clone();

        let mut inner = Inner {
            session: Session::new(config.peer_edge),
            fingerprint,
            status: if capture_error.is_some() {
                "no input capture".into()
            } else {
                "starting".into()
            },
            listening: false,
            bind_addr,
            secret_input: secret.to_string(),
            has_secret: !secret.is_empty(),
            events_rx,
            remote_active: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            return_edge: None,
            last_cursor: (0.0, 0.0),
            last_handoff: None,
            bridge: None,
            log: Vec::new(),
            next_seq: 1,
            peer_name: String::new(),
            peer_addr: None,
            round_trip_ms: None,
            config,
            backend,
            capture_error: capture_error.clone(),
        };
        if let Some(error) = &capture_error {
            log(&mut inner, Level::Error, error.clone());
        }
        match &capture_error {
            Some(error) => tracing::warn!("{error}"),
            None => tracing::info!("input capture running"),
        }

        Ok(AppState {
            inner: Arc::new(Mutex::new(inner)),
        })
    }

    pub fn handle(&self) -> AppHandle {
        AppHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// Drain captured input and act on it.
    ///
    /// Called from the UI poll, so the rate at which input is processed is
    /// tied to how often the frontend asks. That is a deliberate trade: a
    /// dedicated thread would add latency and complexity for no visible gain
    /// at pointer-move rates.
    pub fn pump(&self) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        // Drain rather than take one event: a burst of mouse movement is
        // normally several messages deep by the time we get here, and
        // processing the first while discarding the rest would drop deltas.
        while let Ok(event) = guard.events_rx.try_recv() {
            self.handle_event(&mut guard, event);
        }
    }

    fn handle_event(&self, guard: &mut Inner, event: CapturedEvent) {
        let screen = screen_of(&guard.backend);

        match event {
            CapturedEvent::Move { x, y, dx, dy } => {
                // Low-level hooks report absolute positions only, so the delta
                // is derived from the previous position.
                let (prev_x, prev_y) = guard.last_cursor;
                guard.last_cursor = (x, y);
                let (ex, ey) = if dx == 0.0 && dy == 0.0 {
                    (x - prev_x, y - prev_y)
                } else {
                    (dx, dy)
                };

                if guard.remote_active.load(Ordering::Relaxed) {
                    // The peer drives this machine. Motion arrives over the
                    // link and is replayed there; a physical pointer move is a
                    // local echo, so acting on it would double it.
                    return;
                }

                if guard.forwarding.load(Ordering::Relaxed) {
                    // We own the hardware and the cursor is on the peer. Relay
                    // the motion and leave the return decision to the peer,
                    // which is the side that knows where the mirrored cursor
                    // actually is.
                    if ex != 0.0 || ey != 0.0 {
                        self.send(
                            guard,
                            Message::Input(vec![InputEvent::Move {
                                dx: ex.round() as i32,
                                dy: ey.round() as i32,
                            }]),
                        );
                    }
                    return;
                }

                // The cursor is here. Hand it over only while connected: with no
                // peer a stray edge hit would hide the cursor and queue input
                // for a machine that does not exist.
                if !guard.session.is_local() {
                    return;
                }

                let crossed = screen.crossing(guard.session.edge(), x, y, ex, ey);

                guard.session.on_event(
                    SessionEvent::CursorMoved {
                        x,
                        y,
                        dx: ex,
                        dy: ey,
                    },
                    |_, _, _, _, _| false,
                );

                if let Some(fraction) = crossed {
                    if self.debounce(guard) {
                        self.handoff(guard, fraction, "pushed the cursor off the shared edge");
                    }
                }
            }

            CapturedEvent::Scroll { dx, dy } => {
                self.relay(
                    guard,
                    InputEvent::Scroll {
                        dx: dx.round() as i32,
                        dy: dy.round() as i32,
                    },
                );
            }

            CapturedEvent::Button { button, pressed } => {
                self.relay(guard, InputEvent::Button { button, pressed });
            }

            CapturedEvent::Key { key, pressed, mods } => {
                self.relay(guard, InputEvent::Key { key, pressed, mods });
            }
        }
    }

    /// Handle non-pointer input under the same ownership rules as a move.
    ///
    /// While this machine is driven the OS has already applied the physical
    /// event, so replaying it would double it; while forwarding it goes to the
    /// peer; otherwise it stays local.
    fn relay(&self, guard: &mut Inner, event: InputEvent) {
        if guard.remote_active.load(Ordering::Relaxed) {
            return;
        }
        if guard.forwarding.load(Ordering::Relaxed) {
            self.send(guard, Message::Input(vec![event]));
        }
    }

    /// Consume the debounce window. Returns true when a handoff may proceed.
    fn debounce(&self, guard: &mut Inner) -> bool {
        let now = Instant::now();
        if let Some(last) = guard.last_handoff {
            if now.duration_since(last) < HANDOFF_DEBOUNCE {
                return false;
            }
        }
        guard.last_handoff = Some(now);
        true
    }

    /// Give the cursor to the peer.
    fn handoff(&self, guard: &mut Inner, fraction: f64, reason: &str) {
        // Only a connected, locally-owned session may hand over. This is also
        // the guard against a stray edge hit while there is no peer.
        if !guard.session.is_local() {
            return;
        }
        let edge = guard.session.edge();
        if guard.forwarding.swap(true, Ordering::Relaxed) {
            return;
        }

        // Drive the state machine rather than assigning to it, so ownership
        // rules stay in one place.
        guard.session.on_event(
            SessionEvent::CursorMoved {
                x: guard.last_cursor.0,
                y: guard.last_cursor.1,
                dx: edge.is_vertical().to_f64(),
                dy: (!edge.is_vertical()).to_f64(),
            },
            |_, _, _, _, _| true,
        );

        // The visible cursor is now on the peer, so hide ours and keep
        // relaying. `remote_active` stays clear: that flag means the opposite,
        // that this machine is the one being driven.
        let _ = guard.backend.hide_cursor();
        guard.status = "remote".into();
        log(
            guard,
            Level::Info,
            format!(
                "-> peer: {reason} (edge {edge}, from {}, {})",
                guard.last_cursor.0 as i64, guard.last_cursor.1 as i64
            ),
        );
        self.send(guard, Message::TakeControl { edge, fraction });
    }

    /// Queue a message, dropping it if the peer is not keeping up.
    fn send(&self, guard: &mut Inner, msg: Message) {
        let Some(bridge) = guard.bridge.clone() else {
            return;
        };
        match bridge.lock() {
            Ok(bridge) => {
                if let Err(e) = bridge.send(msg) {
                    log(guard, Level::Warn, format!("send dropped: {e}"));
                }
            }
            Err(_) => log(guard, Level::Error, "bridge lock poisoned".into()),
        }
    }

    pub fn install_bridge(&self, bridge: Arc<Mutex<Bridge>>) {
        self.inner.lock().expect("state mutex poisoned").bridge = Some(bridge);
    }

    /// A copy of the current configuration.
    pub fn config(&self) -> Config {
        self.inner
            .lock()
            .expect("state mutex poisoned")
            .config
            .clone()
    }

    /// The secret as typed, for the link task to pick up.
    pub fn secret_input(&self) -> String {
        self.inner
            .lock()
            .expect("state mutex poisoned")
            .secret_input
            .clone()
    }

    pub fn set_secret(&self, secret: String) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        if let Err(e) = Config::validate_secret(&secret) {
            log(&mut guard, Level::Error, e.to_string());
            return;
        }
        guard.fingerprint = Config::secret_fingerprint(&secret);
        guard.secret_input = secret;
        guard.has_secret = true;
        log(&mut guard, Level::Info, "pairing secret updated".into());
    }

    pub fn set_edge(&self, edge: Edge) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.session.set_edge(edge);
        guard.config.peer_edge = edge;
        log(
            &mut guard,
            Level::Info,
            format!("other screen is now to the {edge}"),
        );
        if let Err(e) = guard.config.save() {
            log(
                &mut guard,
                Level::Warn,
                format!("could not save config: {e}"),
            );
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let guard = self.inner.lock().expect("state mutex poisoned");
        let bounds = screen_of(&guard.backend);
        Snapshot {
            device_name: guard.config.device_name.clone(),
            peer_name: guard.peer_name.clone(),
            fingerprint: guard.fingerprint.clone(),
            status: guard.status.clone(),
            role: guard.config.role,
            edge: guard.session.edge(),
            listening: guard.listening,
            bind_addr: guard.bind_addr.clone(),
            peer_addr: guard.peer_addr.clone(),
            round_trip_ms: guard.round_trip_ms,
            remote_active: guard.remote_active.load(Ordering::Relaxed)
                || guard.forwarding.load(Ordering::Relaxed),
            has_secret: guard.has_secret,
            capture_error: guard.capture_error.clone(),
            screen_width: bounds.width.max(0.0) as u32,
            screen_height: bounds.height.max(0.0) as u32,
            log: guard.log.clone(),
            next_seq: guard.next_seq,
        }
    }

    pub fn log(&self, level: Level, text: String) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        log(&mut guard, level, text);
    }

    pub fn shutdown(&self) {
        let guard = self.inner.lock().expect("state mutex poisoned");
        // Never leave the pointer hidden: hiding is a counted display state,
        // and a graceful quit should always undo it.
        let _ = guard.backend.show_cursor();
        guard.backend.stop_capture();
    }
}

/// Append a line to the bounded scrollback.
fn log(guard: &mut Inner, level: Level, text: String) {
    let seq = guard.next_seq;
    guard.next_seq += 1;
    guard.log.push(LogLine { seq, level, text });
    if guard.log.len() > LOG_CAPACITY {
        let excess = guard.log.len() - LOG_CAPACITY;
        guard.log.drain(0..excess);
    }
}

/// Handle shared with the link task, which runs off the UI thread.
#[derive(Clone)]
pub struct AppHandle {
    inner: Arc<Mutex<Inner>>,
}

impl AppHandle {
    pub fn log(&self, level: Level, text: String) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        log(&mut guard, level, text);
    }

    /// A link came up with `peer`.
    pub fn link_up(&self, peer: PeerHello, addr: String) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.peer_name = peer.device_name.clone();
        guard.fingerprint = peer.secret_fingerprint.clone();
        guard.peer_addr = Some(addr.clone());
        guard.round_trip_ms = None;
        guard.status = "connected".into();
        guard.remote_active.store(false, Ordering::Relaxed);
        guard.forwarding.store(false, Ordering::Relaxed);
        guard.return_edge = None;
        let _ = guard.backend.show_cursor();
        guard
            .session
            .on_event(SessionEvent::Connected, |_, _, _, _, _| false);
        log(
            &mut guard,
            Level::Info,
            format!(
                "paired with {} at {addr} [{}]",
                peer.device_name, peer.secret_fingerprint
            ),
        );
    }

    pub fn link_down(&self, reason: &str) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.peer_addr = None;
        guard.round_trip_ms = None;
        guard.status = "disconnected".into();
        guard.remote_active.store(false, Ordering::Relaxed);
        guard.forwarding.store(false, Ordering::Relaxed);
        guard.return_edge = None;
        let _ = guard.backend.show_cursor();
        guard
            .session
            .on_event(SessionEvent::Disconnected, |_, _, _, _, _| false);
        log(&mut guard, Level::Warn, format!("link down: {reason}"));
    }

    pub fn listening(&self, bound: String) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.listening = true;
        guard.bind_addr = bound.clone();
        log(&mut guard, Level::Info, format!("listening on {bound}"));
    }

    pub fn set_rtt(&self, ms: f64) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.round_trip_ms = Some(ms);
    }

    pub fn set_status(&self, status: &str) {
        self.inner.lock().expect("state mutex poisoned").status = status.into();
    }

    /// Replay an event that arrived from the peer.
    ///
    /// Input only means anything while the peer drives this machine. While it
    /// does, this is also where the mirrored cursor is tracked: the peer knows
    /// where the cursor was handed over, but this side is the one that sees
    /// the motion, so the decision that the cursor has been pushed back over
    /// the near edge belongs here. When that happens control returns and the
    /// crossing move itself is not replayed.
    pub fn inject(&self, event: InputEvent) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");

        if !guard.remote_active.load(Ordering::Relaxed) {
            // A late batch after the handoff ended, or input from a peer that
            // is not driving us. Replaying it would move our cursor for no
            // reason, so drop it.
            return;
        }

        if let InputEvent::Move { dx, dy } = event {
            let bounds = screen_of(&guard.backend);
            let (x, y) = guard.last_cursor;
            let (nx, ny) = (x + dx as f64, y + dy as f64);
            guard.last_cursor = (nx, ny);

            let edge = guard.return_edge.unwrap_or(guard.session.edge());
            if bounds
                .crossing(edge, nx, ny, dx as f64, dy as f64)
                .is_some()
            {
                guard.remote_active.store(false, Ordering::Relaxed);
                guard.return_edge = None;
                let _ = guard.backend.show_cursor();
                guard.status = "connected".into();
                // Drive the state machine here too, so ownership rules stay in
                // one place. On this side it is normally still Local, in which
                // case this is a no-op.
                guard.session.on_event(
                    SessionEvent::RemoteCursorMoved {
                        x: nx,
                        y: ny,
                        dx: 0.0,
                        dy: 0.0,
                    },
                    |_, _, _, _, _| false,
                );
                log(
                    &mut guard,
                    Level::Info,
                    "-> local control (cursor pushed back over the edge)".into(),
                );

                let bridge = guard.bridge.clone();
                drop(guard);
                if let Some(bridge) = bridge {
                    let result = bridge
                        .lock()
                        .map(|bridge| bridge.send(Message::ReleaseControl));
                    if let Ok(Err(e)) = result {
                        self.log(Level::Warn, format!("send dropped: {e}"));
                    }
                }
                return;
            }
        }

        if let Err(e) = guard.backend.inject(&event) {
            log(&mut guard, Level::Warn, format!("inject failed: {e}"));
        }
    }

    /// The peer pushed the cursor over the shared edge.
    pub fn take_control(&self, edge: Edge, fraction: f64) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        guard.forwarding.store(false, Ordering::Relaxed);
        if guard.remote_active.swap(true, Ordering::Relaxed) {
            return;
        }

        // Land the mirrored cursor at the matching point so screens of
        // different sizes or resolutions do not visibly jump. The edge we
        // entered through is the mirror of the one the peer announced, and is
        // what a return crossing is tested against.
        let bounds = screen_of(&guard.backend);
        let entry = edge.opposite();
        let (x, y) = bounds.point_at_fraction(entry, fraction);
        guard.last_cursor = (x, y);
        guard.return_edge = Some(entry);

        let _ = guard.backend.hide_cursor();
        guard.status = "remote".into();
        log(
            &mut guard,
            Level::Info,
            format!("<- peer took control at {edge} ({fraction:.2} across)"),
        );
    }

    /// The peer handed control back.
    pub fn release_control(&self) {
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        let remote = guard.remote_active.swap(false, Ordering::Relaxed);
        let forwarding = guard.forwarding.swap(false, Ordering::Relaxed);
        if !remote && !forwarding {
            return;
        }
        guard.return_edge = None;
        let _ = guard.backend.show_cursor();
        guard.status = "connected".into();
        guard.session.on_event(
            SessionEvent::RemoteCursorMoved {
                x: 0.0,
                y: 0.0,
                dx: 0.0,
                dy: 0.0,
            },
            |_, _, _, _, _| false,
        );
        log(
            &mut guard,
            Level::Info,
            "-> local control (peer released)".into(),
        );
    }

    /// Replay an outbound message to the link.
    pub fn send(&self, msg: Message) {
        let bridge = {
            let guard = self.inner.lock().expect("state mutex poisoned");
            guard.bridge.clone()
        };
        if let Some(bridge) = bridge {
            if let Err(e) = bridge.lock().expect("bridge poisoned").send(msg) {
                self.log(Level::Warn, format!("send dropped: {e}"));
            }
        }
    }
}

/// `f64::from(bool)` is not implemented, so make the intent explicit.
trait BoolToF64 {
    fn to_f64(self) -> f64;
}

impl BoolToF64 for bool {
    fn to_f64(self) -> f64 {
        if self { 1.0 } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Build an app state without starting capture, so a test can feed it
    /// captured events directly.
    fn state_at(edge: Edge) -> (AppState, std::sync::mpsc::Sender<CapturedEvent>) {
        let (tx, rx) = channel();
        let config = Config {
            peer_edge: edge,
            ..Config::default()
        };
        let inner = Inner {
            session: Session::new(edge),
            backend: Platform::new(),
            events_rx: rx,
            remote_active: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            return_edge: None,
            last_cursor: (0.0, 0.0),
            last_handoff: None,
            bridge: None,
            log: Vec::new(),
            next_seq: 1,
            status: "starting".into(),
            peer_name: String::new(),
            fingerprint: String::new(),
            peer_addr: None,
            round_trip_ms: None,
            listening: false,
            bind_addr: String::new(),
            secret_input: String::new(),
            has_secret: false,
            capture_error: None,
            config,
        };
        (
            AppState {
                inner: Arc::new(Mutex::new(inner)),
            },
            tx,
        )
    }

    #[test]
    fn edge_hit_without_a_peer_does_not_hand_off() {
        // Regression: this used to fire while disconnected, which hid the
        // cursor and queued input for a peer that did not exist.
        let (state, tx) = state_at(Edge::Right);

        tx.send(CapturedEvent::Move {
            x: 1.0e9,
            y: 50.0,
            dx: 40.0,
            dy: 0.0,
        })
        .expect("the state holds the receiver");
        state.pump();

        let snap = state.snapshot();
        assert!(!snap.remote_active, "must not hand off without a peer");
        assert_eq!(snap.status, "starting");
    }
}
