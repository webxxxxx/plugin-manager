//! `POST /mcp`: the modern-only (2026-07-28) Streamable HTTP binding.
//!
//! The transport owns authentication, mirrored-header validation and HTTP
//! status mapping. It never reaches up into the face: the caller passes the
//! endpoint projection in as `serve` (for the daemon that is
//! `mcp::serve_modern`, the same code the stdio bridge uses). There is no
//! session and no SSE: every request is one POST answered with one JSON object.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::{json, Value};

use crate::server::http::model as http;
use crate::server::mcp::session::{validate_meta, Served};
use crate::wire::message::rpc;

const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";

/// One HTTP request view. The daemon's own `HttpRequest` stays private to
/// `api`; this keeps the MCP endpoint's input surface small.
pub struct Request<'a> {
    pub method: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

/// `POST /mcp`. `serve` is the face's MCP endpoint projection; `password_hash`
/// is the configured `auth_passwd`, and `None` means no Bearer value can match,
/// so the endpoint stays closed.
pub fn handle(
    serve: &dyn Fn(&rpc::Request) -> Served,
    request: &Request<'_>,
    password_hash: Option<&str>,
) -> Vec<u8> {
    if request.method != "POST" {
        return http::method_not_allowed("POST");
    }
    if !http::same_origin(request.headers) {
        return http::json_error(403, "forbidden", "cross-origin calls are not allowed");
    }
    if let Err(response) = authorize(request.headers, password_hash) {
        return response;
    }
    let headers = request.headers;
    let text = match std::str::from_utf8(request.body) {
        Ok(text) => text,
        Err(_) => return reply(400, rpc::err(None, -32700, "parse error")),
    };
    let request = match rpc::parse(text) {
        Ok(request) => request,
        Err(response) => return reply(400, response),
    };
    if request.id.is_none() {
        // The modern core protocol defines no client-to-server notification
        // over Streamable HTTP; an accepted notification POST gets 202 with no
        // body. (Cancellation is closing the response stream, not a message.)
        return http::response(202, "application/json", b"");
    }
    // The transport mirrors body fields into headers so intermediaries can
    // route without parsing JSON; every mismatch is a 400 HeaderMismatch.
    if let Err(response) = validate_headers(headers, &request) {
        return reply(400, response);
    }
    // Both variants still carry a JSON-RPC envelope. A lost control channel
    // is a -32603 (or `isError` result for tools/call) exactly as on stdio;
    // the transport only remaps status codes the protocol defines.
    let response = match serve(&request) {
        Served::Respond(response) | Served::Unreachable(response) => response,
    };
    let status = if response
        .error
        .as_ref()
        .map(|error| error.code == -32601)
        .unwrap_or(false)
    {
        // The transport reserves 404 for an unknown RPC method. The JSON-RPC
        // body is still the normal MethodNotFoundError.
        404
    } else {
        200
    };
    reply(status, response)
}

/// A `tools/call` that should be answered with an SSE stream because the
/// client asked for `text/event-stream` and supplied a progress token.
pub struct StreamingCall {
    pub id: Option<Value>,
    pub name: String,
    pub arguments: Value,
    pub progress_token: Value,
}

/// Validate the same transport contract as `handle`, then decide whether this
/// request should stream. `Ok(None)` means the caller should use the normal
/// one-JSON response path.
pub fn prepare_streaming(
    request: &Request<'_>,
    password_hash: Option<&str>,
    prefer_streaming: bool,
) -> Result<Option<StreamingCall>, Vec<u8>> {
    if request.method != "POST" {
        return Ok(None);
    }
    if !http::same_origin(request.headers) {
        return Err(http::json_error(
            403,
            "forbidden",
            "cross-origin calls are not allowed",
        ));
    }
    authorize(request.headers, password_hash)?;
    if !stream_requested(
        http::header(request.headers, "accept").unwrap_or(""),
        prefer_streaming,
    ) {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(request.body) else {
        return Err(reply(400, rpc::err(None, -32700, "parse error")));
    };
    let parsed = match rpc::parse(text) {
        Ok(parsed) => parsed,
        Err(response) => return Err(reply(400, response)),
    };
    if parsed.id.is_none() {
        return Ok(None);
    }
    if let Err(response) = validate_headers(request.headers, &parsed) {
        return Err(reply(400, response));
    }
    if parsed.method != "tools/call" {
        return Ok(None);
    }
    let Some(progress_token) = parsed
        .params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
        .cloned()
    else {
        return Ok(None);
    };
    let Some(name) = parsed
        .params
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(reply(
            400,
            rpc::err(parsed.id, -32602, "tools/call requires params.name"),
        ));
    };
    let arguments = parsed
        .params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok(Some(StreamingCall {
        id: parsed.id,
        name,
        arguments,
        progress_token,
    }))
}

/// Whether one Accept header should get a stream.
///
/// A client that can take a single JSON body gets one unless the operator
/// asked for streams: that body skips the stream slot and the reactor hop.
/// A client that only accepts text/event-stream always streams, so the
/// protocol stays usable from SSE-only clients.
fn stream_requested(accept: &str, prefer_streaming: bool) -> bool {
    let accept = accept.to_ascii_lowercase();
    let wants_stream = accept.contains("text/event-stream");
    let can_take_json =
        accept.is_empty() || accept.contains("application/json") || accept.contains("*/*");
    wants_stream && (prefer_streaming || !can_take_json)
}

