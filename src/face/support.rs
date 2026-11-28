//! Helpers shared by the management face and the composition root.
//!
//! These used to live in \`api.rs\`, which made the face call the composition
//! root and hid a \`face -> api\` cycle from the dependency layer check. The
//! helpers depend only on the wire/control channel, HTTP projection and the
//! plugin layout, so they belong to the face. The Windows fallback in
//! \`api.rs\` uses them from above, which is the allowed compose -> face
//! direction.

use std::path::Path;
use std::time::Instant;

use serde_json::{json, Value};

use crate::kernel::call::CallId;
use crate::kernel::failure::{Class, Problem, ProblemError};
use crate::server::http::model as http;
use crate::server::http::{CancelToken, JobBudget, Request};
use crate::wire::control::channel;
use crate::wire::message::rpc;

/// The control-channel budget a face request carries.
///
/// A blocking route always has a deadline stamped by the server; an inline
/// route has none and keeps the CLI-style round trip. \`cancel\` is the pool's
/// per-job token, flipped when the client disconnects or the pool shuts down.
#[derive(Clone, Debug)]
pub struct CallBudget {
    pub deadline: Option<Instant>,
    pub cancel: Option<CancelToken>,
}

impl CallBudget {
    pub fn from_request(request: &Request) -> CallBudget {
        CallBudget {
            deadline: request.deadline,
            cancel: request.cancel.clone(),
        }
    }

    /// A stream job's budget: the pool's disconnect token plus the connection's
    /// no-progress deadline. Stream jobs that call the control channel use
    /// this; a deadline shorter than the connection's would report a failure
    /// the client could no longer receive.
    pub fn from_job(job: &JobBudget) -> CallBudget {
        CallBudget {
            deadline: Some(job.deadline),
            cancel: Some(job.cancel.clone()),
        }
    }

    /// A short follow-up call after a successful long one. It keeps the
    /// disconnect token but gets its own management budget, so an install that
    /// consumed the stream deadline can still disable a plugin instead of
    /// starting the follow-up with an expired deadline.
    pub fn follow_up(&self, control_timeout: std::time::Duration) -> CallBudget {
        CallBudget {
            deadline: Some(Instant::now() + control_timeout),
            cancel: self.cancel.clone(),
        }
    }
}

/// The response header that carries the daemon's call number on a failed HTTP
/// request (#346). The body keeps its `{code, message}` shape.
pub(crate) const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// A daemon RPC failure: status, error code, message, and the call number when
/// the daemon named one. The caller renders the wire envelope.
#[derive(Debug)]
pub struct RpcFailure {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    /// The daemon's call number for this request (`error.data.call`). A timeout
    /// or a disconnect has none: the request never reached the daemon's counter.
    pub call: Option<CallId>,
}

impl RpcFailure {
    /// A failure the daemon did not name.
    fn unnamed(status: u16, code: &'static str, message: impl Into<String>) -> RpcFailure {
        RpcFailure {
            status,
            code,
            message: message.into(),
            call: None,
        }
    }

    /// The rendered error envelope, with the call number as a response header
    /// when the daemon named one. The byte path (install and import streams)
    /// uses this; the `Response` path renders the same header in
    /// `reply::rpc_error`.
    pub fn http_error(&self) -> Vec<u8> {
        let headers = match self.call {
            Some(call) => vec![(REQUEST_ID_HEADER.to_string(), call.to_string())],
            None => Vec::new(),
        };
        http::json_error_with_headers(self.status, self.code, &self.message, &headers)
    }
}

/// One control-channel round trip under \`budget\`. A timeout or a disconnect is
/// a 504 \`timeout\`, never a 503: 503 stays reserved for the blocking queue
/// being full.
pub fn rpc_result(
    config: &Path,
    method: &str,
    params: Value,
    budget: &CallBudget,
) -> Result<Value, RpcFailure> {
    let response = match budget.deadline {
        Some(deadline) => channel::request_within(config, method, params, deadline, || {
            budget
                .cancel
                .as_ref()
                .map(CancelToken::is_cancelled)
                .unwrap_or(false)
        }),
        None => channel::request(config, method, params),
    }
    .map_err(|cause| {
        if matches!(
            cause.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
        ) {
            RpcFailure::unnamed(
                504,
                "timeout",
                format!("svcd control request did not complete: {}", cause),
            )
        } else {
            RpcFailure::unnamed(500, "internal", format!("svcd unreachable: {}", cause))
        }
    })?;
    project_response(response)
}

