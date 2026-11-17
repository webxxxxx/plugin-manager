//! L5 face: URL → capability projection.
//!
//! A face never owns a connection; it receives a request value and completes
//! through the S4 completion sink. #113 starts with `/api/v1/version`; #115
//! adds the plugin UI static-asset face; #116 adds the blocking pool; #117
//! moves the remaining management JSON routes, login/logout and the console
//! page here; #119 moves uploads to a pre-body stream face; #120 moves bundle
//! export to a stream handler; #121 moves package import/install uploads to the
//! same pre-body stream face.

pub mod api;
pub mod blob;
pub mod blob_routes;
pub mod blob_upload;
pub mod export;
pub mod install;
pub mod plugin_streams;
pub mod plugin_ui;
pub mod support;
