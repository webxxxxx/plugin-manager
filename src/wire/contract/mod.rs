//! Daemon ↔ plugin instance: the stdio seam, the raw stream hub and the poll
//! set that drives both.

#[cfg(unix)]
pub mod reactor;
pub mod rawhub;
pub mod stdio;
