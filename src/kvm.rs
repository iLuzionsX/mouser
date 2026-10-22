//! Who owns the cursor: here (Local), here but driving the peer
//! (Forwarding), or driven by the peer (Remote). Also when it crosses, where
//! it lands, and what must be undone when it comes back or the link drops.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use crate::wire::Msg;

/// Distance inland from the seam where the parked pointer sits.
const EDGE_ANCHOR: f64 = 8.0;
/// Maximum drift inland from the seam before snapping back.
const EDGE_MAX_DRIFT: f64 = 16.0;
/// How far back through the seam the mirrored cursor must travel before
/// control goes home, so a pixel of jitter at the crossing does not count.
const RETURN_MARGIN: f64 = 6.0;
/// Minimum gap between handoffs, so the cursor cannot ping-pong.
const HANDOFF_DEBOUNCE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// In discriminant order, which is also the wire encoding.
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    pub fn parse(s: &str) -> Option<Edge> {
        Edge::ALL.into_iter().find(|e| format!("{e:?}").eq_ignore_ascii_case(s))
    }

    fn opposite(self) -> Edge {
        Edge::ALL[self as usize ^ 1]
    }

    fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// The local screen; `right` and `bottom` are exclusive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { left: x, top: y, right: x + w, bottom: y + h }
    }

    fn fraction_at(&self, edge: Edge, x: f64, y: f64) -> f64 {
        let t = if edge.is_vertical() {
            (y - self.top) / (self.bottom - self.top)
        } else {
            (x - self.left) / (self.right - self.left)
        };
        if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 }
    }

    /// Whether a cursor at `(x, y)` moving by `(dx, dy)` is pushing out
    /// through `edge`, and if so how far along it.
    fn crossing(&self, edge: Edge, x: f64, y: f64, dx: f64, dy: f64) -> Option<f64> {
        let out = match edge {
            Edge::Left => dx < 0.0 && x <= self.left,
            Edge::Right => dx > 0.0 && x >= self.right - 1.0,
            Edge::Top => dy < 0.0 && y <= self.top,
            Edge::Bottom => dy > 0.0 && y >= self.bottom - 1.0,
        };
        out.then(|| self.fraction_at(edge, x, y))
    }

    fn clamp(&self, x: f64, y: f64) -> (f64, f64) {
        (x.clamp(self.left, self.right - 1.0), y.clamp(self.top, self.bottom - 1.0))
    }
}

/// The parking spot for the hidden pointer nearest `(x, y)`: `EDGE_ANCHOR`
/// inland from the seam, and at least that far from either end of it.
fn seam_anchor(s: &Rect, edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let along_x = x.clamp(s.left + EDGE_ANCHOR, s.right - EDGE_ANCHOR);
    let along_y = y.clamp(s.top + EDGE_ANCHOR, s.bottom - EDGE_ANCHOR);
    match edge {
        Edge::Left => (s.left + EDGE_ANCHOR, along_y),
        Edge::Right => (s.right - EDGE_ANCHOR, along_y),
        Edge::Top => (along_x, s.top + EDGE_ANCHOR),
        Edge::Bottom => (along_x, s.bottom - EDGE_ANCHOR),
    }
}

/// Where to warp the hidden pointer once it pins against a screen edge or
/// drifts out of the strip along the seam, or `None` while it has headroom
/// in every direction.
fn recenter_seam(s: &Rect, edge: Edge, x: f64, y: f64) -> Option<(f64, f64)> {
    let inland = match edge {
        Edge::Left => x - s.left,
        Edge::Right => s.right - x,
        Edge::Top => y - s.top,
        Edge::Bottom => s.bottom - y,
    };
    let (along, start, end) =
        if edge.is_vertical() { (y, s.top, s.bottom) } else { (x, s.left, s.right) };
    let pinned =
        inland <= 1.0 || inland >= EDGE_MAX_DRIFT || along <= start + 1.0 || along >= end - 1.0;
    pinned.then(|| seam_anchor(s, edge, x, y))
}

/// Just inside `edge`, at `(x, y)`'s position along it: where a cursor
/// arriving or returning through that edge lands.
fn landing(s: &Rect, edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let (x, y) = s.clamp(x, y);
    match edge {
        Edge::Left => (s.left + 2.0, y),
        Edge::Right => (s.right - 3.0, y),
        Edge::Top => (x, s.top + 2.0),
        Edge::Bottom => (x, s.bottom - 3.0),
    }
}

