//! L4 MCP sessions: protocol state that does not own a connection.
//!
//! The HTTP transport in `mcp_http` stays at the endpoint boundary; the state
//! machines that outlive one request live here as the S4 sans-IO handlers.

#[cfg_attr(not(all(feature = "http", unix)), allow(dead_code))]
pub mod progress;
pub mod result;
