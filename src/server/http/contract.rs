//! Values and traits for the S4 seam.
//!
//! Nothing here owns a socket, a file descriptor, a thread, or a deadline;
//! those stay with the server. A face gets a [`Request`] and answers through a
//! [`Completion`], exactly once.

use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;

use crate::http;
use crate::lan::AuthKind;
use crate::log;

/// How the server treats a request body before [`Face::handle`] runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyPolicy {
    /// The route does not read a body. The request value carries an empty body.
    Empty,
    /// The server buffers the complete body (up to `limit`) before `handle`.
    #[allow(dead_code)] // #114 wires the first Buffered route.
    Buffered { limit: u64 },
    /// The server does not buffer; the returned stream handler consumes the body.
    Streamed,
}

/// Why a route's body cannot be handed to the face.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The buffered body is larger than the route's `Buffered { limit }`.
    TooLarge,
}

/// The body value a face receives for a route. The server has already read the
/// request at this point in the legacy engines, so this copies or drops it
/// according to the route policy.
pub fn body_for(policy: BodyPolicy, body: &[u8]) -> Result<Vec<u8>, BodyError> {
    match policy {
        BodyPolicy::Empty | BodyPolicy::Streamed => Ok(Vec::new()),
        BodyPolicy::Buffered { limit } => {
            if body.len() as u64 > limit {
                Err(BodyError::TooLarge)
            } else {
                Ok(body.to_vec())
            }
        }
    }
}

/// Where a face handler runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    /// On the server loop, without blocking.
    Inline,
    /// On the blocking pool; the completion arrives later on the server loop.
    Blocking,
}

/// The route/body decision a face makes before the body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub body: BodyPolicy,
    pub dispatch: Dispatch,
    /// A public route must run without a resolved principal: login, logout and
    /// the console page are reachable before a session exists. The face owns
    /// the endpoint policy for its own public routes; the server only skips the
    /// transport authentication step.
    pub public: bool,
}

impl Route {
    pub fn inline(body: BodyPolicy) -> Route {
        Route {
            body,
            dispatch: Dispatch::Inline,
            public: false,
        }
    }

    /// A handler that blocks on a control-channel round trip or disk work.
    /// The server hands it to the blocking pool; the completion comes back on
    /// the server loop.
    pub fn blocking(body: BodyPolicy) -> Route {
        Route {
            body,
            dispatch: Dispatch::Blocking,
            public: false,
        }
    }

    /// Declare that the route must be reachable before authentication.
    pub fn public(mut self) -> Route {
        self.public = true;
        self
    }
}

/// Authentication resolved by the server once, before the face runs.
pub type Principal = AuthKind;

/// One request value. It carries no socket, file descriptor or connection guard.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Raw request target, including the query string.
    pub path: String,
    #[allow(dead_code)] // #117 endpoint policy reads headers.
    pub headers: Vec<(String, String)>,
    #[allow(dead_code)] // #117 endpoint policy reads peer.
    pub peer: IpAddr,
    #[allow(dead_code)] // #117 endpoint policy reads the resolved auth context.
    pub principal: Principal,
    /// Complete body for `Buffered` routes; empty for `Empty` and `Streamed`.
    #[allow(dead_code)] // #114/#116 wire the first Buffered route.
    pub body: Vec<u8>,
}

impl Request {
    pub fn path_only(&self) -> &str {
        self.path
            .split_once('?')
            .map(|(path, _)| path)
            .unwrap_or(&self.path)
    }
}

/// A complete in-memory response. The server renders the wire bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn new(status: u16, content_type: impl Into<String>, body: impl Into<Vec<u8>>) -> Response {
        Response {
            status,
            content_type: content_type.into(),
            headers: Vec::new(),
            body: body.into(),
        }
    }

    pub fn json(status: u16, value: &Value) -> Response {
        Response::new(
            status,
            "application/json",
            serde_json::to_vec(value).unwrap_or_default(),
        )
    }

    #[allow(dead_code)] // #117 faces add response headers.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn into_bytes(self) -> Vec<u8> {
        http::response_with_headers(self.status, &self.content_type, &self.headers, &self.body)
    }
}

/// The two and only two outcomes of a face request.
pub enum Exchange {
    Response(Response),
    Stream(Box<dyn StreamHandler>),
}

