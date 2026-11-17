//! Modern MCP (2026-07-28) session vocabulary shared by both bindings.
//!
//! `Served`, the protocol-version check and the `_meta` validation live in the
//! server layer so the HTTP transport (`server::mcp::transport`) and the stdio
//! bridge (`mcp`) can share them without the transport reaching up into the
//! face. Face modules import them downward (face → server), the same way they
//! already import `server::mcp::result`.

use serde_json::{json, Value};

use crate::rpc;

/// The modern MCP protocol revision this daemon speaks.
pub const MODERN_VERSION: &str = "2026-07-28";

/// What a modern request produced. `Unreachable` still carries a reply for the
/// client, but the control channel is gone: the stdio bridge sends it and exits
/// 1; HTTP sends the same JSON-RPC envelope and stays up.
pub enum Served {
    Respond(rpc::Response),
    Unreachable(rpc::Response),
}

impl Served {
    pub(crate) fn new(response: rpc::Response, fatal: bool) -> Self {
        if fatal {
            Served::Unreachable(response)
        } else {
            Served::Respond(response)
        }
    }
}

pub(crate) fn validate_meta(id: Option<Value>, params: &Value) -> Result<(), rpc::Response> {
    let Some(meta) = params.get("_meta").and_then(Value::as_object) else {
        return Err(rpc::err(id, -32602, "missing _meta"));
    };
    let Some(version) = meta
        .get("io.modelcontextprotocol/protocolVersion")
        .and_then(Value::as_str)
    else {
        return Err(rpc::err(
            id,
            -32602,
            "missing io.modelcontextprotocol/protocolVersion",
        ));
    };
    if !meta
        .get("io.modelcontextprotocol/clientCapabilities")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        return Err(rpc::err(
            id,
            -32602,
            "missing io.modelcontextprotocol/clientCapabilities",
        ));
    }
    if version != MODERN_VERSION {
        return Err(rpc::err_data(
            id,
            -32022,
            "Unsupported protocol version",
            json!({ "supported": [MODERN_VERSION], "requested": version }),
        ));
    }
    Ok(())
}
