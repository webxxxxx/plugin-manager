//! S4 request handover: the server owns the connection, the face owns endpoints.
//!
//! See `docs/adr/0016-s4-request-handover.md`.

pub mod contract;
pub mod dispatch;
pub mod engine;
pub mod stream;

// The S4 contract is deliberately available to the minimal build (parent spec
// #112) even when no HTTP endpoint is compiled in. Re-exports are allowed to
// look unused in one feature/platform combination.
#[allow(unused_imports)]
pub use contract::{
    body_for, BodyError, BodyPolicy, Completion, CompletionSlot, Dispatch, Exchange, Face,
    Principal, Request, Response, Route,
};
#[allow(unused_imports)]
pub use dispatch::{dispatch_inline, DispatchError};
#[allow(unused_imports)]
pub use stream::{
    BlockingJob, BlockingOutcome, CloseReason, FileSend, OutBuffer, StreamAction, StreamClass,
    StreamEvent, StreamHandler, StreamStep,
};