impl Exchange {
    pub fn kind(&self) -> ExchangeKind {
        match self {
            Exchange::Response(_) => ExchangeKind::Response,
            Exchange::Stream(_) => ExchangeKind::Stream,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExchangeKind {
    Response,
    Stream,
}

pub use super::stream::StreamHandler;

/// The one new seam: a face makes a route decision and handles a request.
pub trait Face: Send + Sync {
    /// Decide whether this face owns the request and how its body is handled.
    /// `None` leaves the request to the server-owned buffered routes. The
    /// pre-body stream hooks run before this decision.
    fn route(&self, method: &str, path: &str) -> Option<Route>;

    /// Handle a routed request. Must call `done.complete` exactly once.
    fn handle(&self, request: Request, done: Completion);
}

/// The result of a completion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionOutcome {
    Completed,
    AlreadyCompleted,
}

/// The server's end of one request's completion slot.
pub struct CompletionSlot {
    state: Arc<Mutex<SlotState>>,
}

/// The face's end of one request's completion slot.
pub struct Completion {
    state: Arc<Mutex<SlotState>>,
}

struct SlotState {
    /// Human-readable endpoint label for duplicate-completion logs.
    endpoint: String,
    exchange: Option<Exchange>,
    sealed: bool,
    accepted: u64,
    rejected: u64,
}

impl CompletionSlot {
    pub fn new(endpoint: impl Into<String>) -> (CompletionSlot, Completion) {
        let state = Arc::new(Mutex::new(SlotState {
            endpoint: endpoint.into(),
            exchange: None,
            sealed: false,
            accepted: 0,
            rejected: 0,
        }));
        (
            CompletionSlot {
                state: state.clone(),
            },
            Completion { state },
        )
    }

    /// Take the one accepted exchange and seal the slot. A completion that
    /// arrives after this point is ignored, so a late handler cannot write into
    /// a request the server already answered or dropped.
    pub fn take(&self) -> Option<Exchange> {
        let mut state = self.lock();
        state.sealed = true;
        state.exchange.take()
    }

    #[cfg(test)]
    pub fn accepted(&self) -> u64 {
        self.lock().accepted
    }

    #[cfg(test)]
    pub fn rejected(&self) -> u64 {
        self.lock().rejected
    }

    fn lock(&self) -> MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }
}

impl Completion {
    /// Deliver the one outcome. The first call wins; a second call is logged
    /// and ignored. The server seals the slot when it gives up, which also
    /// turns a late call into an ignored one.
    pub fn complete(&self, exchange: Exchange) -> CompletionOutcome {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.sealed || state.exchange.is_some() {
            state.rejected = state.rejected.saturating_add(1);
            let endpoint = state.endpoint.clone();
            drop(state);
            log::svcd(&format!(
                "svcd: face completed {} more than once; the extra completion is ignored",
                endpoint
            ));
            return CompletionOutcome::AlreadyCompleted;
        }
        state.exchange = Some(exchange);
        state.accepted = state.accepted.saturating_add(1);
        CompletionOutcome::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response(body: &'static [u8]) -> Exchange {
        Exchange::Response(Response::new(200, "text/plain", body))
    }

    #[test]
    fn a_completion_is_delivered_exactly_once() {
        let (slot, done) = CompletionSlot::new("GET /test");
        assert_eq!(
            done.complete(response(b"first")),
            CompletionOutcome::Completed
        );
        assert_eq!(
            done.complete(response(b"second")),
            CompletionOutcome::AlreadyCompleted
        );
        assert_eq!(slot.accepted(), 1);
        assert_eq!(slot.rejected(), 1);

        let Some(Exchange::Response(taken)) = slot.take() else {
            panic!("one response");
        };
        assert_eq!(taken.body, b"first", "the second completion must not win");
        assert!(slot.take().is_none(), "the slot only yields one exchange");
    }

    #[test]
    fn a_late_completion_is_ignored_after_the_server_seals_the_slot() {
        let (slot, done) = CompletionSlot::new("GET /test");
        assert!(slot.take().is_none(), "the handler has not completed");
        assert_eq!(
            done.complete(response(b"late")),
            CompletionOutcome::AlreadyCompleted
        );
        assert_eq!(slot.rejected(), 1);
        assert!(slot.take().is_none());
    }

    #[test]
    fn dropping_the_sink_without_completing_leaves_the_slot_empty() {
        let (slot, done) = CompletionSlot::new("GET /test");
        drop(done);
        assert!(slot.take().is_none());
        assert_eq!(slot.accepted(), 0);
    }

    #[test]
    fn response_headers_are_rendered_by_the_server() {
        let bytes = Response::new(200, "text/plain", b"ok".to_vec())
            .with_header("X-Test", "1")
            .into_bytes();
        let text = String::from_utf8(bytes).expect("utf8");
        assert!(text.contains("\r\nX-Test: 1\r\n"), "{text}");
    }

    #[test]
    fn a_body_is_copied_only_for_a_buffered_route_within_its_limit() {
        assert_eq!(body_for(BodyPolicy::Empty, b"ignored"), Ok(Vec::new()));
        assert_eq!(body_for(BodyPolicy::Streamed, b"ignored"), Ok(Vec::new()));
        assert_eq!(
            body_for(BodyPolicy::Buffered { limit: 4 }, b"body"),
            Ok(b"body".to_vec())
        );
        assert_eq!(
            body_for(BodyPolicy::Buffered { limit: 3 }, b"body"),
            Err(BodyError::TooLarge)
        );
    }

    #[test]
    fn a_memory_response_renders_with_the_legacy_http_wire_format() {
        let value = json!({"version": "0.0.0"});
        let expected = crate::http::response(
            200,
            "application/json",
            &serde_json::to_vec(&value).expect("json"),
        );
        assert_eq!(Response::json(200, &value).into_bytes(), expected);
    }

    #[test]
    fn a_request_splits_the_query_without_decoding_it() {
        let request = Request {
            method: "GET".into(),
            path: "/api/v1/version?x=a%2Fb".into(),
            headers: Vec::new(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: Principal::None,
            body: Vec::new(),
        };
        assert_eq!(request.path_only(), "/api/v1/version");
    }
}
