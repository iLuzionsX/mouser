//! Geometry helpers describing how two screens sit relative to each other.
//!
//! mouser models a two-machine setup as a *virtual desktop*: the local screen
//! plus the remote screen, placed on one of the local screen's four edges.
//! Pushing the cursor past that shared edge hands control to the peer.

use serde::{Deserialize, Serialize};

/// Which side of the local screen the remote screen is attached to.
///
/// Note this is expressed from the local machine's point of view: `Right`
/// means "the other computer's screen is to the right of this one".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    /// All edges, ordered for stable UI rendering.
    pub const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    /// The edge the cursor leaves through when handing control back.
    ///
    /// Handing off happens at [`Edge::shared`]; control returns when the
    /// remote cursor is pushed through the mirrored edge, which is the
    /// opposite side.
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }

    /// True when the shared edge is vertical, so only the Y axis is shared.
    pub fn is_vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    /// Compact single-glyph label used by the terminal-style UI.
    pub fn glyph(self) -> &'static str {
        match self {
            Edge::Left => "<",
            Edge::Right => ">",
            Edge::Top => "^",
            Edge::Bottom => "v",
        }
    }
}

impl std::fmt::Display for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An axis-aligned rectangle in virtual-desktop pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn left(&self) -> f64 {
        self.x
    }

    pub fn right(&self) -> f64 {
        self.x + self.width
    }

    pub fn top(&self) -> f64 {
        self.y
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.height
    }

    pub fn center(&self) -> (f64, f64) {
        (self.x + self.width / 2.0, self.y + self.height / 2.0)
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.left() && x <= self.right() && y >= self.top() && y <= self.bottom()
    }

    /// The point on `edge` of this rect closest to `(x, y)`.
    ///
    /// Used to decide where a cursor lands when it is clamped against a
    /// screen boundary.
    pub fn clamp_to_edge(&self, edge: Edge, x: f64, y: f64) -> (f64, f64) {
        let (cx, cy) = self.center();
        let x = x.clamp(self.left(), self.right());
        let y = y.clamp(self.top(), self.bottom());
        match edge {
            Edge::Left => (self.left(), if cx < self.right() { y } else { cy }),
            Edge::Right => (self.right(), y),
            Edge::Top => (x, self.top()),
            Edge::Bottom => (x, self.bottom()),
        }
    }

    /// Map a normalized position along `edge` onto a concrete point.
    ///
    /// `t` runs 0..1 along the edge: for vertical edges it is the fraction
    /// down the screen, for horizontal edges the fraction across. This is how
    /// a cursor keeps its relative position when it crosses between screens
    /// of different sizes or resolutions.
    pub fn point_at_fraction(&self, edge: Edge, t: f64) -> (f64, f64) {
        let t = t.clamp(0.0, 1.0);
        match edge {
            Edge::Left => (self.left(), self.top() + self.height * t),
            Edge::Right => (self.right(), self.top() + self.height * t),
            Edge::Top => (self.left() + self.width * t, self.top()),
            Edge::Bottom => (self.left() + self.width * t, self.bottom()),
        }
    }

    /// Inverse of [`Rect::point_at_fraction`].
    pub fn fraction_at(&self, edge: Edge, x: f64, y: f64) -> f64 {
        let t = if edge.is_vertical() {
            if self.height <= 0.0 {
                0.0
            } else {
                (y - self.top()) / self.height
            }
        } else if self.width <= 0.0 {
            0.0
        } else {
            (x - self.left()) / self.width
        };
        t.clamp(0.0, 1.0)
    }

    /// Union of the local and remote rects.
    pub fn union(&self, other: &Rect) -> Rect {
        let x = self.left().min(other.left());
        let y = self.top().min(other.top());
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Rect::new(x, y, right - x, bottom - y)
    }

    /// The combined bounding box implied by attaching `other` at `edge`.
    pub fn joined(&self, edge: Edge, other: &Rect) -> Rect {
        match edge {
            Edge::Left => Rect::new(
                other.left(),
                self.top(),
                other.width + self.width,
                self.height,
            ),
            Edge::Right => Rect::new(
                self.left(),
                self.top(),
                self.width + other.width,
                self.height,
            ),
            Edge::Top => Rect::new(
                self.left(),
                other.top(),
                self.width,
                other.height + self.height,
            ),
            Edge::Bottom => Rect::new(
                self.left(),
                self.top(),
                self.width,
                self.height + other.height,
            ),
        }
    }
}

