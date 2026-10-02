//! Shared state bridging input capture, the encrypted link, and the UI.
//!
//! The webview calls in through Tauri commands, the link runs on a tokio
//! task, and a dedicated pump thread drains input capture the moment each
//! event leaves the platform hook. All of them meet here behind one mutex,
//! which is deliberately simple: the state is tiny and every critical
//! section is microseconds.

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

/// Where to move the hidden local pointer so it stays clear of the shared
/// edge, or `None` while it is already inland.
///
/// Used only while this machine owns the hardware and the visible cursor is on
/// the peer. Warping the invisible pointer a little inland keeps the OS able
/// to report the next movement; a cursor left pinned against the edge reports
/// nothing until it is dragged back.
fn recenter_anchor(screen: &Rect, edge: Edge, x: f64, y: f64) -> Option<(f64, f64)> {
    let near_edge = match edge {
        Edge::Left => x <= screen.left() + RECENTER_MARGIN,
        Edge::Right => x >= screen.right() - RECENTER_MARGIN,
        Edge::Top => y <= screen.top() + RECENTER_MARGIN,
        Edge::Bottom => y >= screen.bottom() - RECENTER_MARGIN,
    };
    near_edge.then(|| match edge {
        Edge::Left => (screen.left() + RECENTER_JUMP, y),
        Edge::Right => (screen.right() - RECENTER_JUMP, y),
        Edge::Top => (x, screen.top() + RECENTER_JUMP),
        Edge::Bottom => (x, screen.bottom() - RECENTER_JUMP),
    })
}

/// Whether an injected move carries the mirrored cursor back through the seam
/// far enough to hand control home.
///
/// Requires both the outward direction [`Rect::crossing`] checks for and a
/// real overshoot, so the pixel of jitter at a crossing does not count.
fn returns_home(bounds: &Rect, edge: Edge, nx: f64, ny: f64, dx: f64, dy: f64) -> bool {
    if bounds.crossing(edge, nx, ny, dx, dy).is_none() {
        return false;
    }
    let overshoot = match edge {
        Edge::Left => bounds.left() - nx,
        Edge::Right => nx - bounds.right(),
        Edge::Top => bounds.top() - ny,
        Edge::Bottom => ny - bounds.bottom(),
    };
    overshoot > RETURN_MARGIN
}

/// Minimum gap between two handoffs.
///
/// Without this the cursor ping-pongs across the seam: one event crosses,
/// the mirrored move crosses straight back, and control never settles.
const HANDOFF_DEBOUNCE: Duration = Duration::from_millis(250);

/// How close to the shared edge the pointer may get before it is nudged back
/// toward the middle.
///
/// The OS pins the cursor once it reaches a screen edge: outward mouse motion
/// then stops changing its position, so the machine that owns the hardware
/// would forward one last delta and then nothing. Nudging the (hidden) pointer
/// inland gives the next motion somewhere to go.
const RECENTER_MARGIN: f64 = 1.0;

/// How far inland the hidden pointer is pulled when it reaches the shared
/// edge.
///
/// Small on purpose. Every pixel of the pull is motion that gets relayed to
/// the peer, and the peer demands it back — past its own seam — before
/// control returns. A pull measured in hundreds of pixels, like the
/// screen-center warp this replaces, costs a whole screen of debt per push
/// and can exceed what a return push can repay before the local pointer pins
/// against the far edge, stranding control with the peer. A few pixels are
/// still enough room for the OS to keep reporting outward motion.
const RECENTER_JUMP: f64 = 8.0;

/// How far back through the seam the mirrored cursor must travel before
/// control is handed home.
///
/// A physical mouse often emits a pixel or two of jitter as it crosses an
/// edge. Without this margin that jitter reads as an immediate return and the
/// session snaps home before any input lands.
const RETURN_MARGIN: f64 = 6.0;

/// Scrollback length.
const LOG_CAPACITY: usize = 250;