fn project_response(response: rpc::Response) -> Result<Value, RpcFailure> {
    if let Some(error) = response.error {
        let status = status_for_rpc_error(&error);
        // The daemon names the request in `error.data.call`; the body stays
        // `{code, message}` so the number travels in a response header (#346).
        let call = error
            .data
            .as_ref()
            .and_then(|data| data.get("call"))
            .and_then(Value::as_u64)
            .map(CallId::from_raw);
        return Err(RpcFailure {
            status,
            code: code_for(status),
            message: error.message,
            call,
        });
    }
    response
        .result
        .ok_or_else(|| RpcFailure::unnamed(500, "internal", "empty daemon response"))
}

/// The rendered-bytes form used by the legacy/fallback router and the install
/// stream.
pub fn rpc(
    config: &Path,
    method: &str,
    params: Value,
    budget: &CallBudget,
) -> Result<Value, Vec<u8>> {
    rpc_result(config, method, params, budget).map_err(|failure| failure.http_error())
}

pub fn require_plugin_result(
    config: &Path,
    id: &str,
    budget: &CallBudget,
) -> Result<(), RpcFailure> {
    if !crate::kernel::registry::valid_id(id) {
        return Err(RpcFailure::unnamed(
            404,
            "not_found",
            format!("unknown plugin: {}", id),
        ));
    }
    let status = rpc_result(config, "svc.status", json!({ "id": id }), budget)?;
    let kind = status
        .pointer("/summary/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "plugin" {
        Ok(())
    } else {
        Err(RpcFailure::unnamed(
            404,
            "not_found",
            format!("not a plugin: {}", id),
        ))
    }
}

pub fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(decode(value))
        } else {
            None
        }
    })
}

pub fn query_bool(query: &str, key: &str) -> Option<bool> {
    query_value(query, key).map(|value| value == "true" || value == "1")
}

/// Read the structured category first. A missing or unrecognized category is
/// `Internal`: the projection never falls back to parsing `message`, so an old
/// or hand-written error cannot pick a status by accident.
fn status_for_rpc_error(error: &rpc::Error) -> u16 {
    if let Some(problem) = Problem::from_error_data(error.data.as_ref()) {
        return status_for_problem(problem);
    }
    let Some(data) = error.data.as_ref() else {
        return 500;
    };
    if let Some(class) = data.get("class").and_then(Value::as_str) {
        return Class::from_str(class).map(status_for_class).unwrap_or(500);
    }
    500
}

fn status_for_problem(problem: Problem) -> u16 {
    match problem {
        Problem::UnknownService
        | Problem::UnknownPlugin
        | Problem::UnknownStrategy
        | Problem::CoreService => 404,
        Problem::NotAPlugin | Problem::Conflict => 409,
        Problem::Invalid | Problem::UnsupportedPlatform | Problem::MissingDependency => 400,
        Problem::TooLarge => 413,
        // WebDAV's code for "the volume cannot hold it": an operator action
        // (free space, uninstall an old version), not a server bug.
        Problem::InsufficientSpace => 507,
        Problem::Timeout => 504,
        Problem::Internal => 500,
    }
}

/// The status a typed management failure projects to. Callers never inspect
/// the detail: the category raised where the failure happened decides (#151).
pub(crate) fn status_for_problem_error(error: &ProblemError) -> u16 {
    status_for_problem(error.problem)
}

/// A typed management refusal as the HTTP error envelope every face route
/// writes. One implementation, so no route re-derives the status.
pub(crate) fn problem_response(refusal: &ProblemError) -> Vec<u8> {
    let status = status_for_problem_error(refusal);
    http::json_error_with_data(
        status,
        code_for(status),
        refusal.detail(),
        refusal.shortfall(),
    )
}

