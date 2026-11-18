//! Daemon ↔ plugin instance: the stdio seam, the raw stream hub and the poll
//! set that drives both.

/// The protocol era whose stdin/stdout carries the raw transport instead of
/// JSON-RPC frames; manifest validation forces `exec.ready=exec` for it.
pub(crate) const RAW_PROTOCOL: u32 = 3;

pub mod rawhub;
#[cfg(unix)]
pub mod reactor;
pub mod stdio;