fn reply(status: u16, response: rpc::Response) -> Vec<u8> {
    http::response(
        status,
        "application/json",
        rpc::to_line(&response).as_bytes(),
    )
}

#[allow(clippy::result_large_err)] // Same protocol-value error path as validate_meta.
fn validate_headers(
    headers: &[(String, String)],
    request: &rpc::Request,
) -> Result<(), rpc::Response> {
    let id = request.id.clone();
    let header_version = http::header(headers, "mcp-protocol-version")
        .ok_or_else(|| header_mismatch(id.clone(), "missing MCP-Protocol-Version header"))?;
    let header_method = http::header(headers, "mcp-method")
        .ok_or_else(|| header_mismatch(id.clone(), "missing Mcp-Method header"))?;
    let body_version = request
        .params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get(VERSION_KEY))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(body_version) = body_version else {
        // Give the same -32602 the stdio bridge gives for missing _meta.
        return Err(validate_meta(id.clone(), &request.params)
            .err()
            .unwrap_or_else(|| rpc::err(id, -32602, "missing _meta")));
    };
    if header_version != body_version {
        return Err(header_mismatch(
            id,
            format!(
                "MCP-Protocol-Version header '{}' does not match body value '{}'",
                header_version, body_version
            ),
        ));
    }
    if header_method != request.method {
        return Err(header_mismatch(
            request.id.clone(),
            format!(
                "Mcp-Method header '{}' does not match body value '{}'",
                header_method, request.method
            ),
        ));
    }
    if request.method == "tools/call" {
        let header_name = http::header(headers, "mcp-name").ok_or_else(|| {
            header_mismatch(request.id.clone(), "missing Mcp-Name header for tools/call")
        })?;
        let Some(body_name) = request.params.get("name").and_then(Value::as_str) else {
            return Err(header_mismatch(
                request.id.clone(),
                "Mcp-Name header has no matching params.name",
            ));
        };
        let decoded = decode_mcp_name(header_name);
        if decoded != body_name {
            return Err(header_mismatch(
                request.id.clone(),
                format!(
                    "Mcp-Name header value '{}' does not match body value '{}'",
                    decoded, body_name
                ),
            ));
        }
    }
    // Finally the request-level _meta contract: client capabilities are
    // required and only 2026-07-28 is implemented. Both map to 400 here.
    validate_meta(request.id.clone(), &request.params)?;
    Ok(())
}

fn header_mismatch(id: Option<Value>, detail: impl Into<String>) -> rpc::Response {
    rpc::err(id, -32020, &format!("Header mismatch: {}", detail.into()))
}

/// `Mcp-Name` may carry a Base64 sentinel for names that are not header-safe.
/// Anything that does not decode falls back to the literal value so the
/// comparison still fails loudly rather than panicking.
fn decode_mcp_name(value: &str) -> String {
    let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|rest| rest.strip_suffix("?="))
    else {
        return value.to_string();
    };
    BASE64
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_else(|| value.to_string())
}

/// Bearer only. Cookies are deliberately not consulted: `/mcp` has no CSRF
/// surface because a browser never attaches an Authorization header.
fn authorize(headers: &[(String, String)], password_hash: Option<&str>) -> Result<(), Vec<u8>> {
    let Some(value) = http::header(headers, "authorization") else {
        return Err(unauthorized("Bearer password required"));
    };
    let Some(bearer) = value
        .split_once(' ')
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, password)| password.trim())
    else {
        return Err(unauthorized("Bearer authorization is required"));
    };
    match password_hash {
        Some(hash) if crate::platform::auth::verify_password(bearer, hash) => Ok(()),
        _ => Err(http::json_error(
            403,
            "forbidden",
            "invalid bearer password",
        )),
    }
}

fn unauthorized(message: &str) -> Vec<u8> {
    http::response_with_headers(
        401,
        "application/json",
        &[("WWW-Authenticate".to_string(), "Bearer".to_string())],
        &serde_json::to_vec(&json!({
            "error": { "code": "unauthorized", "message": message }
        }))
        .unwrap_or_default(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_json_body_is_the_default_even_when_the_client_accepts_sse() {
        // MCP clients send both per spec; the buffered body is cheaper.
        assert!(!stream_requested(
            "application/json, text/event-stream",
            false
        ));
        assert!(!stream_requested("application/json", false));
        assert!(!stream_requested("", false));
        assert!(!stream_requested("*/*", false));
    }

    #[test]
    fn an_sse_only_client_still_streams() {
        assert!(stream_requested("text/event-stream", false));
    }

    #[test]
    fn the_operator_can_prefer_streaming() {
        assert!(stream_requested(
            "application/json, text/event-stream",
            true
        ));
        assert!(!stream_requested("application/json", true));
    }

    #[test]
    fn mcp_name_decodes_the_base64_sentinel() {
        assert_eq!(decode_mcp_name("demo.echo"), "demo.echo");
        assert_eq!(
            decode_mcp_name("=?base64?SGVsbG8sIOS4lueVjA==?="),
            "Hello, 世界"
        );
        assert_eq!(
            decode_mcp_name("=?base64?not-base64?="),
            "=?base64?not-base64?="
        );
    }
}
