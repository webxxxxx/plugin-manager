//! Inline dispatch and exchange validation for the S4 seam.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use crate::http;
use crate::lan::{AuthKind, Lan};
use crate::log;

use super::contract::{
    body_for, BodyError, BodyPolicy, CompletionSlot, Dispatch, Exchange, ExchangeKind, Face,
    Request, Route,
};

/// Why the server could not accept a face result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// The handler returned without completing, or dropped the sink.
    MissingCompletion,
    /// The exchange does not match the route's body policy.
    WrongExchange { body: BodyPolicy, got: ExchangeKind },
    /// The route asked for a dispatch mode this server path does not provide.
    UnsupportedDispatch(Dispatch),
}

impl DispatchError {
    pub fn message(&self) -> String {
        match self {
            DispatchError::MissingCompletion => {
                "request handler did not complete the request".to_string()
            }
            DispatchError::WrongExchange { body, got } => format!(
                "request handler returned {} for a {} route",
                match got {
                    ExchangeKind::Response => "a memory response",
                    ExchangeKind::Stream => "a stream handler",
                },
                match body {
                    BodyPolicy::Empty => "no-body",
                    BodyPolicy::Buffered { .. } => "buffered",
                    BodyPolicy::Streamed => "streamed",
                }
            ),
            DispatchError::UnsupportedDispatch(dispatch) => format!(
                "{} dispatch is not available on this server path",
                match dispatch {
                    Dispatch::Inline => "inline",
                    Dispatch::Blocking => "blocking",
                }
            ),
        }
    }
}

/// A face-owned request whose principal is resolved but whose body is still
/// on the wire. The server reads the body under `route.body` and only then
/// attaches it, so authentication and the route's limit are enforced before a
/// single body byte is buffered.
pub(crate) struct PreparedFace {
    pub(crate) route: Route,
    pub(crate) request: Request,
}

/// Ask the face whether it owns the request and resolve the principal. This is
/// the one `Face::route` call for a request; `Ok(None)` leaves the request to
/// the server-owned routes, `Err` is a rendered authentication error.
pub(crate) fn route_face_request(
    face: &dyn Face,
    head: &http::Head,
    peer: IpAddr,
    lan: &Arc<Mutex<Lan>>,
) -> Result<Option<PreparedFace>, Vec<u8>> {
    let Some(route) = face.route(&head.method, &head.path) else {
        return Ok(None);
    };
    let principal = if route.public {
        AuthKind::None
    } else {
        match lan.lock() {
            Ok(mut state) => match state.authenticate(&head.headers) {
                Ok(principal) => principal,
                Err(()) => {
                    return Err(http::json_error(
                        401,
                        "unauthorized",
                        "authentication required",
                    ))
                }
            },
            Err(_) => return Err(http::json_error(500, "internal", "LAN state unavailable")),
        }
    };
    Ok(Some(PreparedFace {
        route,
        request: Request {
            method: head.method.clone(),
            path: head.path.clone(),
            headers: head.headers.clone(),
            peer,
            principal,
            body: Vec::new(),
        },
    }))
}

/// Attach the body the server read under `route.body`. The server already
/// enforced the limit on the wire; this repeats the check so a future engine
/// cannot hand a route more than it declared.
pub(crate) fn attach_face_body(
    mut prepared: PreparedFace,
    body: &[u8],
) -> Result<PreparedFace, Vec<u8>> {
    prepared.request.body = match body_for(prepared.route.body, body) {
        Ok(body) => body,
        Err(BodyError::TooLarge) => {
            return Err(http::json_error(
                413,
                "payload_too_large",
                "request body is too large",
            ))
        }
    };
    Ok(prepared)
}

/// Turn a dispatch result into wire bytes.
#[cfg_attr(not(unix), allow(dead_code))] // the Windows fallback writes each exchange itself
pub(crate) fn render_exchange(result: Result<Exchange, DispatchError>) -> Vec<u8> {
    match result {
        Ok(Exchange::Response(response)) => response.into_bytes(),
        Ok(Exchange::Stream(_)) => http::json_error(
            501,
            "not_implemented",
            "streamed face routes are not wired into this engine yet",
        ),
        Err(cause) => http::json_error(500, "internal", &cause.message()),
    }
}

/// Run one inline face request and take its exactly-once result.
pub fn dispatch_inline(
    face: &dyn Face,
    route: &Route,
    request: Request,
) -> Result<Exchange, DispatchError> {
    dispatch_now(face, route, request, Dispatch::Inline)
}

