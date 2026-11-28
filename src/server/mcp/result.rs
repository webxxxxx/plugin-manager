//! Modern MCP result projection.
//!
//! The stdio bridge and the HTTP transport finish the same `tools/call`
//! result. Keeping the decoration in the server layer lets the face modules
//! import it (face → server) instead of making the server session reach up
//! into the face for its final response.

use serde_json::{json, Value};

use crate::wire::message::rpc;

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

/// Merge keys into a result's `_meta` instead of replacing it. The face may
/// have put the call number there (#347), and the modern decoration still owes
/// the client `serverInfo`.
pub(crate) fn merge_meta(result: &mut Value, extra: Value) {
    let Value::Object(extra) = extra else {
        return;
    };
    match result.get_mut("_meta") {
        Some(Value::Object(meta)) => {
            for (key, value) in extra {
                meta.insert(key, value);
            }
        }
        _ => {
            result["_meta"] = Value::Object(extra);
        }
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
    merge_meta(&mut result, server_meta());
    rpc::ok(id, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The modern decoration used to replace `_meta` outright, which dropped the
    /// call number the face had put there (#347).
    #[test]
    fn finishing_a_modern_result_keeps_the_meta_the_face_wrote() {
        let result = json!({
            "content": [{ "type": "text", "text": "boom" }],
            "isError": true,
            "_meta": { "io.svcd/call": 7 },
        });
        let response = finish_modern_tool_call(Some(json!(1)), rpc::ok(Some(json!(1)), result));
        let result = response.result.expect("result");
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["_meta"]["io.svcd/call"], 7);
        assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());
    }

    #[test]
    fn finishing_a_result_without_meta_still_adds_server_info() {
        let result = json!({ "content": [], "isError": false });
        let response = finish_modern_tool_call(Some(json!(1)), rpc::ok(Some(json!(1)), result));
        let result = response.result.expect("result");
        assert!(result["_meta"]["io.modelcontextprotocol/serverInfo"].is_object());
    }
}
