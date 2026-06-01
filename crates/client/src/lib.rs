//! Spuria tunnel client library (controller + host roles).
//!
//! The `spuria` CLI binary and the Tauri desktop GUI both drive
//! [`app::run`]. The GUI consumes [`app::ClientEvent`]s pushed onto the
//! optional events channel in [`app::AppConfig`].

pub mod app;
pub mod candidates;
pub mod certs;
pub mod rdp;
pub mod signaling_client;
pub mod tunnel;

pub use app::{run, AppConfig, ClientEvent};
