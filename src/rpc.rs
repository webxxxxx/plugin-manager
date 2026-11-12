use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    /// Local management channel token; plugin stdio never sets it.
    #[serde(default)]
    pub auth: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Error {
    pub code: i32,
    pub message: String,
    /// Optional structured error payload, e.g. the supported versions for
    /// -32022 (UnsupportedProtocolVersion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

pub fn parse(line: &str) -> Result<Request, Response> {
    let request =
        serde_json::from_str::<Request>(line).map_err(|e| err(None, -32700, &e.to_string()))?;
    if request.jsonrpc != "2.0" {
        return Err(err(request.id, -32600, "jsonrpc must be 2.0"));
    }
    Ok(request)
}

pub fn ok(id: Option<Value>, result: Value) -> Response {
    Response {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    }
}

pub fn err(id: Option<Value>, code: i32, message: &str) -> Response {
    error(id, code, message, None)
}

pub fn err_data(id: Option<Value>, code: i32, message: &str, data: Value) -> Response {
    error(id, code, message, Some(data))
}

fn error(id: Option<Value>, code: i32, message: &str, data: Option<Value>) -> Response {
    Response {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(Error {
            code,
            message: message.to_string(),
            data,
        }),
    }
}

pub fn notification(method: &str, params: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
    .to_string()
}

pub fn to_line(r: &Response) -> String {
    serde_json::to_string(r).unwrap_or_else(|_| {
        "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32603,\"message\":\"encode\"}}".to_string()
    })
}
