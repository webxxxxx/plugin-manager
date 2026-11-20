//! Host facts the rest of the daemon asks about but cannot derive itself.

#[cfg(windows)]
pub mod acl;
pub mod dirs;
#[cfg(unix)]
pub mod poll;
pub mod privilege;
pub mod process;
pub mod target;
pub mod threads;
pub mod wake;
#[cfg(feature = "http")]
pub mod zerocopy;
