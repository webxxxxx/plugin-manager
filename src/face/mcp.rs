use crate::server::mcp::result::{
    error_response, merge_meta, server_meta, tool_error_result, tool_result_object, SERVER_NAME,
};
use crate::server::mcp::session::{validate_meta, Served, MODERN_VERSION};
use crate::wire::control::channel;
use crate::wire::message::ipc::{self, FrameEvent};
use crate::wire::message::rpc;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const LEGACY_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const TTL_MS: u64 = 1000;
const LEGACY_POLL: Duration = Duration::from_secs(1);
const MODERN_IDLE: Duration = Duration::from_secs(30);

/// Serve one MCP client over stdio. The bridge never launches svcd; if the
/// daemon is unreachable at startup it writes to stderr and exits 1.
pub fn run_stdio() -> i32 {
    let config = match crate::platform::config::Config::bootstrap_or_report("mcp") {
        Some(config) => config,
        None => return 1,
    };
    let limits = config.limits.clone();
    let config_dir = config.paths.control_dir().to_path_buf();
    if let Err(error) = startup_ping(&config_dir) {
        eprintln!("mcp: {}", error);
        return 1;
    }
    let (sender, receiver) = mpsc::sync_channel(256);
    if let Err(error) = ipc::spawn_reader("mcp-stdin".into(), io::stdin(), sender, None, &limits) {
        eprintln!("mcp: stdin: {}", error);
        return 1;
    }
    let mut bridge = Bridge {
        config: config_dir,
        era: None,
        legacy_ready: false,
        last_version: None,
    };
    let mut stdout = io::stdout();
    let mut last_poll = Instant::now();
    loop {
        let timeout = if bridge.era == Some(Era::Legacy) {
            LEGACY_POLL
        } else {
            MODERN_IDLE
        };
        match receiver.recv_timeout(timeout) {
            Ok(FrameEvent::Frame(Ok(line))) => {
                if let Outcome::Exit(code) = bridge.handle_line(&line, &mut stdout) {
                    return code;
                }
            }
            Ok(FrameEvent::Frame(Err(error))) => {
                eprintln!("mcp: stdin: {}", error);
                return 1;
            }
            Ok(FrameEvent::Closed) => return 0,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return 0,
        }
        // Poll on elapsed time, not only on an idle timeout: a legacy client
        // that keeps sending requests must not starve list_changed.
        if bridge.era == Some(Era::Legacy)
            && bridge.legacy_ready
            && last_poll.elapsed() >= LEGACY_POLL
        {
            if let Err(error) = bridge.poll_legacy(&mut stdout) {
                eprintln!("mcp: svcd unreachable: {}", error);
                return 1;
            }
            last_poll = Instant::now();
        }
    }
}

