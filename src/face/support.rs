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

use crate::channel;
use crate::http;
use crate::package;
use crate::server::http::{CancelToken, JobBudget, Request};

/// The control-channel budget a face request carries.
///
/// A blocking route always has a deadline stamped by the server; an inline
/// route has none and keeps the CLI-style round trip. \`cancel\` is the pool's
/// per-job token, flipped when the client disconnects or the pool shuts down.
#[derive(Clone, Debug, Default)]
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
}

/// A daemon RPC failure: status, error code and message. The caller renders
/// the wire envelope.
pub type RpcFailure = (u16, &'static str, String);

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
            (
                504,
                "timeout",
                format!("svcd control request did not complete: {}", cause),
            )
        } else {
            (500, "internal", format!("svcd unreachable: {}", cause))
        }
    })?;
    if let Some(error) = response.error {
        let status = status_for_error(&error.message);
        return Err((status, code_for(status), error.message));
    }
    response
        .result
        .ok_or_else(|| (500, "internal", "empty daemon response".to_string()))
}

/// The rendered-bytes form used by the legacy/fallback router and the install
/// stream.
pub fn rpc(
    config: &Path,
    method: &str,
    params: Value,
    budget: &CallBudget,
) -> Result<Value, Vec<u8>> {
    rpc_result(config, method, params, budget)
        .map_err(|(status, code, message)| error(status, code, &message))
}

pub fn require_plugin_result(
    config: &Path,
    id: &str,
    budget: &CallBudget,
) -> Result<(), (u16, &'static str, String)> {
    if !package::valid_name(id) {
        return Err((404, "not_found", format!("unknown plugin: {}", id)));
    }
    let status = rpc_result(config, "svc.status", json!({ "id": id }), budget)?;
    let kind = status
        .pointer("/summary/kind")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind == "plugin" {
        Ok(())
    } else {
        Err((404, "not_found", format!("not a plugin: {}", id)))
    }
}

pub fn config_error_status(message: &str) -> (u16, &'static str, String) {
    let lower = message.to_ascii_lowercase();
    let status = if lower.contains("unknown plugin") {
        404
    } else if lower.contains("exceeds 1 mib") {
        413
    } else if lower.contains("configcheck") {
        409
    } else {
        500
    };
    (status, code_for(status), message.to_string())
}

pub fn map_error(message: &str) -> Vec<u8> {
    let (status, code, message) = map_error_status(message);
    error(status, code, &message)
}

pub fn map_error_status(message: &str) -> (u16, &'static str, String) {
    let status = status_for_error(message);
    (status, code_for(status), message.to_string())
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

fn status_for_error(message: &str) -> u16 {
    let lower = message.to_ascii_lowercase();
    if lower.contains("timed out") || lower.contains("timeout") {
        504
    } else if lower.contains("exceeds") || lower.contains("too large") {
        413
    } else if lower.contains("unknown keyid") || lower.contains("trust root") {
        400
    } else if lower.contains("id is a core service") {
        404
    } else if lower.contains("unknown") || lower.contains("not found") {
        404
    } else if lower.contains("configcheck") {
        409
    } else if lower.contains("not a plugin")
        || lower.contains("core service")
        || lower.contains("conflict")
        || lower.contains("cannot be enabled")
    {
        409
    } else if lower.contains("signature")
        || lower.contains("platform")
        || lower.contains("protocol")
        || lower.contains("unsafe")
        || lower.contains("setuid")
        || lower.contains("hooks")
        || lower.contains("invalid")
        || lower.contains("not declared")
        || lower.contains("missing")
        || lower.contains("public key")
    {
        400
    } else {
        500
    }
}

pub(crate) fn code_for(status: u16) -> &'static str {
    match status {
        400 => "bad_request",
        403 => "forbidden",
        404 => "not_found",
        409 => "conflict",
        413 => "payload_too_large",
        504 => "timeout",
        _ => "internal",
    }
}

pub(crate) fn error(status: u16, code: &str, message: &str) -> Vec<u8> {
    http::json_error(status, code, message)
}

pub(crate) fn decode(value: &str) -> String {
    http::percent_decode(value, true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
