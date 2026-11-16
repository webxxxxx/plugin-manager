//! S4 request handover: the server owns the connection, the face owns endpoints.
//!
//! See `docs/adr/0016-s4-request-handover.md`.

pub mod contract;
pub mod dispatch;
pub mod engine;
#[cfg_attr(not(unix), allow(dead_code))]
pub mod pool;

#[cfg(unix)]
pub use contract::CompletionSlot;
#[cfg_attr(not(feature = "http"), allow(unused_imports))]
pub use contract::{
    body_for, BodyError, BodyPolicy, Completion, Dispatch, Exchange, Face, Request, Response, Route,
};
#[cfg(unix)]
pub use dispatch::complete_blocking;
#[cfg(not(unix))]
pub use dispatch::dispatch_blocking_now;
#[cfg_attr(not(feature = "http"), allow(unused_imports))]
pub use dispatch::{dispatch_inline, DispatchError};
#[cfg_attr(any(not(feature = "http"), not(unix)), allow(unused_imports))]
pub use pool::{BlockingPool, Busy, CancelToken, JobOutcome};
