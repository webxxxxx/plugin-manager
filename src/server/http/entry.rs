//! The one place that decides which connection driver this host gets.
//!
//! The composition root builds a [`StartInput`] and calls [`start`]; nothing
//! above this module branches on the platform. The drivers live in
//! `server/http/conn`: the poll loop (Unix `poll(2)`, Windows `WSAPoll`,
//! ADR-0027), or one thread per connection on a host with neither. Adding
//! another driver is one more `impl ConnectionDriver` plus the alias below.

use std::io;

pub(crate) use crate::server::http::conn::{ServerHandle, StartInput};

/// The driver this host runs. This alias is the only platform choice left.
/// Unix and Windows both run the poll loop (Windows over `WSAPoll`); the
/// thread driver remains for hosts with neither wait set.
#[cfg(any(unix, windows))]
type HostDriver = crate::server::http::conn::poll::Driver;
#[cfg(not(any(unix, windows)))]
type HostDriver = crate::server::http::conn::threaded::Driver;

/// Start this host's driver.
pub(crate) fn start(input: StartInput) -> io::Result<ServerHandle> {
    use crate::server::http::conn::ConnectionDriver;
    HostDriver::start(input)
}
