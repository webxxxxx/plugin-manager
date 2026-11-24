//! L5 face: control-channel RPC → Command.
//!
//! The control socket is a wire protocol, not the daemon's API. Method
//! spelling, required parameters and the error frame for a malformed request
//! all belong here; daemon.rs only runs the parsed command against the live
//! services. Adding a method is one variant plus one handler arm.

use serde_json::{json, Value};

use crate::kernel::contract::ProgressToken;
use crate::kernel::failure::Problem;
use crate::manage::Strategy;
use crate::wire::message::rpc;

/// One parsed control request: the id the response must echo, plus the
/// command to run. The wire spelling is gone by the time this value exists.
#[derive(Debug)]
pub struct Call {
    pub id: Option<Value>,
    pub command: Command,
}

/// Every management method the control channel knows. tools/call is here
/// for symmetry, but it is deferred: the caller routes it to the reply sink
/// instead of the synchronous dispatch below.
#[derive(Debug)]
pub enum Command {
    Ping,
    Compat,
    Shutdown,
    ToolsPage {
        cursor: Option<String>,
        limit: Option<usize>,
    },
    Version,
    List,
    Status {
        service: String,
        probe: bool,
    },
    Start {
        service: String,
    },
    Stop {
        service: String,
    },
    Restart {
        service: String,
    },
    Enable {
        service: String,
    },
    Disable {
        service: String,
    },
    Reload,
    Install(Install),
    InstallStaged(Install),
    Uninstall {
        service: String,
        purge: bool,
    },
    AuthStatus,
    AuthPasswd {
        password: String,
    },
    /// Clear the daemon log (`None`) or one plugin's logs.
    LogsClear {
        service: Option<String>,
    },
    /// Restart the daemon itself (user mode) or answer how to restart it.
    RestartDaemon,
    ToolCall(ToolCall),
}

/// A parsed plugin.install / plugin.install_staged request.
#[derive(Debug)]
pub struct Install {
    pub path: String,
    pub strategy: Strategy,
    pub allow_hooks: bool,
    /// The staged uploader's id/version. plugin.install_staged refuses a
    /// package whose manifest disagrees; a plain install does not read them.
    pub expected_id: Option<String>,
    pub expected_version: Option<String>,
    /// An explicit install root (`--root` / `?root=`). The daemon checks it
    /// against `plugins_dir`; `None` lets the size rule decide (ADR-0030).
    pub root: Option<String>,
}

/// A parsed tools/call / registry.call.
#[derive(Debug)]
pub struct ToolCall {
    pub name: String,
    pub arguments: Value,
    pub progress: Option<ProgressToken>,
}

/// Parse one control request. Errors are complete JSON-RPC frames so the
/// caller can forward them without knowing a method's error codes.
#[allow(clippy::result_large_err)] // The response is the wire value; this runs once per request, not per byte.
pub fn parse(request: rpc::Request) -> Result<Call, rpc::Response> {
    let id = request.id.clone();
    let params = &request.params;
    let command = match request.method.as_str() {
        "svcd.ping" => Command::Ping,
        "svcd.compat" => Command::Compat,
        "svcd.shutdown" => Command::Shutdown,
        "registry.tools" | "tools/list" => Command::ToolsPage {
            cursor: params
                .get("cursor")
                .and_then(Value::as_str)
                .map(str::to_string),
            limit: params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|value| value.min(usize::MAX as u64) as usize),
        },
        "registry.version" => Command::Version,
        "svc.list" => Command::List,
        "svc.status" => Command::Status {
            service: required_service(id.clone(), params)?,
            probe: params
                .get("probe")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        "svc.start" => Command::Start {
            service: required_service(id.clone(), params)?,
        },
        "svc.stop" => Command::Stop {
            service: required_service(id.clone(), params)?,
        },
        "svc.restart" => Command::Restart {
            service: required_service(id.clone(), params)?,
        },
        "svc.enable" => Command::Enable {
            service: required_service(id.clone(), params)?,
        },
        "svc.disable" => Command::Disable {
            service: required_service(id.clone(), params)?,
        },
        "svc.reload" | "svcd.reload" => Command::Reload,
        "plugin.install" => Command::Install(install(id.clone(), params, false)?),
        "plugin.install_staged" => Command::InstallStaged(install(id.clone(), params, true)?),
        "plugin.uninstall" => {
            let service = params
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    problem(
                        id.clone(),
                        -32007,
                        "plugin.uninstall requires id",
                        Problem::Internal,
                    )
                })?;
            Command::Uninstall {
                service,
                purge: params
                    .get("purge")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        }
        "auth.status" => Command::AuthStatus,
        "svcd.restart" => Command::RestartDaemon,
        "svc.logs.clear" => Command::LogsClear {
            service: params.get("id").and_then(Value::as_str).map(str::to_string),
        },
        "auth.passwd" => Command::AuthPasswd {
            password: params
                .get("password")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        "registry.call" | "tools/call" => Command::ToolCall(tool_call(id.clone(), params)?),
        _ => {
            return Err(rpc::err_data(
                id,
                -32601,
                "method not found",
                Problem::Invalid.error_data(),
            ))
        }
    };
    Ok(Call { id, command })
}