/// A storage refusal as the HTTP error envelope (see [`status_for_storage`]).
pub(crate) fn storage_response(refusal: &crate::platform::storage::Error) -> Vec<u8> {
    let status = status_for_storage(refusal.kind);
    error(status, code_for(status), &refusal.detail)
}

/// The status a storage failure projects to. Storage keeps its own small
/// taxonomy (it sits below the kernel), so this maps it onto the one management
/// table; `NotFound` is the only category that table cannot name.
pub(crate) fn status_for_storage(kind: crate::platform::storage::Kind) -> u16 {
    match kind {
        crate::platform::storage::Kind::NotFound => 404,
        crate::platform::storage::Kind::Conflict => status_for_problem(Problem::Conflict),
        crate::platform::storage::Kind::Invalid => status_for_problem(Problem::Invalid),
        crate::platform::storage::Kind::TooLarge => status_for_problem(Problem::TooLarge),
        crate::platform::storage::Kind::Internal => status_for_problem(Problem::Internal),
    }
}

/// No face route produces `data.class` today (only MCP tool errors do), but a
/// future control client may. Keep the statuses the old matcher gave those
/// details; `Busy` stays 500 (503 is reserved for the blocking queue).
fn status_for_class(class: Class) -> u16 {
    match class {
        Class::FrameTooLarge => 413,
        Class::Timeout => 504,
        Class::MissingDependency => 400,
        _ => 500,
    }
}

pub(crate) use crate::server::http::model::code_for;

pub(crate) fn error(status: u16, code: &str, message: &str) -> Vec<u8> {
    http::json_error(status, code, message)
}

/// The `Expect: 100-continue` step shared by the package and blob upload
/// streams: send the interim response once, then ask for either a read (it
/// fitted) or another writable event (the output buffer was full).
pub(crate) fn expect_continue(
    out: &mut crate::server::http::OutBuffer<'_>,
    sent: &mut bool,
    expected: bool,
) -> Option<crate::server::http::StreamStep> {
    if *sent || !expected {
        return None;
    }
    if out.write(b"HTTP/1.1 100 Continue\r\n\r\n") {
        *sent = true;
        return Some(crate::server::http::StreamStep::wait(
            crate::server::http::StreamAction::NeedRead,
        ));
    }
    Some(crate::server::http::StreamStep::wait(
        crate::server::http::StreamAction::NeedWrite,
    ))
}

