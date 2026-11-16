//! Inline dispatch and exchange validation for the S4 seam.

use crate::log;

use super::contract::{
    BodyPolicy, CompletionSlot, Dispatch, Exchange, ExchangeKind, Face, Request, Route,
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

/// Run one inline face request and take its exactly-once result.
pub fn dispatch_inline(
    face: &dyn Face,
    route: &Route,
    request: Request,
) -> Result<Exchange, DispatchError> {
    if route.dispatch != Dispatch::Inline {
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
        fn on_event(
            &mut self,
            _event: StreamEvent<'_>,
            _out: &mut OutBuffer<'_>,
        ) -> StreamStep {
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
    fn a_blocking_route_is_not_dispatched_inline() {
        let face = FakeFace {
            behavior: Behavior::Respond,
        };
        let error = expect_error(dispatch_inline(
            &face,
            &Route {
                body: BodyPolicy::Empty,
                dispatch: Dispatch::Blocking,
            },
            request(),
        ));
        assert_eq!(
            error,
            DispatchError::UnsupportedDispatch(Dispatch::Blocking)
        );
    }
}
