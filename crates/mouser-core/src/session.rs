//! The link state machine.
//!
//! Exactly one machine owns the physical mouse and keyboard at a time. This
//! type tracks who that is, decides when ownership changes hands, and is the
//! single place where the handoff rules live. It has no I/O, so the rules can
//! be tested directly.

use serde::{Deserialize, Serialize};

use crate::layout::Edge;

/// Who currently drives the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkState {
    /// This machine has the physical mouse and keyboard.
    Local,
    /// The peer drives; we mirror its input and show a remote cursor.
    Remote,
    /// Not connected to a peer.
    Disconnected,
}

/// Something the session wants the runtime to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Transition {
    /// Stay put.
    None,
    /// Cursor crossed the shared edge; hand control to the peer.
    ///
    /// `fraction` is the normalized position along the edge so the peer's
    /// cursor lands at the matching spot on screens of different sizes.
    Handoff { edge: Edge, fraction: f64 },
    /// The peer gave control back, or we pulled it back locally.
    Regain,
    /// A release hotkey was pressed while remote; return control home.
    PullBack,
}

/// Something observed by the session, produced by the runtime.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionEvent {
    /// A local cursor position with its movement delta.
    CursorMoved { x: f64, y: f64, dx: f64, dy: f64 },
    /// The peer reported a cursor position on its side.
    RemoteCursorMoved { x: f64, y: f64, dx: f64, dy: f64 },
    /// Release hotkey pressed.
    ReleaseHotkey,
    /// Link came up.
    Connected,
    /// Link went down.
    Disconnected,
}

/// Tracks ownership and applies the handoff rules.
#[derive(Debug, Clone)]
pub struct Session {
    state: LinkState,
    edge: Edge,
    last_local: (f64, f64),
}

impl Session {
    pub fn new(edge: Edge) -> Self {
        Self {
            state: LinkState::Disconnected,
            edge,
            last_local: (0.0, 0.0),
        }
    }

    pub fn state(&self) -> LinkState {
        self.state
    }

    pub fn edge(&self) -> Edge {
        self.edge
    }

    /// Reposition the peer without dropping the link. Takes effect next tick.
    pub fn set_edge(&mut self, edge: Edge) {
        self.edge = edge;
    }

    pub fn is_local(&self) -> bool {
        self.state == LinkState::Local
    }

    /// Feed an observed event and get back the action to take.
    ///
    /// `detect_crossing` is injected so this stays free of geometry
    /// specifics and easy to test; callers pass
    /// [`crate::layout::detect_crossing`] in production.
    pub fn on_event<F>(&mut self, event: SessionEvent, mut detect_crossing: F) -> Transition
    where
        F: FnMut(Edge, f64, f64, f64, f64) -> bool,
    {
        match event {
            SessionEvent::Connected => {
                self.state = LinkState::Local;
                Transition::None
            }

            SessionEvent::Disconnected => {
                self.state = LinkState::Disconnected;
                Transition::Regain
            }

            SessionEvent::ReleaseHotkey => match self.state {
                // Pulling control back must never strand the pointer off
                // screen, so it is always allowed.
                LinkState::Remote => {
                    self.state = LinkState::Local;
                    Transition::PullBack
                }
                _ => Transition::None,
            },

            SessionEvent::CursorMoved { x, y, dx, dy } => {
                self.last_local = (x, y);
                if self.state != LinkState::Local {
                    return Transition::None;
                }
                if detect_crossing(self.edge, x, y, dx, dy) {
                    self.state = LinkState::Remote;
                    Transition::Handoff {
                        edge: self.edge,
                        fraction: 0.0,
                    }
                } else {
                    Transition::None
                }
            }

            SessionEvent::RemoteCursorMoved { .. } => {
                if self.state == LinkState::Remote {
                    self.state = LinkState::Local;
                    Transition::Regain
                } else {
                    Transition::None
                }
            }
        }
    }

    pub fn last_local_position(&self) -> (f64, f64) {
        self.last_local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn always(edge: Edge) -> impl FnMut(Edge, f64, f64, f64, f64) -> bool {
        move |requested, _x, _y, _dx, _dy| requested == edge
    }

    fn never() -> impl FnMut(Edge, f64, f64, f64, f64) -> bool {
        |_, _, _, _, _| false
    }

    #[test]
    fn starts_disconnected() {
        let s = Session::new(Edge::Right);
        assert_eq!(s.state(), LinkState::Disconnected);
    }

    #[test]
    fn connect_grants_local_control() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        assert_eq!(s.state(), LinkState::Local);
    }

