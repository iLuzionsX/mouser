//! Core shared logic for mouser.
//!
//! This crate is deliberately free of platform and networking APIs so the
//! interesting parts -- layout math, the wire protocol, and the link state
//! machine -- can be unit tested on any host.

pub mod config;
pub mod layout;
pub mod protocol;
pub mod session;

pub use config::{Config, ConfigError, Role};
pub use layout::{Edge, GeometryError, Rect, ScreenEdgeHit};
pub use protocol::{Message, PROTOCOL_VERSION, PeerHello};
pub use session::{LinkState, SessionEvent, Transition};
