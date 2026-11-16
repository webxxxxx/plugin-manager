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
pub use pool::{BlockingPool, Busy, CancelToken, JobOutcome};
#[allow(unused_imports)]
pub use stream::{
    BlockingJob, BlockingOutcome, CloseReason, FileSend, OutBuffer, StreamAction, StreamClass,
    StreamEvent, StreamHandler, StreamStep,
};

/// The busy response for a stream refused before its headers. Both engines send
/// the same stable envelope so a client cannot tell which one refused it.
#[cfg(feature = "http")]
pub fn stream_busy_response() -> Vec<u8> {
    let body = serde_json::json!({
        "error": { "code": "busy", "message": "stream cap reached" }
    });
    crate::http::response_with_headers(
        503,
        "application/json",
        &[("Retry-After".to_string(), "1".to_string())],
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}
