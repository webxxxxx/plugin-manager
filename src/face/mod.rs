//! L5 face: URL → capability projection.
//!
//! A face never owns a connection; it receives a request value and completes
//! through the S4 completion sink. #113 starts with `/api/v1/version`; the
//! remaining management endpoints move in #117.

pub mod api;
