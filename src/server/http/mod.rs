//! S4 request handover: the server owns the connection, the face owns endpoints.
//!
//! See `docs/adr/0016-s4-request-handover.md`.

#[cfg(feature = "http")]
pub mod budget;
pub mod contract;
pub mod dispatch;
#[cfg(all(unix, feature = "http"))]
pub mod driver;
pub mod engine;
#[cfg(all(not(unix), feature = "http"))]
pub mod fallback;
#[cfg_attr(not(unix), allow(dead_code))]
pub mod pool;
#[cfg(all(unix, feature = "http"))]
pub mod server;
pub mod stream;
pub mod wire;

// The S4 contract is deliberately available to the minimal build (parent spec
// #112) even when no HTTP endpoint is compiled in. Re-exports are allowed to
// look unused in one feature/platform combination.
#[allow(unused_imports)]
pub use contract::{
    body_for, BodyError, BodyPolicy, Completion, CompletionSlot, Dispatch, Exchange, Face,
    Principal, Request, Response, Route,
};
#[allow(unused_imports)]
pub use dispatch::{complete_blocking, dispatch_blocking_now, dispatch_inline, DispatchError};
#[allow(unused_imports)]
pub(crate) use dispatch::{prepare_face_request, render_exchange};
#[allow(unused_imports)]
pub use pool::{BlockingPool, Busy, CancelToken, JobOutcome};
#[allow(unused_imports)]
pub use stream::{
    BlockingJob, BlockingOutcome, CloseReason, FileSend, OutBuffer, StreamAction, StreamClass,
    StreamEvent, StreamHandler, StreamStep,
};

/// Buffered-body limit for a request that reaches the ordinary router. Streamed
/// uploads are claimed before this is consulted.
pub(crate) fn body_limit(path: &str) -> u64 {
    let limits = crate::limits::current();
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    if path.starts_with("/api/v1/plugins") || path.starts_with("/api/v1/import") {
        limits.max_body
    } else if path.trim_end_matches('/').ends_with("/complete")
        && (path.starts_with("/api/v1/blobs/uploads/")
            || path.starts_with("/api/v1/file/uploads/")
            || path.starts_with("/api/v1/files/uploads/"))
    {
        limits.max_complete_body
    } else {
        limits.max_json_body
    }
}

/// The shared 503 busy envelope for a route refused before its headers: one
/// status, one error code and one Retry-After header. Callers own the message.
#[cfg(feature = "http")]
pub fn busy_response(message: &str) -> Vec<u8> {
    let body = serde_json::json!({
        "error": { "code": "busy", "message": message }
    });
    crate::http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

/// The busy response for a stream refused before its headers. Both engines send
/// the same stable envelope so a client cannot tell which one refused it.
#[cfg(feature = "http")]
pub fn stream_busy_response() -> Vec<u8> {
    busy_response("stream cap reached")
}
