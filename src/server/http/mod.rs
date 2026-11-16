//! S4 request handover: the server owns the connection, the face owns endpoints.
//!
//! See `docs/adr/0016-s4-request-handover.md`.

pub mod contract;
pub mod dispatch;
pub mod engine;

#[cfg_attr(not(feature = "http"), allow(unused_imports))]
pub use contract::{
    body_for, BodyError, BodyPolicy, Completion, Exchange, Face, Request, Response, Route,
};
#[cfg_attr(not(feature = "http"), allow(unused_imports))]
pub use dispatch::dispatch_inline;
