use crate::channel;
use crate::ipc::{self, FrameEvent};
use crate::paths;
use crate::registry::{Kind, Registry};
use crate::rpc;
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

pub const MODERN_VERSION: &str = "2026-07-28";
const LEGACY_VERSIONS: [&str; 3] = ["2025-11-25", "2025-06-18", "2025-03-26"];
const SERVER_NAME: &str = "svcd";
const TTL_MS: u64 = 1000;
const LEGACY_POLL: Duration = Duration::from_secs(1);
const MODERN_IDLE: Duration = Duration::from_secs(30);

/// Project the current plugin tool surface.
///
/// The daemon supplies the live predicate because the registry snapshot alone
/// cannot distinguish a crashed plugin from a running one. A dead plugin still
/// exposes its <id>.start tool so it can be brought back.
pub fn tools<F>(reg: &Registry, is_live: F) -> Vec<Value>
where
    F: Fn(&str) -> bool,
{
    let mut out = Vec::new();
    // Core services are lifecycle units, not tool providers.
    for svc in reg.values().filter(|svc| svc.kind == Kind::Plugin) {
        out.push(json!({
            "name": format!("{}.start", svc.id),
            "description": format!("Start plugin {}", svc.id),
            "inputSchema": {"type": "object", "properties": {}}
        }));
        if is_live(&svc.id) {
            for cap in &svc.caps {
                let description = if cap.description.is_empty() {
                    format!("{} on {}", cap.name, svc.id)
                } else {
                    cap.description.clone()
                };
                out.push(json!({
                    "name": format!("{}.{}", svc.id, cap.name),
                    "description": description,
                    "inputSchema": cap.input_schema.clone()
                }));
            }
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// Build one MCP tool result. Both the aggregation layer and the plugin slot
/// use this shape, so the envelope lives here rather than in each caller.
pub fn tool_result(id: Option<Value>, text: String, is_error: bool) -> rpc::Response {
    rpc::ok(id, result_value(text, is_error))
}

/// A tool execution failure (plugin error, crash, timeout, protocol violation).
pub fn tool_error(id: Option<Value>, message: &str) -> rpc::Response {
    tool_result(id, message.to_string(), true)
}

fn result_value(text: String, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn tool_error_result(message: &str) -> Value {
    result_value(message.to_string(), true)
}

fn error_response(id: Option<Value>, error: rpc::Error) -> rpc::Response {
    rpc::Response {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(error),
    }
}

/// Serve one MCP client over stdio. The bridge never launches svcd; if the
/// daemon is unreachable at startup it writes to stderr and exits 1.
pub fn run_stdio() -> i32 {
    let config = paths::config_dir();
    if let Err(error) = startup_ping(&config) {
        eprintln!("mcp: {}", error);
        return 1;
    }
    let (sender, receiver) = mpsc::sync_channel(256);
    if let Err(error) = ipc::spawn_reader("mcp-stdin".into(), io::stdin(), sender, None) {
        eprintln!("mcp: stdin: {}", error);
        return 1;
    }
    let mut bridge = Bridge {
        config,
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

/// What a modern (2026-07-28) request produced. `Unreachable` still carries a
/// reply for the client, but the control channel is gone: the stdio bridge
/// sends it and exits 1; HTTP sends the same JSON-RPC envelope and stays up.
pub enum Served {
    Respond(rpc::Response),
    Unreachable(rpc::Response),
}

impl Served {
    fn new(response: rpc::Response, fatal: bool) -> Self {
        if fatal {
            Served::Unreachable(response)
        } else {
            Served::Respond(response)
        }
    }
}

/// Serve one modern request. The stdio bridge and the HTTP transport both call
/// this, so tool visibility, `_meta` validation and error mapping cannot drift
/// between the two surfaces.
pub fn serve_modern(config: &Path, request: &rpc::Request) -> Served {
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
            let (response, fatal) = tools_call(config, id, Era::Modern, &request.params);
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
            Era::Modern => match serve_modern(&self.config, &request) {
                Served::Respond(response) => (response, false),
                Served::Unreachable(response) => (response, true),
            },
            Era::Legacy => self.handle_legacy(&request),
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

    fn handle_legacy(&mut self, request: &rpc::Request) -> (rpc::Response, bool) {
        let id = request.id.clone();
        match request.method.as_str() {
            "initialize" => (self.initialize(id, &request.params), false),
            "ping" => (rpc::ok(id, json!({})), false),
            "tools/list" => tools_list(&self.config, id, Era::Legacy, &request.params),
            "tools/call" => tools_call(&self.config, id, Era::Legacy, &request.params),
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
            let mut result = json!({ "tools": tools });
            if let Some(next_cursor) = result_value.get("nextCursor") {
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

fn tools_call(config: &Path, id: Option<Value>, era: Era, params: &Value) -> (rpc::Response, bool) {
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
    let request = json!({ "name": name, "arguments": arguments });
    match channel::request(config, "registry.call", request) {
        Ok(response) => {
            if let Some(error) = response.error {
                return (error_response(id, error), false);
            }
            let mut result = tool_result_object(response.result.unwrap_or_else(|| json!({})));
            if era == Era::Modern {
                result["resultType"] = json!("complete");
                result["_meta"] = server_meta();
            }
            (rpc::ok(id, result), false)
        }
        Err(error) => {
            eprintln!("mcp: svcd unreachable: {}", error);
            let mut result = tool_error_result(&format!("svcd unreachable: {}", error));
            if era == Era::Modern {
                result["resultType"] = json!("complete");
                result["_meta"] = server_meta();
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

/// A plugin may return an arbitrary JSON value. MCP tool results must be an
/// object, so wrap a scalar before decorating it.
fn tool_result_object(result: Value) -> Value {
    if result.is_object() {
        result
    } else {
        tool_error_result("plugin returned a non-object tool result")
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

fn server_meta() -> Value {
    json!({
        "io.modelcontextprotocol/serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
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
    use crate::registry::{Capability, Exec, Ready, Service, State};

    fn capability(name: &str) -> Capability {
        Capability {
            name: name.into(),
            description: format!("{} description", name),
            input_schema: json!({"type": "object", "properties": {"input": {"type": "string"}}}),
        }
    }

    fn service(id: &str, kind: Kind, caps: &[&str]) -> Service {
        Service {
            id: id.into(),
            kind,
            version: "1.0.0".into(),
            protocol: 1,
            desc: "test".into(),
            deps: Vec::new(),
            caps: caps.iter().map(|cap| capability(cap)).collect(),
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
                interpreter: None,
            }),
            lifecycle: None,
            enabled: true,
            state: State::Running,
            pid: None,
            since: None,
            ready_since: None,
            next_at: None,
            failures: 0,
            last_exit: None,
            config: None,
        }
    }

    fn names(tools: &[Value]) -> Vec<String> {
        tools
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect()
    }

    fn modern_meta(version: &str) -> Value {
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": version,
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        })
    }

    #[test]
    fn core_services_never_appear_in_the_tool_projection() {
        let mut registry = Registry::new();
        registry
            .insert(service("core", Kind::Core, &["status"]))
            .expect("core");
        registry
            .insert(service("plug", Kind::Plugin, &["status"]))
            .expect("plugin");

        let names = names(&tools(&registry, |_| true));

        assert!(
            !names.iter().any(|name| name.starts_with("core.")),
            "{names:?}"
        );
        assert!(names.contains(&"plug.status".to_string()), "{names:?}");
    }

    #[test]
    fn a_dead_plugin_only_exposes_start() {
        let mut registry = Registry::new();
        registry
            .insert(service("plug", Kind::Plugin, &["status"]))
            .expect("plugin");

        let names = names(&tools(&registry, |_| false));

        assert_eq!(names, vec!["plug.start".to_string()]);
    }

    #[test]
    fn a_live_plugin_exposes_manifest_descriptions_and_schemas() {
        let mut registry = Registry::new();
        registry
            .insert(service("plug", Kind::Plugin, &["status"]))
            .expect("plugin");

        let projected = tools(&registry, |_| true);
        let status = projected
            .iter()
            .find(|tool| tool["name"] == "plug.status")
            .expect("status tool");

        assert_eq!(status["description"], "status description");
        assert_eq!(status["inputSchema"]["type"], "object");
        assert_eq!(
            status["inputSchema"]["properties"]["input"]["type"],
            "string"
        );
    }

    #[test]
    fn tools_are_sorted_by_name() {
        let mut registry = Registry::new();
        registry
            .insert(service("z", Kind::Plugin, &["alpha"]))
            .expect("z");
        registry
            .insert(service("a", Kind::Plugin, &["zeta"]))
            .expect("a");

        let names = names(&tools(&registry, |_| true));

        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
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
