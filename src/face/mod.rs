//! L5 face: URL → capability projection.
//!
//! A face never owns a connection; it receives a request value and completes
//! through the S4 completion sink. #113 starts with `/api/v1/version`; #115
//! adds the plugin UI static-asset face; #116 adds the blocking pool; #117
//! moves the remaining management JSON routes, login/logout and the console
//! page here; #119 moves uploads to a pre-body stream face; #120 moves bundle
//! export to a stream handler; #121 moves package import/install uploads to the
//! same pre-body stream face.

#[cfg(feature = "http")]
pub mod api;
#[cfg(feature = "http")]
pub mod blob;
#[cfg(feature = "http")]
pub mod blob_routes;
#[cfg(feature = "http")]
pub mod blob_upload;
pub mod catalog;
pub mod control;
#[cfg(feature = "http")]
pub mod export;
#[cfg(feature = "http")]
pub mod fallback;
#[cfg(feature = "http")]
pub mod install;
pub mod mcp;
pub mod mcp_tools;
#[cfg(feature = "http")]
pub mod plugin_streams;
#[cfg(feature = "http")]
pub mod plugin_ui;
#[cfg(feature = "http")]
pub mod prebody;
#[cfg(feature = "http")]
pub mod support;
#[cfg(feature = "http")]
pub mod uichannel;
#[cfg(feature = "http")]
pub mod webui;
