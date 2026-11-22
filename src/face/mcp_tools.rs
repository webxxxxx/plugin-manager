//! MCP 面的协议投影：能力面 → `tools`，一次调用结果 → MCP envelope。
//!
//! 按 ADR-0025 的二分，值投影归内核、**协议投影归面**。`tools` 的
//! `inputSchema`，四个 envelope 函数的 `content[0].text` / `isError` /
//! `data.class` / `data.kind`，以及 JSON-RPC 码（`-32000` / `-32602`）都在这里
//! 成形，组合根只做装配与分发。
//!
//! 桥客户端（`run_stdio` / `Bridge` / era 装饰 / legacy 握手）仍留在
//! [`crate::face::mcp`]：它要 stdio、mpsc 与 socket，不属于投影。

use crate::kernel::failure::{Class, Failure, Rejection};
use crate::kernel::registry::{Kind, Registry};
// 本文件唯一一条 face → server 依赖：`content[0].text` 的字面形状单源在
// `server/mcp/result.rs`（HTTP 会话那边也要用），它的模块注释点名了 face →
// server 这个方向。face 里 api.rs 等也引 server，这条不是全局唯一。
use crate::server::mcp::result::result_value;
use crate::wire::message::rpc;
use serde_json::{json, Value};

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
    // The capability surface, not the kind, decides whether a service exposes
    // tools: the builtin `svc` is a core service with tools. Only plugins get a
    // `<id>.start` tool, because only a supervised process can be dead.
    for svc in reg
        .values()
        .filter(|svc| svc.kind == Kind::Plugin || !svc.contract.surface().is_empty())
    {
        let live = is_live(&svc.id);
        if svc.kind == Kind::Plugin || !live {
            out.push(json!({
                "name": format!("{}.start", svc.id),
                "description": format!("Start service {}", svc.id),
                "inputSchema": {"type": "object", "properties": {}}
            }));
        }
        if live {
            for cap in svc.contract.surface() {
                let name = cap.name.as_str();
                let description = if cap.desc.is_empty() {
                    format!("{} on {}", name, svc.id)
                } else {
                    cap.desc.clone()
                };
                let input_schema = cap.input_schema.clone().unwrap_or(Value::Null);
                let mut tool = json!({
                    "name": format!("{}.{}", svc.id, name),
                    "description": description,
                    "inputSchema": input_schema
                });
                // Undeclared stays absent: a `null` outputSchema would claim the
                // capability has no output, which is a different statement
                // (#260).
                if let Some(output_schema) = &cap.output_schema {
                    tool["outputSchema"] = output_schema.clone();
                }
                out.push(tool);
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

/// Face-side projection of a completed slot call. The slot only emits a
/// `Result<Value, Failure>`; MCP codes/strings live here. Only
/// [`project_slot_outcome`] needs the structured frame; the assembly point
/// consumes the serialized line.
fn project_slot_response(
    external_id: Option<Value>,
    outcome: Result<Value, Failure>,
) -> rpc::Response {
    match outcome {
        Ok(value) => rpc::ok(external_id, value),
        Err(failure) => match failure.class {
            Class::Busy => rpc::err_data(
                external_id,
                -32000,
                "busy: too many in-flight tool calls",
                json!({ "class": failure.class.as_str() }),
            ),
            Class::FrameTooLarge => rpc::err_data(
                external_id,
                -32602,
                "tools/call request exceeds the 1 MiB frame limit",
                json!({ "class": failure.class.as_str() }),
            ),
            Class::NotRunning | Class::WriteFailed => {
                tool_result(external_id, failure.detail, true)
            }
            _ => tool_error(external_id, &failure.detail),
        },
    }
}

pub fn project_slot_outcome(external_id: Option<Value>, outcome: Result<Value, Failure>) -> String {
    rpc::to_line(&project_slot_response(external_id, outcome))
}

/// Face-side projection of an immediate `Instance::call` rejection: the call was
/// never attempted, so it has no [`Failure`]. The wire shapes stay the ones the
/// old `rejection_failure` path produced (BadArguments through the frame-limit
/// error, UnknownCapability through the isError tool result); only the
/// structured `data.kind` is added to the JSON-RPC error.
pub fn project_rejection(external_id: Option<Value>, rejection: Rejection) -> rpc::Response {
    let kind = rejection.as_str();
    match rejection {
        Rejection::BadArguments { .. } => rpc::err_data(
            external_id,
            -32602,
            "tools/call request exceeds the 1 MiB frame limit",
            json!({ "kind": kind }),
        ),
        Rejection::UnknownCapability { capability } => {
            tool_error(external_id, &format!("unknown capability {capability}"))
        }
    }
}

/// Project a builtin call the same way the MCP face projects a slot call:
/// success becomes a text tool result, failures keep the classified envelope.
pub fn project_builtin_outcome(
    external_id: Option<Value>,
    outcome: Result<Value, Failure>,
) -> String {
    match outcome {
        Ok(value) => rpc::to_line(&tool_result(external_id, value.to_string(), false)),
        Err(failure) => project_slot_outcome(external_id, Err(failure)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::contract::{Capability as KernelCapability, CapabilityName, Effect};
    use crate::kernel::failure::Retry;
    use crate::kernel::registry::{Exec, Ready, Service, State};

    fn capability(name: &str) -> KernelCapability {
        KernelCapability {
            name: CapabilityName::new(name),
            desc: format!("{} description", name),
            input_schema: Some(
                json!({"type": "object", "properties": {"input": {"type": "string"}}}),
            ),
            output_schema: None,
            effect: Effect::Write,
            idempotent: false,
        }
    }

    fn service(id: &str, kind: Kind, caps: &[&str]) -> Service {
        let caps = caps.iter().map(|cap| capability(cap)).collect();
        let contract = crate::kernel::registry::contract_for(
            id,
            "1.0.0",
            caps,
            crate::kernel::registry::Carrier::Slot,
            &crate::platform::limits::test_default(),
        );
        Service {
            id: id.into(),
            kind,
            version: "1.0.0".into(),
            protocol: 1,
            desc: "test".into(),
            display_name: None,
            deps: Vec::new(),
            contract,
            impl_ref: None,
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
                interpreter: None,
            }),
            lifecycle: None,
            enabled: true,
            restart: crate::kernel::registry::Restart::Never,
            remain_after_exit: false,
            compat: None,
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

    #[test]
    fn the_capability_surface_not_the_kind_decides_the_tool_projection() {
        let mut registry = Registry::new();
        registry
            .insert(service("core", Kind::Core, &["status"]))
            .expect("core");
        registry
            .insert(service("lifecycle", Kind::Core, &[]))
            .expect("lifecycle");
        registry
            .insert(service("plug", Kind::Plugin, &["status"]))
            .expect("plugin");

        let names = names(&tools(&registry, |_| true));

        assert!(names.contains(&"core.status".to_string()), "{names:?}");
        assert!(
            !names.iter().any(|name| name.starts_with("lifecycle.")),
            "{names:?}"
        );
        assert!(!names.contains(&"core.start".to_string()), "{names:?}");
        assert!(names.contains(&"plug.status".to_string()), "{names:?}");
    }

    #[test]
    fn a_live_builtin_service_publishes_its_capabilities_without_start() {
        let registry = Registry::with_builtin(&crate::platform::limits::test_default());

        let names = names(&tools(&registry, |_| true));

        assert_eq!(
            names,
            vec!["svc.health".to_string(), "svc.version".to_string()]
        );
    }

    #[test]
    fn a_dead_core_with_capabilities_only_exposes_start() {
        let mut registry = Registry::new();
        registry
            .insert(service("core", Kind::Core, &["status"]))
            .expect("core");

        let names = names(&tools(&registry, |_| false));

        assert_eq!(names, vec!["core.start".to_string()]);
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
    fn output_schema_is_projected_only_when_the_capability_declares_one() {
        let mut registry = Registry::new();
        let mut declared = capability("declared");
        declared.output_schema =
            Some(json!({"type": "object", "properties": {"ok": {"type": "boolean"}}}));
        let contract = crate::kernel::registry::contract_for(
            "plug",
            "1.0.0",
            vec![declared, capability("bare")],
            crate::kernel::registry::Carrier::Slot,
            &crate::platform::limits::test_default(),
        );
        let mut plugin = service("plug", Kind::Plugin, &[]);
        plugin.contract = contract;
        registry.insert(plugin).expect("plugin");

        let projected = tools(&registry, |_| true);
        let tool = |name: &str| {
            projected
                .iter()
                .find(|tool| tool["name"] == name)
                .unwrap_or_else(|| panic!("missing tool {name}"))
                .clone()
        };
        assert_eq!(
            tool("plug.declared")["outputSchema"],
            json!({"type": "object", "properties": {"ok": {"type": "boolean"}}})
        );
        // Undeclared is absent, not `null`: `null` would claim "no output".
        assert!(tool("plug.bare").get("outputSchema").is_none());

        // The builtin `svc` has no output contract either.
        let registry = Registry::with_builtin(&crate::platform::limits::test_default());
        for tool in tools(&registry, |_| true) {
            assert!(tool.get("outputSchema").is_none(), "{tool}");
        }
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
    fn the_builtin_outcome_projects_values_and_failures() {
        let ok = project_builtin_outcome(Some(json!(1)), Ok(json!({"version": "1.2.3"})));
        let response: rpc::Response = serde_json::from_str(&ok).expect("ok json");
        let result = response.result.expect("result");
        assert_eq!(result["isError"], false);
        assert_eq!(result["content"][0]["text"], "{\"version\":\"1.2.3\"}");

        let failed = project_builtin_outcome(
            Some(json!(2)),
            Err(Failure::new(Class::Busy, Retry::Never, "busy")),
        );
        let response: rpc::Response = serde_json::from_str(&failed).expect("error json");
        let error = response.error.expect("error");
        assert_eq!(error.code, -32000);
        assert_eq!(error.data.expect("data")["class"], "busy");
    }

    #[test]
    fn the_slot_response_keeps_all_eight_class_shapes() {
        for class in Class::ALL {
            let detail = format!("detail {}", class.as_str());
            let response = project_slot_response(
                Some(json!(1)),
                Err(Failure::new(class, class.default_retry(), detail.clone())),
            );
            match class {
                Class::Busy => {
                    let error = response.error.expect("busy error");
                    assert_eq!(error.code, -32000);
                    assert_eq!(error.message, "busy: too many in-flight tool calls");
                    assert_eq!(error.data.expect("data")["class"], "busy");
                }
                Class::FrameTooLarge => {
                    let error = response.error.expect("frame error");
                    assert_eq!(error.code, -32602);
                    assert_eq!(
                        error.message,
                        "tools/call request exceeds the 1 MiB frame limit"
                    );
                    assert_eq!(error.data.expect("data")["class"], "frame_too_large");
                }
                _ => {
                    let result = response.result.expect("isError result");
                    assert_eq!(result["isError"], true);
                    assert_eq!(result["content"][0]["text"], detail);
                }
            }
        }
    }

    #[test]
    fn an_immediate_rejection_projects_its_kind_without_changing_the_message() {
        let response = project_rejection(
            Some(json!(5)),
            Rejection::BadArguments {
                detail: "arguments are too large".into(),
            },
        );
        let error = response.error.expect("bad arguments error");
        assert_eq!(error.code, -32602);
        assert_eq!(
            error.message,
            "tools/call request exceeds the 1 MiB frame limit"
        );
        assert_eq!(error.data.expect("data")["kind"], "bad_arguments");

        let response = project_rejection(
            Some(json!(6)),
            Rejection::UnknownCapability {
                capability: "nope".into(),
            },
        );
        let result = response.result.expect("unknown capability result");
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], "unknown capability nope");
    }
}