#[allow(clippy::result_large_err)] // The response is the wire value, not an allocation per request.
fn required_service(id: Option<Value>, params: &Value) -> Result<String, rpc::Response> {
    params
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| rpc::err_data(id, -32602, "missing id", Problem::Invalid.error_data()))
}

/// plugin.install / plugin.install_staged share path/strategy/hooks. The
/// staged variant additionally pins the id/version the uploader inspected.
#[allow(clippy::result_large_err)] // The response is the wire value, not an allocation per request.
fn install(id: Option<Value>, params: &Value, staged: bool) -> Result<Install, rpc::Response> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            problem(
                id.clone(),
                -32006,
                if staged {
                    "plugin.install_staged requires path"
                } else {
                    "plugin.install requires path"
                },
                Problem::Internal,
            )
        })?;
    let strategy = Strategy::parse(
        params
            .get("strategy")
            .and_then(Value::as_str)
            .unwrap_or("replace"),
    )
    .ok_or_else(|| {
        problem(
            id.clone(),
            -32006,
            "unknown strategy",
            Problem::UnknownStrategy,
        )
    })?;
    let allow_hooks = params
        .get("allowHooks")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let (expected_id, expected_version) = if staged {
        (
            params.get("id").and_then(Value::as_str).map(str::to_string),
            params
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_string),
        )
    } else {
        (None, None)
    };
    Ok(Install {
        path,
        strategy,
        allow_hooks,
        expected_id,
        expected_version,
        root: params
            .get("root")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[allow(clippy::result_large_err)] // The response is the wire value, not an allocation per request.
fn tool_call(id: Option<Value>, params: &Value) -> Result<ToolCall, rpc::Response> {
    let name = params
        .get("name")
        .or_else(|| params.get("tool"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| rpc::err(id.clone(), -32602, "tools/call requires params.name"))?;
    let arguments = match params.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => {
            return Err(rpc::err(
                id,
                -32602,
                "tools/call arguments must be an object",
            ))
        }
    };
    let progress = params.get("progressToken").and_then(|token| match token {
        Value::String(token) => Some(ProgressToken::new(token.clone())),
        Value::Number(token) => Some(ProgressToken::new(token.to_string())),
        _ => None,
    });
    Ok(ToolCall {
        name,
        arguments,
        progress,
    })
}

fn problem(id: Option<Value>, code: i32, message: &str, problem: Problem) -> rpc::Response {
    rpc::err_data(id, code, message, problem.error_data())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::result_large_err)] // Same shape as the production parser under test.
    fn call(method: &str, params: Value) -> Result<Call, rpc::Response> {
        parse(rpc::Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(7)),
            method: method.into(),
            params,
        })
    }

    fn command(method: &str, params: Value) -> Command {
        call(method, params)
            .unwrap_or_else(|response| panic!("{method} should parse: {response:?}"))
            .command
    }

    #[test]
    fn every_read_method_parses() {
        assert!(matches!(command("svcd.ping", json!({})), Command::Ping));
        assert!(matches!(command("svcd.compat", json!({})), Command::Compat));
        assert!(matches!(
            command("registry.tools", json!({})),
            Command::ToolsPage {
                cursor: None,
                limit: None
            }
        ));
        assert!(matches!(
            command("tools/list", json!({"cursor": "c1", "limit": 9})),
            Command::ToolsPage {
                cursor: Some(ref cursor),
                limit: Some(9)
            } if cursor == "c1"
        ));
        assert!(matches!(
            command("registry.version", json!({})),
            Command::Version
        ));
        assert!(matches!(command("svc.list", json!({})), Command::List));
        assert!(matches!(
            command("svc.status", json!({"id": "alpha", "probe": true})),
            Command::Status {
                ref service,
                probe: true
            } if service == "alpha"
        ));
        assert!(matches!(
            command("auth.status", json!({})),
            Command::AuthStatus
        ));
    }

    #[test]
    fn every_lifecycle_method_parses() {
        for (method, expected) in [
            ("svc.start", "start"),
            ("svc.stop", "stop"),
            ("svc.restart", "restart"),
            ("svc.enable", "enable"),
            ("svc.disable", "disable"),
        ] {
            let command = command(method, json!({"id": "alpha"}));
            let service = match command {
                Command::Start { service } if expected == "start" => service,
                Command::Stop { service } if expected == "stop" => service,
                Command::Restart { service } if expected == "restart" => service,
                Command::Enable { service } if expected == "enable" => service,
                Command::Disable { service } if expected == "disable" => service,
                other => panic!("{method} parsed as {other:?}"),
            };
            assert_eq!(service, "alpha", "{method}");
        }
        assert!(matches!(command("svc.reload", json!({})), Command::Reload));
        assert!(matches!(command("svcd.reload", json!({})), Command::Reload));
    }

    #[test]
    fn plugin_methods_parse_their_parameters() {
        let Command::Install(install) = command(
            "plugin.install",
            json!({"path": "/tmp/x.spk", "strategy": "keep-old", "allowHooks": true}),
        ) else {
            panic!("plugin.install did not parse");
        };
        assert_eq!(install.path, "/tmp/x.spk");
        assert_eq!(install.strategy, Strategy::KeepOld);
        assert!(install.allow_hooks);
        assert!(install.expected_id.is_none());

        let Command::InstallStaged(staged) = command(
            "plugin.install_staged",
            json!({"path": "/tmp/staging", "id": "alpha", "version": "1.2.3"}),
        ) else {
            panic!("plugin.install_staged did not parse");
        };
        assert_eq!(staged.strategy, Strategy::Replace);
        assert_eq!(staged.expected_id.as_deref(), Some("alpha"));
        assert_eq!(staged.expected_version.as_deref(), Some("1.2.3"));

        assert!(matches!(
            command("plugin.uninstall", json!({"id": "alpha", "purge": true})),
            Command::Uninstall {
                ref service,
                purge: true
            } if service == "alpha"
        ));
    }

    #[test]
    fn auth_password_and_tool_calls_parse() {
        assert!(matches!(
            command("auth.passwd", json!({"password": "secret"})),
            Command::AuthPasswd { ref password } if password == "secret"
        ));
        assert!(matches!(
            command("auth.passwd", json!({})),
            Command::AuthPasswd { ref password } if password.is_empty()
        ));

        let Command::ToolCall(tool) = command(
            "tools/call",
            json!({"name": "alpha.echo", "arguments": {"x": 1}, "progressToken": 3}),
        ) else {
            panic!("tools/call did not parse");
        };
        assert_eq!(tool.name, "alpha.echo");
        assert_eq!(tool.arguments, json!({"x": 1}));
        assert!(tool.progress.is_some());

        let Command::ToolCall(tool) = command("registry.call", json!({"tool": "alpha.echo"}))
        else {
            panic!("registry.call did not parse");
        };
        assert_eq!(tool.name, "alpha.echo");
        assert_eq!(tool.arguments, json!({}));
    }

    #[test]
    fn malformed_requests_keep_their_wire_error() {
        let unknown = call("no.such.method", json!({})).unwrap_err();
        assert_eq!(unknown.error.as_ref().unwrap().code, -32601);
        assert_eq!(unknown.error.unwrap().data.unwrap()["kind"], "invalid");

        let missing = call("svc.start", json!({})).unwrap_err();
        assert_eq!(missing.error.as_ref().unwrap().code, -32602);
        assert_eq!(missing.error.unwrap().data.unwrap()["kind"], "invalid");

        let no_path = call("plugin.install", json!({})).unwrap_err();
        assert_eq!(no_path.error.as_ref().unwrap().code, -32006);
        assert_eq!(no_path.error.unwrap().data.unwrap()["kind"], "internal");

        let bad_strategy =
            call("plugin.install", json!({"path": "x", "strategy": "nope"})).unwrap_err();
        assert_eq!(bad_strategy.error.as_ref().unwrap().code, -32006);
        assert_eq!(
            bad_strategy.error.unwrap().data.unwrap()["kind"],
            "unknown_strategy"
        );

        let no_id = call("plugin.uninstall", json!({})).unwrap_err();
        assert_eq!(no_id.error.as_ref().unwrap().code, -32007);
        assert_eq!(no_id.error.unwrap().data.unwrap()["kind"], "internal");
    }

    #[test]
    fn malformed_tool_calls_are_parameter_errors() {
        let no_name = call("tools/call", json!({})).unwrap_err();
        assert_eq!(no_name.error.as_ref().unwrap().code, -32602);
        assert_eq!(
            no_name.error.as_ref().unwrap().message,
            "tools/call requires params.name"
        );
        assert!(no_name.error.unwrap().data.is_none());

        let bad_arguments =
            call("tools/call", json!({"name": "alpha.echo", "arguments": []})).unwrap_err();
        assert_eq!(bad_arguments.error.as_ref().unwrap().code, -32602);
        assert_eq!(
            bad_arguments.error.as_ref().unwrap().message,
            "tools/call arguments must be an object"
        );
        assert!(bad_arguments.error.unwrap().data.is_none());
    }
}
