//! Modern MCP result projection.
//!
//! The stdio bridge and the HTTP transport finish the same `tools/call`
//! result. Keeping the decoration in the server layer lets the face modules
//! import it (face → server) instead of making the server session reach up
//! into the face for its final response.

use serde_json::{json, Value};

use crate::rpc;

pub(crate) const SERVER_NAME: &str = "svcd";

pub(crate) fn result_value(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

pub(crate) fn tool_error_result(message: &str) -> Value {
    result_value(message.to_string(), true)
}

pub(crate) fn error_response(id: Option<Value>, error: rpc::Error) -> rpc::Response {
    rpc::Response {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(error),
    }
}

/// A plugin may return an arbitrary JSON value. MCP tool results must be an
/// object, so wrap a scalar before decorating it.
pub(crate) fn tool_result_object(result: Value) -> Value {
    if result.is_object() {
        result
    } else {
        tool_error_result("plugin returned a non-object tool result")
    }
}

pub(crate) fn server_meta() -> Value {
    json!({
        "io.modelcontextprotocol/serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

/// Finish one modern `tools/call` result after a streaming transport has
/// already forwarded progress notifications.
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub fn finish_modern_tool_call(id: Option<Value>, response: rpc::Response) -> rpc::Response {
    if let Some(error) = response.error {
        return error_response(id, error);
    }
    let mut result = tool_result_object(response.result.unwrap_or_else(|| json!({})));
    result["resultType"] = json!("complete");
    result["_meta"] = server_meta();
    rpc::ok(id, result)
}