pub(crate) fn decode(value: &str) -> String {
    http::percent_decode(value, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::failure::{RootRoom, SpaceShortfall};

    /// The face-local HTTP table, one row per management category. These are
    /// the codes the old message matchers returned (#151); the category is now
    /// the only input.
    #[test]
    fn every_management_category_keeps_its_http_status() {
        let cases = [
            (Problem::UnknownService, 404),
            (Problem::UnknownPlugin, 404),
            (Problem::UnknownStrategy, 404),
            (Problem::CoreService, 404),
            (Problem::NotAPlugin, 409),
            (Problem::Conflict, 409),
            (Problem::Invalid, 400),
            (Problem::UnsupportedPlatform, 400),
            (Problem::MissingDependency, 400),
            (Problem::TooLarge, 413),
            (Problem::InsufficientSpace, 507),
            (Problem::Timeout, 504),
            (Problem::Internal, 500),
        ];
        for (problem, status) in cases {
            assert_eq!(status_for_problem(problem), status, "{}", problem.as_str());
            let error = problem.error("detail");
            assert_eq!(
                status_for_problem_error(&error),
                status,
                "{}",
                problem.as_str()
            );
        }
    }

    /// The 507 body is a contract, not a detail: the console keys its recovery
    /// dialog on these exact fields (#328, #330). Pin the whole JSON, so a
    /// refactor of how the payload is stored cannot quietly rename a key, drop
    /// one, or turn an unmeasured root into zero bytes.
    #[test]
    fn a_space_refusal_body_is_pinned_byte_for_byte() {
        let refusal = Problem::InsufficientSpace
            .error("need 10 bytes")
            .with_shortfall(SpaceShortfall {
                id: "demo".into(),
                need: 10,
                installed: 4,
                root: Some("/data/plugins".into()),
                roots: vec![
                    RootRoom {
                        root: "/data/plugins".into(),
                        free: Some(6),
                    },
                    RootRoom {
                        root: "/mnt/sd".into(),
                        free: None,
                    },
                ],
            });
        let text = String::from_utf8(problem_response(&refusal)).expect("utf8");
        let (head, body) = text.split_once("\r\n\r\n").expect("a body");
        assert!(head.starts_with("HTTP/1.1 507 "), "{head}");
        assert_eq!(
            body,
            r#"{"error":{"code":"insufficient_space","data":{"id":"demo","installed":4,"need":10,"root":"/data/plugins","roots":[{"free":6,"root":"/data/plugins"},{"free":null,"root":"/mnt/sd"}]},"message":"need 10 bytes"}}"#
        );

        // A refusal that never picked a root (the automatic choice found no
        // volume big enough) says `root: null`: there is nothing to uninstall,
        // and the dialog must not offer one.
        let fresh = Problem::InsufficientSpace
            .error("need 10 bytes")
            .with_shortfall(SpaceShortfall {
                id: "demo".into(),
                need: 10,
                installed: 0,
                root: None,
                roots: Vec::new(),
            });
        let text = String::from_utf8(problem_response(&fresh)).expect("utf8");
        let (_, body) = text.split_once("\r\n\r\n").expect("a body");
        assert_eq!(
            body,
            r#"{"error":{"code":"insufficient_space","data":{"id":"demo","installed":0,"need":10,"root":null,"roots":[]},"message":"need 10 bytes"}}"#
        );

        // A refusal without a payload keeps the envelope it always had.
        let plain =
            String::from_utf8(problem_response(&Problem::Invalid.error("no"))).expect("utf8");
        assert!(!plain.contains("\"data\""), "a payload is opt-in: {plain}");
    }

    /// The storage taxonomy reuses the management table and adds exactly one
    /// category of its own: a missing upload session is 404.
    #[test]
    fn a_storage_failure_projects_onto_the_same_table() {
        assert_eq!(
            status_for_storage(crate::platform::storage::Kind::Conflict),
            409
        );
        assert_eq!(
            status_for_storage(crate::platform::storage::Kind::Invalid),
            400
        );
        assert_eq!(
            status_for_storage(crate::platform::storage::Kind::TooLarge),
            413
        );
        assert_eq!(
            status_for_storage(crate::platform::storage::Kind::Internal),
            500
        );
        assert_eq!(
            status_for_storage(crate::platform::storage::Kind::NotFound),
            404
        );
    }

    /// The stream-job handoff must carry both halves of the budget: a job that
    /// only got a deadline would keep a disconnected install alive, and one
    /// that only got the token would keep a stuck supervisor forever.
    #[test]
    fn a_stream_job_budget_carries_the_pool_token_and_deadline() {
        let job = JobBudget::for_test();
        let budget = CallBudget::from_job(&job);
        assert_eq!(budget.deadline, Some(job.deadline));
        assert!(budget.cancel.is_some());
        assert!(!budget.cancel.unwrap().is_cancelled());
    }

    fn rpc_error(message: &str, data: Option<Value>) -> rpc::Error {
        rpc::Error {
            code: -32000,
            message: message.into(),
            data,
        }
    }

    fn failure_response(error: rpc::Error) -> rpc::Response {
        rpc::Response {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            result: None,
            error: Some(error),
        }
    }

    #[test]
    fn a_daemon_failure_carries_the_call_number_the_daemon_named() {
        let failure = project_response(failure_response(rpc_error(
            "install failed",
            Some(json!({"call": 7, "kind": "internal"})),
        )))
        .expect_err("failure");
        assert_eq!(failure.call, Some(CallId::from_raw(7)));
        assert_eq!(failure.status, 500);
        assert_eq!(failure.message, "install failed");
    }

    #[test]
    fn a_failure_the_daemon_did_not_name_has_no_call_number() {
        let failure =
            project_response(failure_response(rpc_error("boom", None))).expect_err("failure");
        assert_eq!(failure.call, None);
    }

    #[test]
    fn a_named_failure_renders_the_call_in_the_request_id_header() {
        let bytes = RpcFailure {
            status: 500,
            code: "internal",
            message: "boom".into(),
            call: Some(CallId::from_raw(7)),
        }
        .http_error();
        let text = String::from_utf8(bytes).expect("utf8").to_ascii_lowercase();
        assert!(text.contains("x-request-id: 7\r\n"), "{text}");
    }

    #[test]
    fn an_unnamed_failure_renders_no_request_id_header() {
        let bytes = RpcFailure {
            status: 500,
            code: "internal",
            message: "boom".into(),
            call: None,
        }
        .http_error();
        let text = String::from_utf8(bytes).expect("utf8").to_ascii_lowercase();
        assert!(!text.contains("x-request-id"), "{text}");
    }

    #[test]
    fn the_control_projection_reads_the_wire_kind() {
        let response = |data: Option<Value>| rpc::Response {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            result: None,
            error: Some(rpc::Error {
                code: -32003,
                message: "start failed".into(),
                data,
            }),
        };
        let failure = project_response(response(Some(json!({"kind": "conflict"}))))
            .expect_err("structured conflict");
        assert_eq!(failure.status, 409);
        assert_eq!(failure.code, "conflict");
        assert_eq!(failure.message, "start failed");

        let failure = project_response(response(None)).expect_err("missing data");
        assert_eq!(failure.status, 500);
    }

    #[test]
    fn structured_problem_kinds_project_without_reading_the_message() {
        let cases = [
            ("unknown_service", 404),
            ("unknown_plugin", 404),
            ("unknown_strategy", 404),
            ("core_service", 404),
            ("not_a_plugin", 409),
            ("conflict", 409),
            ("invalid", 400),
            ("unsupported_platform", 400),
            ("missing_dependency", 400),
            ("too_large", 413),
            ("timeout", 504),
            ("internal", 500),
        ];
        for (kind, status) in cases {
            let error = rpc_error(
                "this message would project differently",
                Some(json!({"kind": kind})),
            );
            assert_eq!(status_for_rpc_error(&error), status, "{kind}");
        }
    }

    #[test]
    fn a_structured_kind_wins_over_a_misleading_message() {
        let error = rpc_error("unknown service: demo", Some(json!({"kind": "internal"})));
        assert_eq!(status_for_rpc_error(&error), 500);
    }

    #[test]
    fn class_data_and_missing_data_have_a_safe_projection() {
        let class = |name: &str| rpc_error("ignored", Some(json!({"class": name})));
        assert_eq!(status_for_rpc_error(&class("frame_too_large")), 413);
        assert_eq!(status_for_rpc_error(&class("timeout")), 504);
        assert_eq!(status_for_rpc_error(&class("missing_dependency")), 400);
        assert_eq!(status_for_rpc_error(&class("busy")), 500);

        assert_eq!(
            status_for_rpc_error(&rpc_error("unknown service: demo", None)),
            500
        );
        assert_eq!(
            status_for_rpc_error(&rpc_error("ignored", Some(json!({"kind": "no_such_kind"})))),
            500
        );
    }

    /// The install/import helpers call this exact function with the stream
    /// job budget. An expired budget must project 504 timeout instead of
    /// blocking on a supervisor that does not answer.
    #[cfg(unix)]
    #[test]
    fn an_expired_stream_budget_projects_504() {
        use std::os::unix::net::UnixListener;

        let root = crate::platform::test_support::temp_dir().join(format!(
            "svcd-support-budget-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("config dir");
        let listener = UnixListener::bind(root.join("svcd.sock")).expect("listener");
        let server = std::thread::spawn(move || {
            let (_stream, _) = listener.accept().expect("accept");
            std::thread::sleep(std::time::Duration::from_secs(1));
        });
        let budget = CallBudget {
            deadline: Some(Instant::now()),
            cancel: None,
        };
        let failure = rpc_result(&root, "svcd.ping", json!({}), &budget)
            .expect_err("an expired budget must fail");
        assert_eq!(failure.status, 504);
        assert_eq!(failure.code, "timeout");
        assert!(failure.message.contains("timed out"), "{}", failure.message);
        server.join().expect("server");
        let _ = std::fs::remove_dir_all(&root);
    }
}