/// Whether a mirrored move carries the cursor back out through `edge` far
/// enough to hand control home.
fn returns_home(s: &Rect, edge: Edge, nx: f64, ny: f64, dx: f64, dy: f64) -> bool {
    let overshoot = match edge {
        Edge::Left => s.left - nx,
        Edge::Right => nx - s.right,
        Edge::Top => s.top - ny,
        Edge::Bottom => ny - s.bottom,
    };
    s.crossing(edge, nx, ny, dx, dy).is_some() && overshoot > RETURN_MARGIN
}

/// Keep seam hysteresis, but never accumulate invisible travel past the
/// other edges.
fn mirrored(s: &Rect, edge: Edge, x: f64, y: f64) -> (f64, f64) {
    let m = RETURN_MARGIN + 1.0;
    let slack = |e: Edge| if e == edge { m } else { 0.0 };
    (
        x.clamp(s.left - slack(Edge::Left), s.right - 1.0 + slack(Edge::Right)),
        y.clamp(s.top - slack(Edge::Top), s.bottom - 1.0 + slack(Edge::Bottom)),
    )
}

/// What the capture hooks need to decide synchronously, without the state
/// machine's lock: whether input belongs to the peer right now.
pub struct Claims {
    forwarding: AtomicBool,
    /// One bit per HID key (0..256) and mouse button (256..261) whose press
    /// went to the peer, so its release goes there too, and only then.
    pressed: [AtomicU64; 5],
}

pub static CLAIMS: Claims = Claims::new();

impl Claims {
    pub const fn new() -> Claims {
        Claims { forwarding: AtomicBool::new(false), pressed: [const { AtomicU64::new(0) }; 5] }
    }

    pub fn forwarding(&self) -> bool {
        self.forwarding.load(Relaxed)
    }

    /// Whether a captured button, key or wheel event belongs to the peer, in
    /// which case the hook swallows it locally. A release follows its press,
    /// so a key held across a handoff is released where it was pressed.
    pub fn claim(&self, msg: Msg) -> bool {
        let (bit, down) = match msg {
            Msg::Key(k, down) => (usize::from(k), down),
            Msg::Button(b, down) => (256 + usize::from(b), down),
            _ => return self.forwarding(),
        };
        let (word, mask) = (&self.pressed[bit / 64], 1u64 << (bit % 64));
        if !down {
            return word.fetch_and(!mask, Relaxed) & mask != 0;
        }
        let ours = self.forwarding();
        if ours {
            word.fetch_or(mask, Relaxed);
        }
        ours
    }
}

/// An event from the local capture hook.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Captured {
    /// Absolute position for edge detection, delta for the peer.
    Move { x: f64, y: f64, dx: f64, dy: f64 },
    /// A button, key or wheel event the hook claimed for the peer.
    Input(Msg),
}

/// The platform side: screen geometry and local injection.
pub trait Io {
    /// Whether local motion keeps moving the hidden pointer while
    /// forwarding (Windows), so it must be parked by warping it back.
    const PARK: bool;
    fn screen(&self) -> Rect;
    fn inject(&mut self, msg: Msg);
    fn warp(&mut self, x: f64, y: f64);
    fn hide(&mut self);
    fn show(&mut self);
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Local,
    Forwarding,
    /// Driven by the peer; the cursor entered through this edge.
    Remote(Edge),
}

pub struct Kvm<I: Io> {
    io: I,
    claims: &'static Claims,
    edge: Edge,
    /// Breaks a simultaneous crossing from both sides: the host keeps control.
    wins_ties: bool,
    connected: bool,
    mode: Mode,
    cursor: (f64, f64),
    /// Releases for every key and button injected down, replayed whenever
    /// the peer stops driving.
    held: Vec<Msg>,
    last_handoff: Option<Instant>,
}

