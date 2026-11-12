use crate::registry::Registry;
use serde_json::{json, Value};

/// MCP tool projection; wired by the bridge in #32.
#[allow(dead_code)]
pub fn tools(reg: &Registry) -> Vec<Value> {
    let mut out = Vec::new();
    for svc in reg.values() {
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
