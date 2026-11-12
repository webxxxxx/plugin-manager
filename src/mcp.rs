use crate::registry::{Kind, Registry};
use serde_json::{json, Value};

/// MCP tool projection; wired by the bridge in #32.
#[allow(dead_code)]
pub fn tools(reg: &Registry) -> Vec<Value> {
    let mut out = Vec::new();
    // Core services are lifecycle units, not tool providers.
    for svc in reg.values().filter(|svc| svc.kind == Kind::Plugin) {
        out.push(json!({
            "name": format!("{}.start", svc.id),
            "description": format!("Start service {}", svc.id),
            "inputSchema": {"type": "object", "properties": {}}
        }));
        if svc.state.is_live() {
            for cap in &svc.caps {
                out.push(json!({
                    "name": format!("{}.{}", svc.id, cap),
                    "description": format!("{} on {}", cap, svc.id),
                    "inputSchema": {"type": "object", "properties": {}}
                }));
            }
        }
    }
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
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
    use crate::registry::{Exec, Ready, Service, State};

    fn service(id: &str, kind: Kind, caps: Vec<String>) -> Service {
        Service {
            id: id.into(),
            kind,
            version: "1.0.0".into(),
            desc: "test".into(),
            deps: Vec::new(),
            caps,
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

    #[test]
    fn core_services_never_appear_in_the_tool_projection() {
        let mut registry = Registry::new();
        registry
            .insert(service("core", Kind::Core, vec!["status".into()]))
            .expect("core");
        registry
            .insert(service("plug", Kind::Plugin, vec!["status".into()]))
            .expect("plugin");

        let names: Vec<String> = tools(&registry)
            .iter()
            .filter_map(|tool| tool["name"].as_str().map(str::to_string))
            .collect();

        assert!(
            !names.iter().any(|name| name.starts_with("core.")),
            "{names:?}"
        );
        assert!(names.contains(&"plug.status".to_string()), "{names:?}");
    }
}
