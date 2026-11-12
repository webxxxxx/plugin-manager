use crate::registry::{Kind, Registry};
use crate::rpc;
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
    rpc::ok(
        id,
        json!({
            "content": [{ "type": "text", "text": text }],
            "isError": is_error,
        }),
    )
}

/// A tool execution failure (plugin error, crash, timeout, protocol violation).
pub fn tool_error(id: Option<Value>, message: &str) -> rpc::Response {
    tool_result(id, message.to_string(), true)
}

pub fn run_stdio() -> i32 {
    use std::io::{BufRead, Write};
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let resp = match crate::rpc::parse(&line) {
            Ok(req) => crate::rpc::ok(req.id, json!({"ok": true})),
            Err(e) => e,
        };
        if writeln!(stdout, "{}", crate::rpc::to_line(&resp)).is_err() {
            break;
        }
        let _ = stdout.flush();
    }
    0
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
            desc: "test".into(),
            deps: Vec::new(),
            caps: caps.iter().map(|cap| capability(cap)).collect(),
            exec: Some(Exec {
                path: "/bin/true".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
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
}
