//! Spuria relay server library. The `spuria-relay` binary is a thin wrapper
//! over [`server::run`]; integration tests reuse it in-process.

pub mod server;

pub use server::{run, RelayConfig};
