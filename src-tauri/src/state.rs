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

/// Where to move the hidden local pointer so it stays clear of all screen
/// edges, or `None` while it is comfortably within the central area.
///
/// Used while this machine owns the hardware and the visible cursor is on
/// the peer. Warping the invisible pointer back toward the center gives the
/// OS unlimited room to keep reporting motion in all directions without
/// ever stalling against an edge.
fn recenter_offset(screen: &Rect, x: f64, y: f64) -> Option<(f64, f64)> {
    let (cx, cy) = screen.center();
    let margin_x = (screen.width * 0.25).max(100.0);
    let margin_y = (screen.height * 0.25).max(100.0);
    if (x - cx).abs() > margin_x || (y - cy).abs() > margin_y {
        Some((cx, cy))
    } else {
        None
    }
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

// Keep seam hysteresis, but never accumulate invisible travel beyond other edges.
fn mirrored_position(bounds: &Rect, edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let margin = RETURN_MARGIN + 1.0;
    (
        x.clamp(
            bounds.left() - if edge == Edge::Left { margin } else { 0.0 },
            bounds.right() + if edge == Edge::Right { margin } else { -1.0 },
        ),
        y.clamp(
            bounds.top() - if edge == Edge::Top { margin } else { 0.0 },
            bounds.bottom() + if edge == Edge::Bottom { margin } else { -1.0 },
        ),
    )
}

/// Minimum gap between two handoffs.
///
/// Without this the cursor ping-pongs across the seam: one event crosses,
/// the mirrored move crosses straight back, and control never settles.
const HANDOFF_DEBOUNCE: Duration = Duration::from_millis(250);


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
    held: Vec<InputEvent>,
    last_handoff: Option<Instant>,
    bridge: Option<Arc<Bridge>>,
    log: Vec<LogLine>,
    next_seq: u64,
    status: String,
    peer_name: String,
    fingerprint: String,
    peer_addr: Option<String>,
    listening: bool,
    bind_addr: String,
    /// Held so the UI can hand over a new secret without a restart.
    secret_input: String,
    has_secret: bool,
    /// Set when [`mouser_input::InputBackend::start_capture`] failed, so the UI
    /// can tell the user what is missing instead of the process dying.
    capture_error: Option<String>,
}

impl Inner {
    fn release_held(&mut self) {
        for event in self.held.drain(..) {
            let _ = self.backend.inject(&event);
        }
    }
}