fn startup_ping(config: &Path) -> Result<(), String> {
    let mut response = channel::request(config, "svcd.ping", json!({}))
        .map_err(|error| format!("svcd unreachable: {}", error))?;
    if let Some(error) = response.error.take() {
        return Err(error.message);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Era {
    Modern,
    Legacy,
}

enum Outcome {
    Continue,
    Exit(i32),
}

/// Serve one modern request without a progress sink (HTTP).
#[cfg_attr(not(feature = "http"), allow(dead_code))]
pub fn serve_modern(config: &Path, request: &rpc::Request) -> Served {
    serve_modern_with(config, request, None)
}

/// Serve one modern request. The stdio bridge passes its stdout as a progress
/// sink so plugin `notifications/progress` frames reach the MCP client while
/// the request is still in flight. HTTP has no streaming response and passes
/// `None`, but still drains the notifications.
pub fn serve_modern_with(
    config: &Path,
    request: &rpc::Request,
    progress: Option<&mut dyn Write>,
) -> Served {
    let id = request.id.clone();
    if let Err(response) = validate_meta(id.clone(), &request.params) {
        return Served::Respond(response);
    }
    match request.method.as_str() {
        "server/discover" => Served::Respond(discover(id)),
        "tools/list" => {
            let (response, fatal) = tools_list(config, id, Era::Modern, &request.params);
            Served::new(response, fatal)
        }
        "tools/call" => {
            let (response, fatal) = tools_call(config, id, Era::Modern, &request.params, progress);
            Served::new(response, fatal)
        }
        _ => Served::Respond(rpc::err(id, -32601, "method not found")),
    }
}

struct Bridge {
    config: PathBuf,
    era: Option<Era>,
    /// The legacy handshake is complete only after notifications/initialized.
    legacy_ready: bool,
    /// Baseline registry version for legacy list_changed polling.
    last_version: Option<u64>,
}

impl Bridge {
    fn handle_line(&mut self, line: &str, stdout: &mut impl Write) -> Outcome {
        let request = match rpc::parse(line) {
            Ok(request) => request,
            Err(response) => {
                if let Err(error) = write_response(stdout, &response) {
                    eprintln!("mcp: stdout: {}", error);
                    return Outcome::Exit(1);
                }
                return Outcome::Continue;
            }
        };
        if request.id.is_none() {
            // Notifications never get a response. The legacy lifecycle uses
            // notifications/initialized to open the normal operation phase;
            // notifications/cancelled is accepted and ignored.
            if self.era == Some(Era::Legacy) && request.method == "notifications/initialized" {
                self.legacy_ready = true;
            }
            return Outcome::Continue;
        }
        if self.era.is_none() {
            self.era = Some(if request.method == "initialize" {
                Era::Legacy
            } else {
                Era::Modern
            });
        }
        let (response, exit) = match self.era.expect("era was just selected") {
            Era::Modern => match serve_modern_with(&self.config, &request, Some(stdout)) {
                Served::Respond(response) => (response, false),
                Served::Unreachable(response) => (response, true),
            },
            Era::Legacy => self.handle_legacy(&request, stdout),
        };
        if let Err(error) = write_response(stdout, &response) {
            eprintln!("mcp: stdout: {}", error);
            return Outcome::Exit(1);
        }
        if exit {
            Outcome::Exit(1)
        } else {
            Outcome::Continue
        }
    }

    fn handle_legacy(
        &mut self,
        request: &rpc::Request,
        stdout: &mut impl Write,
    ) -> (rpc::Response, bool) {
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => (self.initialize(id, &request.params), false),
            "ping" => (rpc::ok(id, json!({})), false),
            "tools/list" => tools_list(&self.config, id, Era::Legacy, &request.params),
            "tools/call" => {
                tools_call(&self.config, id, Era::Legacy, &request.params, Some(stdout))
            }
            _ => (rpc::err(id, -32601, "method not found"), false),
        }
    }
}

fn tools_list(config: &Path, id: Option<Value>, era: Era, params: &Value) -> (rpc::Response, bool) {
    let mut request = json!({});
    if let Some(cursor) = params.get("cursor").and_then(Value::as_str) {
        request["cursor"] = json!(cursor);
    }
    if let Some(limit) = params.get("limit").and_then(Value::as_u64) {
        request["limit"] = json!(limit);
    }
    match channel::request(config, "registry.tools", request) {
        Ok(response) => {
            if let Some(error) = response.error {
                return (error_response(id, error), false);
            }
            let result_value = response.result.unwrap_or_else(|| json!({}));
            let tools = result_value
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // `registry.tools` returns the face-neutral `next`; the MCP result
            // names the cursor and decides which fields cross into the protocol.
            let mut result = json!({ "tools": tools });
            if let Some(next_cursor) = result_value.get("next") {
                result["nextCursor"] = next_cursor.clone();
            }
            if era == Era::Modern {
                result["resultType"] = json!("complete");
                result["ttlMs"] = json!(TTL_MS);
                result["cacheScope"] = json!("private");
                result["_meta"] = server_meta();
            }
            (rpc::ok(id, result), false)
        }
        Err(error) => {
            eprintln!("mcp: svcd unreachable: {}", error);
            (
                rpc::err(id, -32603, &format!("svcd unreachable: {}", error)),
                true,
            )
        }
    }
}

fn tools_call(
    config: &Path,
    id: Option<Value>,
    era: Era,
    params: &Value,
    progress: Option<&mut dyn Write>,
) -> (rpc::Response, bool) {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return (
            rpc::err(id, -32602, "tools/call requires params.name"),
            false,
        );
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let progress_token = params
        .get("_meta")
        .and_then(|meta| meta.get("progressToken"))
        .cloned();
    let mut request = json!({ "name": name, "arguments": arguments });
    if let Some(token) = progress_token {
        request["progressToken"] = token;
    }
    let mut progress = progress;
    let response = channel::request_stream(
        config,
        "registry.call",
        request,
        None,
        || false,
        |line| {
            if let Some(out) = progress.as_deref_mut() {
                let _ = out.write_all(line.as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }
        },
    );
    match response {
        Ok(response) => {
            if let Some(error) = response.error {
                return (error_response(id, error), false);
            }
            let mut result = tool_result_object(response.result.unwrap_or_else(|| json!({})));
            if era == Era::Modern {
                result["resultType"] = json!("complete");
                merge_meta(&mut result, server_meta());
            }
            (rpc::ok(id, result), false)
        }
        Err(error) => {
            eprintln!("mcp: svcd unreachable: {}", error);
            let mut result = tool_error_result(&format!("svcd unreachable: {}", error));
            if era == Era::Modern {
                result["resultType"] = json!("complete");
                merge_meta(&mut result, server_meta());
            }
            (rpc::ok(id, result), true)
        }
    }
}

impl Bridge {
    /// Answer the legacy handshake and take a baseline of the tool list so a
    /// change immediately after initialize is still notified by the poller.
    fn initialize(&mut self, id: Option<Value>, params: &Value) -> rpc::Response {
        let response = legacy_initialize(id, params);
        if let Ok(mut version) = channel::request(&self.config, "registry.version", json!({})) {
            if version.error.is_none() {
                self.last_version = version
                    .result
                    .take()
                    .and_then(|value| value.get("version").and_then(Value::as_u64));
            }
        }
        response
    }

    fn poll_legacy(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        let mut response = channel::request(&self.config, "registry.version", json!({}))
            .map_err(|error| io::Error::other(error.to_string()))?;
        if let Some(error) = response.error.take() {
            return Err(io::Error::other(error.message));
        }
        let version = response
            .result
            .unwrap_or_else(|| json!({}))
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::other("registry.version returned no version"))?;
        if let Some(previous) = self.last_version {
            if previous != version {
                write_notification(stdout, "notifications/tools/list_changed")?;
            }
        }
        self.last_version = Some(version);
        Ok(())
    }
}

fn discover(id: Option<Value>) -> rpc::Response {
    rpc::ok(
        id,
        json!({
            "resultType": "complete",
            "supportedVersions": [MODERN_VERSION],
            "capabilities": { "tools": { "listChanged": false } },
            "_meta": server_meta(),
            "ttlMs": TTL_MS,
            "cacheScope": "private",
        }),
    )
}

fn legacy_initialize(id: Option<Value>, params: &Value) -> rpc::Response {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("");
    let version = LEGACY_VERSIONS
        .iter()
        .find(|version| **version == requested)
        .copied()
        .unwrap_or(LEGACY_VERSIONS[0]);
    rpc::ok(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": true } },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

fn write_response(stdout: &mut impl Write, response: &rpc::Response) -> io::Result<()> {
    ipc::write_frame(stdout, &rpc::to_line(response))
}

fn write_notification(stdout: &mut impl Write, method: &str) -> io::Result<()> {
    ipc::write_frame(
        stdout,
        &json!({ "jsonrpc": "2.0", "method": method }).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modern_meta(version: &str) -> Value {
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        })
    }

    #[test]
    fn modern_requests_need_the_two_required_meta_fields() {
        let missing = validate_meta(Some(json!(1)), &json!({})).expect_err("missing _meta");
        assert_eq!(missing.error.expect("error").code, -32602);

        let missing_caps = validate_meta(
            Some(json!(1)),
            &json!({"_meta": {"io.modelcontextprotocol/protocolVersion": MODERN_VERSION}}),
        )
        .expect_err("missing capabilities");
        assert_eq!(missing_caps.error.expect("error").code, -32602);

        validate_meta(Some(json!(1)), &modern_meta(MODERN_VERSION)).expect("valid meta");
    }

    #[test]
    fn an_unsupported_version_returns_minus_32022_with_supported() {
        let response = validate_meta(Some(json!(1)), &modern_meta("1900-01-01"))
            .expect_err("unsupported version");
        let error = response.error.expect("error");
        assert_eq!(error.code, -32022);
        assert_eq!(
            error.data.expect("data")["supported"],
            json!([MODERN_VERSION])
        );
    }

    #[test]
    fn discovery_declares_the_modern_contract() {
        let response = discover(Some(json!(1)));
        let result = response.result.expect("result");
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["supportedVersions"], json!([MODERN_VERSION]));
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["ttlMs"], TTL_MS);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(
            result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
            SERVER_NAME
        );
    }

    #[test]
    fn legacy_initialize_echoes_a_supported_version_and_falls_back() {
        let response = legacy_initialize(
            Some(json!(1)),
            &json!({"protocolVersion": "2025-06-18", "capabilities": {}}),
        );
        let result = response.result.expect("result");
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], true);
        assert!(result.get("resultType").is_none());

        let fallback = legacy_initialize(Some(json!(2)), &json!({"protocolVersion": "1900-01-01"}));
        assert_eq!(
            fallback.result.expect("fallback")["protocolVersion"],
            LEGACY_VERSIONS[0]
        );
    }
}
