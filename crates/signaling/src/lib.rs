//! Spuria signaling server library: registry, srflx reflection, WS control
//! plane, and (optional) admin HTTP API. The `spuria-signaling` binary is a
//! thin wrapper over [`server::run`]; integration tests reuse it in-process.

pub mod admin;
pub mod reflect;
pub mod registry;
pub mod server;

pub use server::{run, SignalingConfig};
