//! Spuria shared crate.
//!
//! Everything that the signaling server, the relay server and the tunnel
//! client must agree on lives here: the wire protocol, the device identity /
//! authentication model, the end-to-end crypto primitives and the
//! "reliable stream + unreliable datagram" tunnel abstraction.
//!
//! See `p2p-rdp-tunnel-plan.md` §9 for the crate division this implements.

pub mod auth;
pub mod candidate;
pub mod crypto;
pub mod error;
pub mod ids;
pub mod protocol;
pub mod ratelimit;
pub mod transport;

pub use error::{Error, Result};
pub use ids::DeviceId;

/// Protocol version negotiated between client and servers. Bumped on any
/// breaking change to [`protocol`].
pub const PROTOCOL_VERSION: u16 = 1;

/// Default ports used across the system (override via config / CLI flags).
pub mod defaults {
    /// Signaling WebSocket control-plane port.
    pub const SIGNALING_PORT: u16 = 21116;
    /// Signaling UDP reflection ("srflx") port — our minimal STUN substitute.
    pub const REFLECT_PORT: u16 = 21117;
    /// Relay TCP data-plane port.
    pub const RELAY_PORT: u16 = 21118;
    /// The local RDP service on the controlled host.
    pub const RDP_PORT: u16 = 3389;
}
