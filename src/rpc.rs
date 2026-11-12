use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

pub fn parse(line: &str) -> Result<Request, Response> {
    serde_json::from_str::<Request>(line).map_err(|e| err(None, -32700, &e.to_string()))
}

pub fn ok(id: Option<Value>, result: Value) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: Some(result),
        error: None,
    }
}

pub fn err(id: Option<Value>, code: i32, message: &str) -> Response {
    Response {
        jsonrpc: "2.0",
        id,
        result: None,
        error: Some(Error {
            code,
            message: message.to_string(),
        }),
    }
}

pub fn to_line(r: &Response) -> String {
    serde_json::to_string(r).unwrap_or_else(|_| "{\"jsonrpc\":\"2.0\",\"error\":{\"code\":-32603,\"message\":\"encode\"}}".to_string())
}