/// Errors raised while configuring the virtual desktop.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GeometryError {
    #[error("screen rectangle must have a positive width and height")]
    EmptyScreen,
    #[error("remote screen would overlap the local screen")]
    OverlappingScreens,
}

/// The outcome of testing a cursor position against the shared edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScreenEdgeHit {
    /// The cursor has not reached the shared edge.
    None,
    /// The cursor crossed the shared edge, carrying this fraction along it.
    Crossed { edge: Edge, fraction: f64 },
}

/// Decide whether a cursor at `(x, y)` has been pushed off `local` through
/// the shared `edge` into `remote`.
///
/// `local` and `remote` are the two screens' rects. The check is done in the
/// *virtual desktop* frame, so a cursor sitting exactly on the seam counts as
/// a crossing only when it moves further out. Callers should feed this the
/// post-clamp position plus the movement delta so that a cursor that cannot
/// physically leave the screen still triggers a handoff.
pub fn detect_crossing(
    local: &Rect,
    remote: &Rect,
    edge: Edge,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Result<ScreenEdgeHit, GeometryError> {
    if local.width <= 0.0 || local.height <= 0.0 {
        return Err(GeometryError::EmptyScreen);
    }
    if remote.width <= 0.0 || remote.height <= 0.0 {
        return Err(GeometryError::EmptyScreen);
    }

    if local.intersects_excluding_shared_edge(remote, edge) {
        return Err(GeometryError::OverlappingScreens);
    }

    // Direction of travel along the axis that crosses the seam.
    let (pushing, overshoot) = match edge {
        Edge::Left => (-dx > 0.0, x < local.left()),
        Edge::Right => (dx > 0.0, x > local.right()),
        Edge::Top => (-dy > 0.0, y < local.top()),
        Edge::Bottom => (dy > 0.0, y > local.bottom()),
    };

    if !pushing {
        return Ok(ScreenEdgeHit::None);
    }

    // On the seam, or already past it in virtual-desktop space.
    let on_seam = match edge {
        Edge::Left => x <= local.left(),
        Edge::Right => x >= local.right(),
        Edge::Top => y <= local.top(),
        Edge::Bottom => y >= local.bottom(),
    };

    if on_seam || overshoot {
        Ok(ScreenEdgeHit::Crossed {
            edge,
            fraction: local.fraction_at(edge, x, y),
        })
    } else {
        Ok(ScreenEdgeHit::None)
    }
}

impl Rect {
    fn intersects_excluding_shared_edge(&self, other: &Rect, edge: Edge) -> bool {
        // Overlap on the axis *along* the seam is expected and fine; only
        // overlap on the axis that crosses it is a configuration error.
        let (a_min, a_max, b_min, b_max) = if edge.is_vertical() {
            (self.left(), self.right(), other.left(), other.right())
        } else {
            (self.top(), self.bottom(), other.top(), other.bottom())
        };
        a_max > b_min && b_max > a_min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: f64, y: f64) -> Rect {
        Rect::new(x, y, 1920.0, 1080.0)
    }

    #[test]
    fn opposite_edges_are_symmetric() {
        for edge in Edge::ALL {
            assert_eq!(edge.opposite().opposite(), edge);
            assert_ne!(edge.opposite(), edge);
        }
    }

    #[test]
    fn crossing_right_edge_at_seam() {
        let local = screen(0.0, 0.0);
        let remote = screen(1920.0, 0.0);
        // Cursor at the right edge of local, moving right.
        let hit = detect_crossing(&local, &remote, Edge::Right, 1920.0, 540.0, 4.0, 0.0).unwrap();
        assert_eq!(
            hit,
            ScreenEdgeHit::Crossed {
                edge: Edge::Right,
                fraction: 0.5
            }
        );
    }

    #[test]
    fn no_crossing_when_moving_away() {
        let local = screen(0.0, 0.0);
        let remote = screen(1920.0, 0.0);
        let hit = detect_crossing(&local, &remote, Edge::Right, 1920.0, 540.0, -4.0, 0.0).unwrap();
        assert_eq!(hit, ScreenEdgeHit::None);
    }

    #[test]
    fn no_crossing_in_middle_of_screen() {
        let local = screen(0.0, 0.0);
        let remote = screen(1920.0, 0.0);
        let hit = detect_crossing(&local, &remote, Edge::Right, 960.0, 540.0, 4.0, 0.0).unwrap();
        assert_eq!(hit, ScreenEdgeHit::None);
    }

    #[test]
    fn fraction_is_relative_not_absolute() {
        let local = screen(0.0, 0.0);
        // A peer of a different size and resolution: the same fraction must
        // survive the trip.
        let tall = Rect::new(1920.0, 0.0, 2560.0, 1440.0);
        let hit = detect_crossing(&local, &tall, Edge::Right, 1920.0, 270.0, 1.0, 0.0).unwrap();
        let ScreenEdgeHit::Crossed { fraction, .. } = hit else {
            panic!("expected crossing, got {hit:?}");
        };
        assert!((fraction - 0.25).abs() < 1e-9);
        let (x, y) = tall.point_at_fraction(Edge::Left, fraction);
        assert!((y - 360.0).abs() < 1e-9, "landed at {y}");
        assert_eq!(x, tall.left());
    }

    #[test]
    fn point_at_fraction_and_fraction_at_round_trip() {
        let rect = screen(100.0, 50.0);
        for edge in Edge::ALL {
            for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let (x, y) = rect.point_at_fraction(edge, t);
                let back = rect.fraction_at(edge, x, y);
                assert!((back - t).abs() < 1e-9, "{edge:?} {t} -> {back}");
            }
        }
    }

    #[test]
    fn joined_rectangles_span_both_screens() {
        let local = screen(0.0, 0.0);
        let remote = screen(1920.0, 0.0);
        let joined = local.joined(Edge::Right, &remote);
        assert_eq!(joined.left(), 0.0);
        assert_eq!(joined.right(), 3840.0);
        assert_eq!(joined.height, 1080.0);
    }

    #[test]
    fn overlapping_screens_rejected() {
        let local = screen(0.0, 0.0);
        let remote = screen(960.0, 0.0);
        let err = detect_crossing(&local, &remote, Edge::Right, 1920.0, 0.0, 1.0, 0.0).unwrap_err();
        assert_eq!(err, GeometryError::OverlappingScreens);
    }

    #[test]
    fn empty_screen_rejected() {
        let local = Rect::new(0.0, 0.0, 0.0, 1080.0);
        let remote = screen(1920.0, 0.0);
        let err = detect_crossing(&local, &remote, Edge::Right, 0.0, 0.0, 1.0, 0.0).unwrap_err();
        assert_eq!(err, GeometryError::EmptyScreen);
    }

    #[test]
    fn vertical_edges_share_only_y() {
        let local = screen(0.0, 0.0);
        // Stacked below, so the shared edge is horizontal and only X carries.
        let remote = screen(0.0, 1080.0);
        let hit = detect_crossing(&local, &remote, Edge::Bottom, 960.0, 1080.0, 0.0, 3.0).unwrap();
        assert_eq!(
            hit,
            ScreenEdgeHit::Crossed {
                edge: Edge::Bottom,
                fraction: 0.5
            }
        );
    }
}
