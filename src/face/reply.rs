//! Response builders for the management face: a value the face already has,
//! turned into the HTTP reply the caller expects.
//!
//! The route table (`face/routes.rs`), the macro tables and handlers
//! (`face/api.rs`) and these builders are three separate jobs; keeping the
//! builders here keeps each file inside the size budget.

use std::path::Path;

use serde_json::{json, Value};

use crate::kernel::failure::ProblemError;
use crate::platform::paths::Paths;
use crate::server::http::model as http;
use crate::server::http::Response;

use super::support;

pub(crate) fn decode(value: &str) -> String {
    http::percent_decode(value, true)
}

pub(crate) fn cap_log(mut text: String, log_max_bytes: u64) -> String {
    if text.len() <= log_max_bytes as usize {
        return text;
    }
    let mut start = text.len() - log_max_bytes as usize;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    if let Some(newline) = text[start..].find('\n') {
        // Keep complete lines when possible; a single line longer than the
        // budget keeps its tail rather than becoming empty.
        let candidate = start + newline + 1;
        if candidate < text.len() {
            start = candidate;
        }
    }
    text.drain(..start);
    text
}

pub(crate) fn error_response(status: u16, code: &str, message: &str) -> Response {
    Response::json(
        status,
        &json!({ "error": { "code": code, "message": message } }),
    )
}

pub(crate) fn rpc_error(failure: support::RpcFailure) -> Response {
    let (status, code, message) = failure;
    error_response(status, code, &message)
}

/// One projection for every typed management failure: the category raised
/// where the operation failed decides the status (#151).
pub(crate) fn problem_error_response(error: &ProblemError) -> Response {
    let status = support::status_for_problem_error(error);
    error_response(status, support::code_for(status), error.detail())
}

pub(crate) fn method_not_allowed(allow: &str) -> Response {
    Response::json(
        405,
        &json!({
            "error": {
                "code": "method_not_allowed",
                "message": format!("method not allowed; use {}", allow),
            }
        }),
    )
    .with_header("Allow", allow)
}

pub(crate) fn summary_response(status: Value) -> Response {
    Response::json(
        200,
        &json!({
            "summary": status.get("summary").cloned().unwrap_or(Value::Null)
        }),
    )
}

pub(crate) fn download_response(filename: &str, data: &[u8]) -> Response {
    Response::new(200, "application/octet-stream", data)
        .with_header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .with_header("Cache-Control", "no-store")
}

pub(crate) fn safe_filename(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_')
        })
        .collect()
}

pub(crate) fn project_service(status: &Value, paths: &Paths) -> Option<Value> {
    let has_config = status
        .pointer("/details/config")
        .and_then(Value::as_str)
        .is_some();
    let has_ui = status
        .get("summary")
        .and_then(|summary| summary.get("id"))
        .and_then(Value::as_str)
        .map(|id| crate::manage::web_assets(paths, id).is_some())
        .unwrap_or(false);
    let root_index = status
        .pointer("/summary/installDir")
        .and_then(Value::as_str)
        .and_then(|install_dir| root_index(paths, Path::new(install_dir)));
    let mut summary = status.get("summary").cloned()?;
    if let Some(object) = summary.as_object_mut() {
        object.insert("hasConfig".into(), Value::Bool(has_config));
        object.insert("hasUI".into(), Value::Bool(has_ui));
        if let Some(index) = root_index {
            object.insert("installRootIndex".into(), json!(index));
        }
    }
    Some(summary)
}

/// The 1-based position of the configured root that holds `install_dir`, in the
/// order the settings list shows (#313). Nested roots resolve to the most
/// specific one, so `/data` listed before `/data/plugins` does not claim every
/// plugin under the latter.
fn root_index(paths: &Paths, install_dir: &Path) -> Option<usize> {
    paths
        .plugin_roots
        .iter()
        .enumerate()
        .filter(|(_, root)| install_dir.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())
        .map(|(index, _)| index + 1)
}