/// Cloned handles share one state; the input pump thread holds a clone so it
/// can process events the moment they are captured.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    config: Config,
    session: Session,
    backend: Platform,
    /// The peer is driving this machine: mirrored input arrives over the link
    /// and drives the visible cursor.
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
    /// Set right after the pointer is recentered away from the shared edge.
    ///
    /// Input is pumped in bursts, so the queue can still hold mouse events
    /// captured before the warp; they carry the pinned edge position and would
    /// be relayed as a large outward jump. While this is set, moves that are
    /// still against the shared edge are dropped until the fresh (inland)
    /// position arrives.
    await_recenter: Option<(f64, f64)>,
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
            remote_active: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            return_edge: None,
            last_cursor: (0.0, 0.0),
            await_recenter: None,
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

        let state = AppState {
            inner: Arc::new(Mutex::new(inner)),
        };
        state.spawn_input_pump(events_rx)?;
        Ok(state)
    }

    /// Drain captured input on a dedicated thread.
    ///
    /// Input used to be pumped from the UI poll, which throttled motion to
    /// the poll rate: a pointer update crossed the link only when the window
    /// next asked for a snapshot, so the peer rendered a burst of deltas
    /// every tick and the visible cursor was steppy and laggy. Events arrive
    /// from the platform hook thread already; this thread forwards each one
    /// the moment it is captured.
    ///
    /// The channel closes when capture stops, or never opens, which ends the
    /// thread.
    fn spawn_input_pump(&self, events_rx: Receiver<CapturedEvent>) -> anyhow::Result<()> {
        let state = self.clone();
        std::thread::Builder::new()
            .name("mouser-input-pump".into())
            .spawn(move || {
                while let Ok(event) = events_rx.recv() {
                    let mut guard = state.inner.lock().expect("state mutex poisoned");
                    state.handle_event(&mut guard, event);
                }
            })
            .map_err(|e| anyhow::anyhow!("could not start the input pump: {e}"))?;
        Ok(())
    }

    pub fn handle(&self) -> AppHandle {
        AppHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    fn handle_event(&self, guard: &mut Inner, event: CapturedEvent) {
        let screen = screen_of(&guard.backend);

        match event {
            CapturedEvent::Move { x, y, dx, dy } => {
                // The peer drives this machine. Motion arrives over the link
                // and is replayed there, and a hardware move must not
                // disturb the mirrored position the return crossing is
                // judged on — not even update it — so drop it before anything
                // below touches that state. Injected motion never arrives
                // here: the hook filters it, which is what keeps the replay
                // from doubling.
                if guard.remote_active.load(Ordering::Relaxed) {
                    return;
                }

                // A recenter warps the pointer, but events captured before the
                // warp are still queued and carry the pinned edge position.
                // Drop those rather than relaying the jump to the middle; the
                // first fresh (inland) event ends the wait.
                if guard.await_recenter.is_some()
                    && recenter_anchor(&screen, guard.session.edge(), x, y).is_some()
                {
                    return;
                }
                guard.await_recenter = None;

                // Low-level hooks report absolute positions only, so the delta
                // is derived from the previous position.
                let (prev_x, prev_y) = guard.last_cursor;
                guard.last_cursor = (x, y);
                let (ex, ey) = if dx == 0.0 && dy == 0.0 {
                    (x - prev_x, y - prev_y)
                } else {
                    (dx, dy)
                };

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
                    self.recenter(guard, &screen);
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
        // that this machine is the one being driven. On Windows the hide is a
        // counted no-op — its display count only applies over windows of the
        // calling thread — so the parked pointer stays visible at the seam,
        // the usual software-KVM behavior.
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

    /// Nudge the hidden local pointer in from the shared edge while the peer
    /// owns it.
    ///
    /// The OS pins the cursor once it reaches a screen edge, and outward mouse
    /// motion then no longer changes its position. Without this the peer gets
    /// one last delta and then nothing, so the cursor stalls the moment it
    /// arrives. Moving the invisible pointer a little inland of the seam gives
    /// the next movement somewhere to go while keeping the debt the peer must
    /// repay to hand control back down to a small push.
    ///
    /// The warp is posted through the backend's injection path, which both
    /// platforms tag as synthetic, so it is never captured and forwarded as if
    /// the user had moved the mouse.
    fn recenter(&self, guard: &mut Inner, screen: &Rect) {
        let edge = guard.session.edge();
        let (x, y) = guard.last_cursor;
        let Some((ax, ay)) = recenter_anchor(screen, edge, x, y) else {
            return;
        };
        if let Err(e) = guard.backend.warp_cursor(ax, ay) {
            log(
                guard,
                Level::Warn,
                format!("could not recenter the pointer: {e}"),
            );
            return;
        }
        guard.last_cursor = (ax, ay);
        guard.await_recenter = Some((ax, ay));
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
    // Mirror to the console as well: the webview shows the same lines, but a
    // run captured from a terminal (or a headless test harness) can only be
    // read there.
    match level {
        Level::Info => tracing::info!("{text}"),
        Level::Warn => tracing::warn!("{text}"),
        Level::Error => tracing::error!("{text}"),
    }
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
        guard.await_recenter = None;
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
        guard.await_recenter = None;
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
            if returns_home(&bounds, edge, nx, ny, dx as f64, dy as f64) {
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
                    // A poisoned bridge lock is reported like a failed send
                    // rather than silently swallowed: a release that never
                    // leaves the machine strands control with the peer.
                    let result =
                        bridge.lock().map(|bridge| bridge.send(Message::ReleaseControl));
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            self.log(Level::Warn, format!("send dropped: {e}"));
                        }
                        Err(poisoned) => {
                            self.log(Level::Warn, format!("send dropped: {poisoned}"));
                        }
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

        // Move the pointer there as well, so the cursor the user watches and
        // the position the return crossing is judged on are the same one.
        // Without the warp the two sit apart by wherever the pointer happened
        // to rest, and control comes back before the visible cursor reaches
        // the seam.
        let _ = guard.backend.warp_cursor(x, y);
        guard.await_recenter = None;

        // The mirrored cursor is the one the user is now working with, so it
        // stays visible; nothing here hides it.
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
        guard.await_recenter = None;
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
    /// captured events directly. The receiver is handed back alongside the
    /// sender: the real pump thread owns it, and the test drains it through
    /// [`pump_all`] instead.
    fn state_at(
        edge: Edge,
    ) -> (
        AppState,
        std::sync::mpsc::Sender<CapturedEvent>,
        Receiver<CapturedEvent>,
    ) {
        let (tx, rx) = channel();
        let config = Config {
            peer_edge: edge,
            ..Config::default()
        };
        let inner = Inner {
            session: Session::new(edge),
            backend: Platform::new(),
            remote_active: AtomicBool::new(false),
            forwarding: AtomicBool::new(false),
            return_edge: None,
            last_cursor: (0.0, 0.0),
            await_recenter: None,
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
            rx,
        )
    }

    /// Process every queued event exactly the way the input pump thread does.
    fn pump_all(state: &AppState, rx: &Receiver<CapturedEvent>) {
        while let Ok(event) = rx.try_recv() {
            let mut guard = state.inner.lock().expect("state mutex poisoned");
            state.handle_event(&mut guard, event);
        }
    }

    #[test]
    fn edge_hit_without_a_peer_does_not_hand_off() {
        // Regression: this used to fire while disconnected, which hid the
        // cursor and queued input for a peer that did not exist.
        let (state, tx, rx) = state_at(Edge::Right);

        tx.send(CapturedEvent::Move {
            x: 1.0e9,
            y: 50.0,
            dx: 40.0,
            dy: 0.0,
        })
        .expect("the state holds the receiver");
        pump_all(&state, &rx);

        let snap = state.snapshot();
        assert!(!snap.remote_active, "must not hand off without a peer");
        assert_eq!(snap.status, "starting");
    }

    #[test]
    fn pointer_is_recentered_only_near_the_shared_edge() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 800.0);
        // Pinned against the right edge: pull it just inland, keeping the
        // height it crossed at.
        assert_eq!(
            recenter_anchor(&screen, Edge::Right, 1000.0, 400.0),
            Some((1000.0 - RECENTER_JUMP, 400.0))
        );
        // A little inland: leave it alone, motion is still being reported.
        assert_eq!(recenter_anchor(&screen, Edge::Right, 900.0, 400.0), None);
        // The same on the other edges.
        assert_eq!(
            recenter_anchor(&screen, Edge::Left, 0.0, 400.0),
            Some((RECENTER_JUMP, 400.0))
        );
        assert_eq!(
            recenter_anchor(&screen, Edge::Bottom, 400.0, 800.0),
            Some((400.0, 800.0 - RECENTER_JUMP))
        );
        assert_eq!(recenter_anchor(&screen, Edge::Top, 400.0, 40.0), None);
    }

    #[test]
    fn a_recenter_cycle_is_cheap_for_the_peer_to_give_back() {
        // One full cycle of pushing past the shared edge costs the pull plus
        // the return margin. The peer repays it with one small push, and a
        // return push has a whole screen of room to travel, so control can
        // never be stranded by the recentering itself.
        let screen = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let cycle_debt = RECENTER_JUMP + RETURN_MARGIN;
        let return_budget = screen.width - RECENTER_JUMP;
        assert!(
            cycle_debt < return_budget / 10.0,
            "one push cycle costs {cycle_debt}px but a return push only has \
             {return_budget}px of room; repeated pushes would strand control"
        );
    }

    #[test]
    fn a_pixel_of_jitter_does_not_return_control() {
        let bounds = Rect::new(0.0, 0.0, 1000.0, 800.0);
        // The mirrored cursor starts exactly on the seam ...
        let (x, y) = (1000.0, 400.0);
        // ... so a single outward pixel must not count as coming home.
        assert!(!returns_home(&bounds, Edge::Right, x + 1.0, y, 1.0, 0.0));
        // A deliberate push back does.
        assert!(returns_home(&bounds, Edge::Right, x + 40.0, y, 40.0, 0.0));
        // Moving further in is never a return.
        assert!(!returns_home(&bounds, Edge::Right, x - 40.0, y, -40.0, 0.0));
    }

    #[test]
    fn hardware_motion_while_driven_does_not_hijack_the_mirrored_cursor() {
        // Regression: a hardware mouse event on the driven machine used to
        // overwrite the mirrored cursor position before the driven-mode early
        // return. The return crossing was then judged against wherever the
        // local pointer happened to idle, so pushing back over the seam
        // released nothing and control stayed with the peer.
        let (state, tx, rx) = state_at(Edge::Left);
        let (bridge, out_rx) = Bridge::channel();
        state.install_bridge(Arc::new(Mutex::new(bridge)));
        let handle = state.handle();

        // Where the pointer started, so the test can put it back.
        let home = state
            .inner
            .lock()
            .expect("state mutex poisoned")
            .backend
            .cursor_position();

        // The peer takes control through our left seam, landing the mirrored
        // cursor at our right seam, and walks it inland.
        handle.take_control(Edge::Left, 0.5);
        handle.inject(InputEvent::Move { dx: -300, dy: 0 });
        assert!(state.snapshot().remote_active);

        // The local mouse twitches somewhere unrelated while the peer
        // drives. It must neither hijack the mirrored position nor be
        // relayed back to the peer.
        tx.send(CapturedEvent::Move {
            x: 513.0,
            y: 1065.0,
            dx: 2.0,
            dy: 1.0,
        })
        .expect("the state holds the receiver");
        pump_all(&state, &rx);
        assert!(
            out_rx.try_recv().is_err(),
            "a driven machine must not relay its own hardware input"
        );
        assert!(
            state.snapshot().remote_active,
            "the twitch must not end the drive"
        );

        // Push the mirrored cursor back over the seam. The 300px of walk
        // plus the return margin takes four 80px pushes; with the hijacked
        // position of the old code the same pushes got nowhere near it.
        let pushes = ((300.0 + RETURN_MARGIN + 1.0) / 80.0).ceil() as i32;
        for _ in 1..pushes {
            handle.inject(InputEvent::Move { dx: 80, dy: 0 });
        }
        assert!(
            state.snapshot().remote_active,
            "control must not return before the seam is crossed"
        );
        handle.inject(InputEvent::Move { dx: 80, dy: 0 });
        assert!(
            !state.snapshot().remote_active,
            "pushing back over the seam must return control"
        );
        assert!(matches!(
            out_rx.try_recv(),
            Ok(Message::ReleaseControl))
        );

        // Put the pointer back where the test found it.
        if let Some((x, y)) = home {
            let mut guard = state.inner.lock().expect("state mutex poisoned");
            let _ = guard.backend.warp_cursor(x, y);
        }
    }
}
