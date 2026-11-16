//! L5 face: URL → capability projection.
//!
//! A face never owns a connection; it receives a request value and completes
//! through the S4 completion sink. #113 starts with `/api/v1/version`; #115
//! adds the plugin UI static-asset face; #116 adds the blocking pool; #117
//! moves the remaining management JSON routes, login/logout and the console
//! page here.

pub mod api;
pub mod plugin_ui;