/// Run one blocking route on the calling thread. The Unix server loop hands
/// `Blocking` routes to the blocking pool instead; the Windows per-connection
/// fallback uses this because its connection thread is already the dedicated
/// worker for one request.
#[cfg_attr(unix, allow(dead_code))] // Unix uses BlockingPool instead.
pub fn dispatch_blocking_now(
    face: &dyn Face,
    route: &Route,
    request: Request,
) -> Result<Exchange, DispatchError> {
    dispatch_now(face, route, request, Dispatch::Blocking)
}

fn dispatch_now(
    face: &dyn Face,
    route: &Route,
    request: Request,
    expected: Dispatch,
) -> Result<Exchange, DispatchError> {
    if route.dispatch != expected {
        return Err(DispatchError::UnsupportedDispatch(route.dispatch));
    }
    let endpoint = format!("{} {}", request.method, request.path);
    let (slot, done) = CompletionSlot::new(endpoint.clone());
    face.handle(request, done);
    let Some(exchange) = slot.take() else {
        log::svcd(&format!("svcd: face did not complete {}", endpoint));
        return Err(DispatchError::MissingCompletion);
    };
    match check_exchange(route.body, exchange) {
        Ok(exchange) => Ok(exchange),
        Err(error) => {
            log::svcd(&format!(
                "svcd: face violated the exchange contract for {}: {}",
                endpoint,
                error.message()
            ));
            Err(error)
        }
    }
}

/// Validate the exchange a blocking job produced. A missing completion is an
/// implementation error, exactly as it is on the inline path.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn complete_blocking(
    body: BodyPolicy,
    exchange: Option<Exchange>,
) -> Result<Exchange, DispatchError> {
    match exchange {
        Some(exchange) => check_exchange(body, exchange),
        None => Err(DispatchError::MissingCompletion),
    }
}

