//! L1 wire: bytes ↔ messages.
//!
//! - `message/` — the shared envelope (JSON-RPC, NDJSON frames)
//! - `contract/` — daemon ↔ plugin instance (stdio) and raw streams
//! - `control/` — daemon ↔ command line (control channel)
//! - `ws/` — WebSocket frame shape and opening handshake
//!
//! No routing and no domain concepts (§6, §7).

pub mod contract;
pub mod control;
pub mod message;
/// WebSocket frame shape and opening handshake. Only the HTTP feature has a
/// consumer (the plugin UI streams), so the minimal build does not compile it —
/// same visibility as the `server/http` module it moved out of (#206).
#[cfg(feature = "http")]
pub(crate) mod ws;