impl<I: Io> Kvm<I> {
    pub fn new(io: I, claims: &'static Claims, edge: Edge, wins_ties: bool) -> Self {
        Kvm {
            io,
            claims,
            edge,
            wins_ties,
            connected: false,
            mode: Mode::Local,
            cursor: (0.0, 0.0),
            held: Vec::new(),
            last_handoff: None,
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.claims.forwarding.store(mode == Mode::Forwarding, Relaxed);
    }

    /// The link came up or went down: either way, nothing stays held,
    /// hidden or forwarded.
    pub fn link(&mut self, up: bool) {
        self.release_held();
        self.set_mode(Mode::Local);
        self.io.show();
        self.connected = up;
    }

    /// The user moved the other screen. If we were driving the peer, hand
    /// control back first so neither side is left stuck.
    pub fn set_edge(&mut self, edge: Edge) -> Option<Msg> {
        self.edge = edge;
        if self.mode != Mode::Forwarding {
            return None;
        }
        self.set_mode(Mode::Local);
        self.io.show();
        Some(Msg::Release)
    }

    fn release_held(&mut self) {
        for up in std::mem::take(&mut self.held) {
            self.io.inject(up);
        }
    }

    /// Handle captured input; returns what to send to the peer.
    pub fn local(&mut self, ev: Captured) -> Option<Msg> {
        let (x, y, dx, dy) = match ev {
            Captured::Input(msg) => return (self.mode == Mode::Forwarding).then_some(msg),
            Captured::Move { x, y, dx, dy } => (x, y, dx, dy),
        };
        let forwarding = match self.mode {
            // The peer drives this machine; local motion must not disturb
            // the mirrored position the return is judged on.
            Mode::Remote(_) => return None,
            mode => mode == Mode::Forwarding,
        };
        let (ex, ey) = if dx != 0.0 || dy != 0.0 {
            (dx, dy)
        } else if forwarding {
            (0.0, 0.0)
        } else {
            (x - self.cursor.0, y - self.cursor.1)
        };
        self.cursor = (x, y);
        let s = self.io.screen();

        if forwarding {
            if I::PARK {
                if let Some((ax, ay)) = recenter_seam(&s, self.edge, x, y) {
                    self.io.warp(ax, ay);
                    self.cursor = (ax, ay);
                }
            }
            return (ex != 0.0 || ey != 0.0)
                .then(|| Msg::Move(ex.round() as i16, ey.round() as i16));
        }

        let at = s.crossing(self.edge, x, y, ex, ey)?;
        if !self.connected || !self.debounce() {
            return None;
        }
        self.set_mode(Mode::Forwarding);
        self.io.hide();
        if I::PARK {
            let (px, py) = seam_anchor(&s, self.edge, x, y);
            self.io.warp(px, py);
            self.cursor = (px, py);
        }
        log!("-> peer: pushed off the {:?} edge at ({x:.0}, {y:.0})", self.edge);
        Some(Msg::Take(self.edge, (at * 65535.0).round() as u16))
    }

    /// Handle a message from the peer; returns a reply, if any.
    pub fn peer(&mut self, msg: Msg) -> Option<Msg> {
        match (msg, self.mode) {
            (Msg::Move(dx, dy), Mode::Remote(entry)) => return self.remote_move(entry, dx, dy),
            (Msg::Button(..) | Msg::Key(..) | Msg::Scroll(..), Mode::Remote(_)) => {
                let up = match msg {
                    Msg::Button(b, _) => Some(Msg::Button(b, false)),
                    Msg::Key(k, _) => Some(Msg::Key(k, false)),
                    _ => None,
                };
                if let Some(up) = up {
                    self.held.retain(|h| *h != up);
                    if up != msg {
                        self.held.push(up);
                    }
                }
                self.io.inject(msg);
            }
            (Msg::Take(edge, at), _) => self.take(edge, at),
            (Msg::Release, _) => self.released(),
            _ => {}
        }
        None
    }

    fn remote_move(&mut self, entry: Edge, dx: i16, dy: i16) -> Option<Msg> {
        let s = self.io.screen();
        let (dx, dy) = (f64::from(dx), f64::from(dy));
        let (nx, ny) = (self.cursor.0 + dx, self.cursor.1 + dy);
        self.cursor = mirrored(&s, entry, nx, ny);
        if returns_home(&s, entry, nx, ny, dx, dy) {
            self.release_held();
            self.set_mode(Mode::Local);
            log!("-> local control: cursor pushed back over the {entry:?} edge");
            return Some(Msg::Release);
        }
        let (x, y) = s.clamp(self.cursor.0, self.cursor.1);
        self.io.warp(x, y);
        None
    }

    /// The peer pushed its cursor over to us.
    fn take(&mut self, edge: Edge, at: u16) {
        match self.mode {
            Mode::Remote(_) => return,
            Mode::Forwarding if self.wins_ties => return,
            _ => {}
        }
        self.io.show();
        let s = self.io.screen();
        let entry = edge.opposite();
        // Only the position along the edge matters; landing() sets the rest.
        let t = f64::from(at) / 65535.0;
        let (x, y) = (s.left + (s.right - s.left) * t, s.top + (s.bottom - s.top) * t);
        self.cursor = landing(&s, entry, x, y);
        self.set_mode(Mode::Remote(entry));
        self.io.warp(self.cursor.0, self.cursor.1);
        log!("<- peer took control through the {entry:?} edge");
    }

    /// The peer handed control back.
    fn released(&mut self) {
        self.release_held();
        self.last_handoff = Some(Instant::now());
        if self.mode == Mode::Local {
            return;
        }
        self.set_mode(Mode::Local);
        self.io.show();
        let s = self.io.screen();
        self.cursor = landing(&s, self.edge, self.cursor.0, self.cursor.1);
        self.io.warp(self.cursor.0, self.cursor.1);
        log!("-> local control: peer released");
    }

    /// Consume the debounce window. True when a handoff may proceed.
    fn debounce(&mut self) -> bool {
        let now = Instant::now();
        if self.last_handoff.is_some_and(|t| now - t < HANDOFF_DEBOUNCE) {
            return false;
        }
        self.last_handoff = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { left: 0.0, top: 0.0, right: 1920.0, bottom: 1080.0 };

    #[derive(Default)]
    struct Fake {
        injected: Vec<Msg>,
        warps: Vec<(f64, f64)>,
        hidden: bool,
    }

    impl Io for Fake {
        const PARK: bool = true;
        fn screen(&self) -> Rect {
            SCREEN
        }
        fn inject(&mut self, msg: Msg) {
            self.injected.push(msg);
        }
        fn warp(&mut self, x: f64, y: f64) {
            self.warps.push((x, y));
        }
        fn hide(&mut self) {
            self.hidden = true;
        }
        fn show(&mut self) {
            self.hidden = false;
        }
    }

    fn kvm(edge: Edge, host: bool) -> Kvm<Fake> {
        let claims = Box::leak(Box::new(Claims::new()));
        let mut k = Kvm::new(Fake::default(), claims, edge, host);
        k.link(true);
        k
    }

    fn mv(x: f64, y: f64, dx: f64, dy: f64) -> Captured {
        Captured::Move { x, y, dx, dy }
    }

    #[test]
    fn pushing_off_the_seam_hands_over_and_parks() {
        let mut k = kvm(Edge::Right, true);
        assert_eq!(k.local(mv(1900.0, 540.0, 5.0, 0.0)), None);
        assert_eq!(k.local(mv(1919.0, 540.0, 5.0, 0.0)), Some(Msg::Take(Edge::Right, 32768)));
        assert!(k.io.hidden && k.claims.forwarding());
        assert_eq!(k.io.warps, [(1912.0, 540.0)]);
        // Motion is relayed, and a pointer pinned at the edge is re-parked.
        assert_eq!(k.local(mv(1919.0, 540.0, 7.0, -2.0)), Some(Msg::Move(7, -2)));
        assert_eq!(k.io.warps.last(), Some(&(1912.0, 540.0)));
        assert_eq!(k.local(Captured::Input(Msg::Key(4, true))), Some(Msg::Key(4, true)));
    }

    #[test]
    fn moving_the_screen_mid_handoff_hands_control_back() {
        let mut k = kvm(Edge::Right, true);
        assert_eq!(k.set_edge(Edge::Top), None);
        assert!(k.local(mv(960.0, 0.0, 0.0, -5.0)).is_some());
        assert_eq!(k.set_edge(Edge::Bottom), Some(Msg::Release));
        assert!(!k.io.hidden && !k.claims.forwarding());
    }

    #[test]
    fn no_handoff_without_a_peer_or_inside_the_screen() {
        let mut k = kvm(Edge::Left, true);
        assert_eq!(k.local(mv(0.0, 10.0, 3.0, 0.0)), None); // moving inland
        k.link(false);
        assert_eq!(k.local(mv(0.0, 10.0, -3.0, 0.0)), None);
        assert!(!k.io.hidden);
    }

    #[test]
    fn mirrored_cursor_returns_through_its_entry_edge() {
        // The peer left through its right edge, so we were entered from the left.
        let mut k = kvm(Edge::Bottom, false);
        k.peer(Msg::Take(Edge::Right, 0));
        assert_eq!(k.io.warps, [(2.0, 0.0)]);
        k.peer(Msg::Button(0, true));
        k.peer(Msg::Key(0xE1, true));
        k.peer(Msg::Key(0xE1, false));
        assert_eq!(k.peer(Msg::Move(-5, 0)), None, "inside the return margin");
        assert_eq!(k.peer(Msg::Move(-5, 0)), Some(Msg::Release));
        // The drag still held is released; the key already let go is not.
        assert_eq!(k.io.injected.last(), Some(&Msg::Button(0, false)));
        assert_eq!(k.io.injected.len(), 4);
        assert_eq!(k.peer(Msg::Move(5, 0)), None, "late motion is ignored");
    }

    #[test]
    fn released_returns_the_cursor_to_our_seam() {
        let mut k = kvm(Edge::Top, true);
        k.local(mv(300.0, 0.0, 0.0, -4.0));
        k.peer(Msg::Release);
        assert!(!k.io.hidden && !k.claims.forwarding());
        assert_eq!(k.io.warps.last(), Some(&(300.0, 2.0)));
    }

    #[test]
    fn link_loss_releases_everything_held() {
        let mut k = kvm(Edge::Left, false);
        k.peer(Msg::Take(Edge::Right, 100));
        k.peer(Msg::Key(0xE0, true));
        k.peer(Msg::Button(3, true));
        k.link(false);
        assert_eq!(k.io.injected[2..], [Msg::Key(0xE0, false), Msg::Button(3, false)]);
        k.peer(Msg::Key(4, true));
        assert_eq!(k.io.injected.len(), 4, "input after the link is gone is dropped");
    }

    #[test]
    fn simultaneous_crossing_leaves_the_host_in_control() {
        let mut host = kvm(Edge::Right, true);
        let mut client = kvm(Edge::Left, false);
        host.local(mv(1919.0, 500.0, 5.0, 0.0));
        client.local(mv(0.0, 500.0, -5.0, 0.0));
        host.peer(Msg::Take(Edge::Left, 30000));
        client.peer(Msg::Take(Edge::Right, 30000));
        assert!(host.claims.forwarding() && host.io.hidden);
        assert!(!client.claims.forwarding() && !client.io.hidden);
        assert_eq!(client.mode, Mode::Remote(Edge::Left));
    }

    #[test]
    fn releases_follow_their_presses() {
        let c = Claims::new();
        assert!(!c.claim(Msg::Key(4, true)), "not forwarding: stays local");
        c.forwarding.store(true, Relaxed);
        assert!(!c.claim(Msg::Key(4, false)), "pressed locally, so released locally");
        assert!(c.claim(Msg::Button(4, true)));
        assert!(c.claim(Msg::Scroll(0, 120)));
        c.forwarding.store(false, Relaxed);
        assert!(c.claim(Msg::Button(4, false)), "pressed on the peer, released there");
        assert!(!c.claim(Msg::Button(4, false)));
    }

    #[test]
    fn parking_keeps_headroom_on_every_edge() {
        assert_eq!(recenter_seam(&SCREEN, Edge::Bottom, 500.0, 1072.0), None);
        assert_eq!(recenter_seam(&SCREEN, Edge::Bottom, 500.0, 1079.0), Some((500.0, 1072.0)));
        assert_eq!(recenter_seam(&SCREEN, Edge::Bottom, 500.0, 1060.0), Some((500.0, 1072.0)));
        assert_eq!(recenter_seam(&SCREEN, Edge::Bottom, 0.0, 1072.0), Some((8.0, 1072.0)));
        assert_eq!(recenter_seam(&SCREEN, Edge::Left, 0.0, 1079.0), Some((8.0, 1072.0)));
        assert_eq!(seam_anchor(&SCREEN, Edge::Top, 1919.0, 300.0), (1912.0, 8.0));
    }

    #[test]
    fn edges_parse_and_mirror() {
        assert_eq!(Edge::parse("Bottom"), Some(Edge::Bottom));
        assert_eq!(Edge::parse("up"), None);
        for e in Edge::ALL {
            assert_ne!(e.opposite(), e);
            assert_eq!(e.opposite().opposite(), e);
        }
    }
}