fn key_up(event: InputEvent) -> Option<InputEvent> {
    match event {
        InputEvent::Key { key, .. } => Some(InputEvent::Key {
            key,
            pressed: false,
            mods: Default::default(),
        }),
        InputEvent::Button { button, .. } => Some(InputEvent::Button {
            button,
            pressed: false,
        }),
        _ => None,
    }
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
            held: Vec::new(),
            last_handoff: None,
            bridge: None,
            log: Vec::new(),
            next_seq: 1,
            peer_name: String::new(),
            peer_addr: None,
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

                let (ex, ey) = if dx != 0.0 || dy != 0.0 {
                    (dx, dy)
                } else {
                    let (prev_x, prev_y) = guard.last_cursor;
                    (x - prev_x, y - prev_y)
                };
                guard.last_cursor = (x, y);

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

        mouser_input::set_forwarding(true);

        // Drive the state machine rather than assigning to it, so ownership
        // rules stay in one place.
        guard.session.on_event(
            SessionEvent::CursorMoved {
                x: guard.last_cursor.0,
                y: guard.last_cursor.1,
                dx: f64::from(u8::from(edge.is_vertical())),
                dy: f64::from(u8::from(!edge.is_vertical())),
            },
            |_, _, _, _, _| true,
        );

        // The visible cursor is now on the peer, so hide ours and keep
        // relaying. `remote_active` stays clear: that flag means the opposite,
        // that this machine is the one being driven. On Windows the hide is a
        // no-op — its display count only applies over windows of the
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

        let screen = screen_of(&guard.backend);
        let (cx, cy) = screen.center();
        let _ = guard.backend.warp_cursor(cx, cy);
        guard.last_cursor = (cx, cy);
    }

    /// Keep the hidden local pointer away from the screen boundaries while
    /// the peer owns it.
    ///
    /// The OS pins the cursor once it reaches a screen edge, and outward mouse
    /// motion then no longer changes its position. Warping the pointer back
    /// towards the center of the screen whenever it wanders outside the
    /// central deadzone gives the mouse unlimited room to move in all
    /// directions without ever stalling against an edge.
    fn recenter(&self, guard: &mut Inner, screen: &Rect) {
        let (x, y) = guard.last_cursor;
        let Some((cx, cy)) = recenter_offset(screen, x, y) else {
            return;
        };
        if let Err(e) = guard.backend.warp_cursor(cx, cy) {
            log(
                guard,
                Level::Warn,
                format!("could not recenter the pointer: {e}"),
            );
            return;
        }
        guard.last_cursor = (cx, cy);
    }

    /// Queue a message, dropping it if the peer is not keeping up.
    fn send(&self, guard: &mut Inner, msg: Message) {
        let Some(bridge) = guard.bridge.clone() else {
            return;
        };
        if let Err(e) = bridge.send(msg) {
            log(guard, Level::Warn, format!("send dropped: {e}"));
        }
    }

    pub fn install_bridge(&self, bridge: Arc<Bridge>) {
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
        let mut guard = self.inner.lock().expect("state mutex poisoned");
        // Never leave the pointer hidden: hiding is a counted display state,
        // and a graceful quit should always undo it.
        let _ = guard.backend.show_cursor();
        guard.release_held();
        mouser_input::set_forwarding(false);
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
        guard.status = "connected".into();
        guard.remote_active.store(false, Ordering::Relaxed);
        guard.forwarding.store(false, Ordering::Relaxed);
        mouser_input::set_forwarding(false);
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
        guard.release_held();
        guard.bridge = None;
        guard.peer_addr = None;
        guard.status = "disconnected".into();
        guard.remote_active.store(false, Ordering::Relaxed);
        guard.forwarding.store(false, Ordering::Relaxed);
        mouser_input::set_forwarding(false);
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
            let edge = guard.return_edge.unwrap_or(guard.session.edge());
            guard.last_cursor = mirrored_position(&bounds, edge, nx, ny);

            if returns_home(&bounds, edge, nx, ny, dx as f64, dy as f64) {
                guard.release_held();
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
                    if let Err(e) = bridge.send(Message::ReleaseControl) {
                        self.log(Level::Warn, format!("send dropped: {e}"));
                    }
                }
                return;
            }
            let (x, y) = guard.last_cursor;
            if let Err(e) = guard.backend.warp_cursor(
                x.clamp(bounds.left(), bounds.right() - 1.0),
                y.clamp(bounds.top(), bounds.bottom() - 1.0),
            ) {
                log(&mut guard, Level::Warn, format!("move failed: {e}"));
            }
            return;
        }

        if let Some(release) = key_up(event) {
            guard.held.retain(|held| *held != release);
            if matches!(
                event,
                InputEvent::Key { pressed: true, .. } | InputEvent::Button { pressed: true, .. }
            ) {
                guard.held.push(release);
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
        mouser_input::set_forwarding(false);
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
        let (start_x, start_y) = match entry {
            Edge::Left => (bounds.left() + 2.0, y),
            Edge::Right => (bounds.right() - 3.0, y),
            Edge::Top => (x, bounds.top() + 2.0),
            Edge::Bottom => (x, bounds.bottom() - 3.0),
        };
        guard.last_cursor = (start_x, start_y);
        guard.return_edge = Some(entry);

        // Move the pointer there as well, so the cursor the user watches and
        // the position the return crossing is judged on are the same one.
        let _ = guard.backend.warp_cursor(
            start_x.clamp(bounds.left(), bounds.right() - 1.0),
            start_y.clamp(bounds.top(), bounds.bottom() - 1.0),
        );

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
        guard.release_held();
        guard.last_handoff = Some(Instant::now());
        let remote = guard.remote_active.swap(false, Ordering::Relaxed);
        let forwarding = guard.forwarding.swap(false, Ordering::Relaxed);
        mouser_input::set_forwarding(false);
        if !remote && !forwarding {
            return;
        }
        guard.return_edge = None;
        let _ = guard.backend.show_cursor();
        guard.status = "connected".into();

        // Warp the cursor to just inside the edge where it returned
        let bounds = screen_of(&guard.backend);
        let edge = guard.session.edge();
        let (rx, ry) = guard.last_cursor;
        let return_pos = match edge {
            Edge::Left => (bounds.left() + 2.0, ry.clamp(bounds.top(), bounds.bottom() - 1.0)),
            Edge::Right => (bounds.right() - 3.0, ry.clamp(bounds.top(), bounds.bottom() - 1.0)),
            Edge::Top => (rx.clamp(bounds.left(), bounds.right() - 1.0), bounds.top() + 2.0),
            Edge::Bottom => (rx.clamp(bounds.left(), bounds.right() - 1.0), bounds.bottom() - 3.0),
        };
        let _ = guard.backend.warp_cursor(return_pos.0, return_pos.1);
        guard.last_cursor = return_pos;

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
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Build an app state without starting capture, so a test can feed it
    /// captured events directly. The receiver is handed back alongside the
    /// sender: the real pump thread owns it, and the test drains it through
    /// [`pump_all`] instead.
    pub(crate) fn state_at(
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
            held: Vec::new(),
            last_handoff: None,
            bridge: None,
            log: Vec::new(),
            next_seq: 1,
            status: "starting".into(),
            peer_name: String::new(),
            fingerprint: String::new(),
            peer_addr: None,
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
    fn pointer_is_recentered_when_wandering_from_center() {
        let screen = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let center = (500.0, 400.0);
        // Near center: leave it alone.
        assert_eq!(recenter_offset(&screen, 500.0, 400.0), None);
        assert_eq!(recenter_offset(&screen, 550.0, 450.0), None);

        // Wandering beyond margin (25% = 250px on X, 200px on Y): pull back to center.
        assert_eq!(recenter_offset(&screen, 100.0, 400.0), Some(center));
        assert_eq!(recenter_offset(&screen, 900.0, 400.0), Some(center));
        assert_eq!(recenter_offset(&screen, 500.0, 100.0), Some(center));
        assert_eq!(recenter_offset(&screen, 500.0, 700.0), Some(center));
    }

    #[test]
    fn forwarding_relays_continuous_moves_without_stalling() {
        let (state, tx, rx) = state_at(Edge::Left);
        let (bridge, mut out_rx) = Bridge::channel();
        state.install_bridge(Arc::new(bridge));
        state.handle().link_up(
            PeerHello {
                protocol_version: 1,
                device_name: "peer".into(),
                secret_fingerprint: "abc".into(),
                screen_width: 1920,
                screen_height: 1080,
                edge: Edge::Right,
            },
            "127.0.0.1:47583".into(),
        );

        // Hand off across the left seam (screen is 0..1000 x 0..800)
        tx.send(CapturedEvent::Move {
            x: 0.0,
            y: 400.0,
            dx: -10.0,
            dy: 0.0,
        })
        .unwrap();
        pump_all(&state, &rx);

        assert!(matches!(out_rx.try_recv(), Ok(Message::TakeControl { .. })));
        assert!(state.snapshot().remote_active);

        // Subsequent pushes in any direction must continue relaying without being dropped
        for i in 1..=5 {
            tx.send(CapturedEvent::Move {
                x: 500.0 - (i as f64 * 20.0),
                y: 400.0,
                dx: -20.0,
                dy: 0.0,
            })
            .unwrap();
            pump_all(&state, &rx);

            assert!(
                matches!(out_rx.try_recv(), Ok(Message::Input(events)) if matches!(events[0], InputEvent::Move { dx: -20, dy: 0 })),
                "continuous move {i} must be forwarded"
            );
        }
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
        let (bridge, mut out_rx) = Bridge::channel();
        state.install_bridge(Arc::new(bridge));
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
        assert!(matches!(out_rx.try_recv(), Ok(Message::ReleaseControl)));

        // Put the pointer back where the test found it.
        if let Some((x, y)) = home {
            let guard = state.inner.lock().expect("state mutex poisoned");
            let _ = guard.backend.warp_cursor(x, y);
        }
    }
    #[test]
    fn offscreen_motion_does_not_build_up_before_returning() {
        let bounds = Rect::new(0.0, 0.0, 1000.0, 800.0);
        let (x, y) = mirrored_position(&bounds, Edge::Left, 100_000.0, -100_000.0);
        assert_eq!((x, y), (999.0, 0.0));
        assert_eq!(
            mirrored_position(&bounds, Edge::Left, x - 10.0, y + 10.0),
            (989.0, 10.0)
        );
        // Small deliberate pushes still accumulate across the return seam.
        let mut x = 0.0;
        for _ in 0..4 {
            x = mirrored_position(&bounds, Edge::Left, x - 2.0, 400.0).0;
        }
        assert!(returns_home(&bounds, Edge::Left, x, 400.0, -2.0, 0.0));
    }

    #[test]
    fn disconnect_drops_the_old_queue_and_releases_held_input() {
        let (state, _, _) = state_at(Edge::Right);
        let (bridge, mut rx) = Bridge::channel();
        state.install_bridge(Arc::new(bridge));
        {
            let mut guard = state.inner.lock().unwrap();
            // Unknown key: exercises bookkeeping without typing into the desktop.
            guard.held.push(
                key_up(InputEvent::Key {
                    key: mouser_core::protocol::HidKey(255),
                    pressed: true,
                    mods: Default::default(),
                })
                .unwrap(),
            );
        }
        state.handle().link_down("test disconnect");
        let guard = state.inner.lock().unwrap();
        assert!(guard.bridge.is_none());
        assert!(guard.held.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