    #[test]
    fn crossing_shared_edge_hands_off_control() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        let t = s.on_event(
            SessionEvent::CursorMoved {
                x: 1919.0,
                y: 500.0,
                dx: 5.0,
                dy: 0.0,
            },
            always(Edge::Right),
        );
        assert_eq!(
            t,
            Transition::Handoff {
                edge: Edge::Right,
                fraction: 0.0
            }
        );
        assert_eq!(s.state(), LinkState::Remote);
    }

    #[test]
    fn no_handoff_when_movement_does_not_cross() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        let t = s.on_event(
            SessionEvent::CursorMoved {
                x: 100.0,
                y: 100.0,
                dx: 1.0,
                dy: 0.0,
            },
            never(),
        );
        assert_eq!(t, Transition::None);
        assert_eq!(s.state(), LinkState::Local);
    }

    #[test]
    fn only_one_handoff_per_crossing() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        let crossing = SessionEvent::CursorMoved {
            x: 1919.0,
            y: 10.0,
            dx: 5.0,
            dy: 0.0,
        };
        let first = s.on_event(crossing, always(Edge::Right));
        assert!(matches!(first, Transition::Handoff { .. }));
        // Still over the edge, but we already own the remote side.
        let second = s.on_event(crossing, always(Edge::Right));
        assert_eq!(second, Transition::None);
    }

    #[test]
    fn remote_cursor_movement_regains_control() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        s.on_event(
            SessionEvent::CursorMoved {
                x: 1919.0,
                y: 0.0,
                dx: 1.0,
                dy: 0.0,
            },
            always(Edge::Right),
        );
        assert_eq!(s.state(), LinkState::Remote);
        let t = s.on_event(
            SessionEvent::RemoteCursorMoved {
                x: 0.0,
                y: 0.0,
                dx: -1.0,
                dy: 0.0,
            },
            never(),
        );
        assert_eq!(t, Transition::Regain);
        assert_eq!(s.state(), LinkState::Local);
    }

    #[test]
    fn release_hotkey_pulls_control_back() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        s.on_event(
            SessionEvent::CursorMoved {
                x: 1919.0,
                y: 0.0,
                dx: 1.0,
                dy: 0.0,
            },
            always(Edge::Right),
        );
        assert_eq!(s.state(), LinkState::Remote);
        let t = s.on_event(SessionEvent::ReleaseHotkey, never());
        assert_eq!(t, Transition::PullBack);
        assert_eq!(s.state(), LinkState::Local);
    }

    #[test]
    fn release_hotkey_is_ignored_while_local() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        let t = s.on_event(SessionEvent::ReleaseHotkey, never());
        assert_eq!(t, Transition::None);
        assert_eq!(s.state(), LinkState::Local);
    }

    #[test]
    fn disconnect_returns_control_locally() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        s.on_event(
            SessionEvent::CursorMoved {
                x: 1919.0,
                y: 0.0,
                dx: 1.0,
                dy: 0.0,
            },
            always(Edge::Right),
        );
        let t = s.on_event(SessionEvent::Disconnected, never());
        assert_eq!(t, Transition::Regain);
        assert_eq!(s.state(), LinkState::Disconnected);
    }

    #[test]
    fn changing_edge_takes_effect_immediately() {
        let mut s = Session::new(Edge::Right);
        s.on_event(SessionEvent::Connected, never());
        s.set_edge(Edge::Bottom);
        let t = s.on_event(
            SessionEvent::CursorMoved {
                x: 0.0,
                y: 1079.0,
                dx: 0.0,
                dy: 1.0,
            },
            always(Edge::Bottom),
        );
        assert!(matches!(t, Transition::Handoff { .. }));
        assert_eq!(s.edge(), Edge::Bottom);
    }

    #[test]
    fn tracks_last_local_position() {
        let mut s = Session::new(Edge::Left);
        s.on_event(SessionEvent::Connected, never());
        s.on_event(
            SessionEvent::CursorMoved {
                x: 42.0,
                y: 77.0,
                dx: 1.0,
                dy: 1.0,
            },
            never(),
        );
        assert_eq!(s.last_local_position(), (42.0, 77.0));
    }
}
