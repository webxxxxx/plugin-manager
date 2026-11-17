//! L4 server: protocol sessions and the S4 request-handover seam.

// The HTTP feature supplies the callers. Keep the S4 contract, including the
// sans-IO stream handler, compiled in the minimal build (parent spec #112)
// without failing dead-code analysis for types that later slices still use.
#[cfg(unix)]
pub mod control;
pub mod auth;
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub mod http;
#[cfg_attr(not(all(feature = "http", unix)), allow(dead_code))]
pub mod mcp;
