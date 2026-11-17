//! Host facts the rest of the daemon asks about but cannot derive itself.

#[cfg(windows)]
pub mod acl;
pub mod dirs;
pub mod privilege;
pub mod process;
pub mod target;