fn check_exchange(body: BodyPolicy, exchange: Exchange) -> Result<Exchange, DispatchError> {
    let got = exchange.kind();
    let accepted = matches!(
        (body, got),
        (BodyPolicy::Streamed, ExchangeKind::Stream)
            | (
                BodyPolicy::Buffered { .. } | BodyPolicy::Empty,
                ExchangeKind::Response
            )
    );
    if accepted {
        Ok(exchange)
    } else {
        Err(DispatchError::WrongExchange { body, got })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::http::contract::{Completion, Response, StreamHandler};
    use crate::server::http::{OutBuffer, StreamAction, StreamEvent, StreamStep};
    use serde_json::json;

    struct NoopStream;

    impl StreamHandler for NoopStream {
        fn on_event(&mut self, _event: StreamEvent<'_>, _out: &mut OutBuffer<'_>) -> StreamStep {
            StreamStep::wait(StreamAction::Continue)
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Behavior {
        Respond,
        Ignore,
        CompleteTwice,
        Stream,
    }

    struct FakeFace {
        behavior: Behavior,
    }

    impl Face for FakeFace {
        fn route(&self, _method: &str, _path: &str) -> Option<Route> {
            Some(Route::inline(BodyPolicy::Empty))
        }

        fn handle(&self, _request: Request, done: Completion) {
            match self.behavior {
                Behavior::Respond => {
                    done.complete(Exchange::Response(Response::json(
                        200,
                        &json!({ "ok": true }),
                    )));
                }
                Behavior::Ignore => drop(done),
                Behavior::CompleteTwice => {
                    done.complete(Exchange::Response(Response::new(
                        200,
                        "text/plain",
                        b"first".to_vec(),
                    )));
                    done.complete(Exchange::Response(Response::new(
                        500,
                        "text/plain",
                        b"second".to_vec(),
                    )));
                }
                Behavior::Stream => {
                    done.complete(Exchange::Stream(Box::new(NoopStream)));
                }
            }
        }
    }

    fn request() -> Request {
        Request {
            method: "GET".into(),
            path: "/api/v1/version".into(),
            headers: Vec::new(),
            peer: "127.0.0.1".parse().expect("peer"),
            principal: crate::lan::AuthKind::None,
            body: Vec::new(),
        }
    }

    fn route(body: BodyPolicy) -> Route {
        Route::inline(body)
    }

    fn expect_error(result: Result<Exchange, DispatchError>) -> DispatchError {
        match result {
            Err(error) => error,
            Ok(_) => panic!("expected a dispatch error, got a successful exchange"),
        }
    }

    #[test]
    fn a_completed_face_returns_its_response() {
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let exchange = dispatch_inline(&face, &route(BodyPolicy::Empty), request())
            .expect("a completed face must return its exchange");
        let Exchange::Response(response) = exchange else {
            panic!("the fake face returned a memory response");
        };
        assert_eq!(response.status, 200);
        assert_eq!(
            response.body,
            serde_json::to_vec(&json!({ "ok": true })).expect("json")
        );
    }

    #[test]
    fn a_handler_that_never_completes_is_an_implementation_error() {
        let face = FakeFace {
            behavior: Behavior::Ignore,
        };
        let error = expect_error(dispatch_inline(&face, &route(BodyPolicy::Empty), request()));
        assert_eq!(error, DispatchError::MissingCompletion);
    }

    #[test]
    fn a_face_unit_test_covers_response_exactly_once_missing_and_double_completion() {
        // One response, accepted exactly once.
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let (slot, done) = CompletionSlot::new("GET /api/v1/version");
        face.handle(request(), done);
        assert_eq!(slot.accepted(), 1);
        assert_eq!(slot.rejected(), 0);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the face must produce one memory response");
        };
        assert_eq!(response.status, 200);

        // Two completions: the first wins, the second is ignored.
        let face = FakeFace {
            behavior: Behavior::CompleteTwice,
        };
        let (slot, done) = CompletionSlot::new("GET /api/v1/version");
        face.handle(request(), done);
        assert_eq!(slot.accepted(), 1);
        assert_eq!(slot.rejected(), 1);
        let Some(Exchange::Response(response)) = slot.take() else {
            panic!("the first completion must be kept");
        };
        assert_eq!(response.body, b"first");

        // Dropping the sink without a call leaves the slot empty.
        let face = FakeFace {
            behavior: Behavior::Ignore,
        };
        let (slot, done) = CompletionSlot::new("GET /api/v1/version");
        face.handle(request(), done);
        assert_eq!(slot.accepted(), 0);
        assert!(slot.take().is_none());
    }

    #[test]
    fn a_second_completion_does_not_replace_the_first_response() {
        let face = FakeFace {
            behavior: Behavior::CompleteTwice,
        };
        let exchange = dispatch_inline(&face, &route(BodyPolicy::Empty), request())
            .expect("the first completion wins");
        let Exchange::Response(response) = exchange else {
            panic!("the fake face returned a memory response");
        };
        assert_eq!(response.body, b"first");
    }

    #[test]
    fn a_stream_handler_is_accepted_only_on_a_streamed_route() {
        let face = FakeFace {
            behavior: Behavior::Stream,
        };
        match dispatch_inline(&face, &route(BodyPolicy::Streamed), request()) {
            Ok(Exchange::Stream(_)) => {}
            Ok(_) => panic!("a streamed route must accept a stream handler"),
            Err(error) => panic!("unexpected dispatch error: {error:?}"),
        }
        let error = expect_error(dispatch_inline(&face, &route(BodyPolicy::Empty), request()));
        assert_eq!(
            error,
            DispatchError::WrongExchange {
                body: BodyPolicy::Empty,
                got: ExchangeKind::Stream,
            }
        );
    }

    #[test]
    fn a_memory_response_is_rejected_on_a_streamed_route() {
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let error = expect_error(dispatch_inline(
            &face,
            &route(BodyPolicy::Streamed),
            request(),
        ));
        assert_eq!(
            error,
            DispatchError::WrongExchange {
                body: BodyPolicy::Streamed,
                got: ExchangeKind::Response,
            }
        );
    }

    #[test]
    fn a_blocking_route_can_run_on_the_calling_thread() {
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let route = Route::blocking(BodyPolicy::Empty);
        let exchange = dispatch_blocking_now(&face, &route, request())
            .expect("a blocking route must complete on the calling thread");
        let Exchange::Response(response) = exchange else {
            panic!("the fake face returned a memory response");
        };
        assert_eq!(response.status, 200);

        let error = expect_error(dispatch_blocking_now(
            &face,
            &Route::inline(BodyPolicy::Empty),
            request(),
        ));
        assert_eq!(error, DispatchError::UnsupportedDispatch(Dispatch::Inline));
    }

    #[test]
    fn a_blocking_route_is_not_dispatched_inline() {
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let error = expect_error(dispatch_inline(
            &face,
            &Route {
                body: BodyPolicy::Empty,
                dispatch: Dispatch::Blocking,
                public: false,
            },
            request(),
        ));
        assert_eq!(
            error,
            DispatchError::UnsupportedDispatch(Dispatch::Blocking)
        );
    }
}
