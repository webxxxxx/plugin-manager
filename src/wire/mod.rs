//! L1 wire: bytes ↔ messages.
//!
//! - `message/` — the shared envelope (JSON-RPC, NDJSON frames)
//! - `contract/` — daemon ↔ plugin instance (stdio) and raw streams
//! - `control/` — daemon ↔ command line (control channel)
//!
//! No routing and no domain concepts (§6, §7).

pub mod contract;
pub mod control;
pub mod message;
