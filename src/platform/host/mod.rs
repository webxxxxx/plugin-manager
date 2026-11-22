//! Host facts the rest of the daemon asks about but cannot derive itself.

#[cfg(windows)]
pub mod acl;
pub mod cpu;
pub mod dirs;
pub mod notify;
pub mod poll;
pub mod privilege;
pub mod process;
pub mod signal;
pub mod target;
pub mod threads;
pub mod wake;
// Only a connection loop owns a wake pipe, and the control-only loop that uses
// it exists on Unix only.
#[cfg(any(unix, feature = "http"))]
pub mod wake_pipe;
#[cfg(feature = "http")]
pub mod zerocopy;
