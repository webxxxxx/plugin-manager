//! `svc status` payload and its text projection.
//!
//! `since`, `ready_since` and `next_at` are Unix epoch seconds; `last_exit` is
//! the last recorded exit code. Missing runtime values stay `null`/`-` until the
//! supervisor (#29) supplies them.

use std::fmt::Display;

use serde::Serialize;

use crate::registry::{Kind, Registry, Service, State};

/// The fixed `svc status` payload. `svc list` and the text output are both
/// projections of this shape; MCP `svc.list` / `svc.describe` will reuse it when
/// the bridge lands (#32).
#[derive(Debug, Serialize)]
pub struct Status<'a> {
    pub summary: Summary<'a>,
    pub details: Details<'a>,
}

#[derive(Debug, Serialize)]
pub struct Summary<'a> {
    pub id: &'a str,
    pub kind: Kind,
    pub state: State,
    pub enabled: bool,
    pub pid: Option<u32>,
    pub since: Option<u64>,
    pub version: &'a str,
}

#[derive(Debug, Serialize)]
pub struct Details<'a> {
    pub dependencies: &'a [String],
    pub last_exit: Option<i32>,
    pub next_at: Option<u64>,
    pub ready_since: Option<u64>,
    pub failures: u32,
    pub config: Option<&'a str>,
    /// Crash-restart policy; the supervisor supplies it in #29.
    pub restart: Option<&'a str>,
}

impl<'a> Status<'a> {
    pub fn of(service: &'a Service) -> Self {
        Status {
            summary: Summary {
                id: &service.id,
                kind: service.kind,
                state: service.state,
                enabled: service.enabled,
                pid: service.pid,
                since: service.since,
                version: &service.version,
            },
            details: Details {
                dependencies: &service.deps,
                last_exit: service.last_exit,
                next_at: service.next_at,
                ready_since: service.ready_since,
                failures: service.failures,
                config: service.config.as_deref(),
                restart: None,
            },
        }
    }
}

const COLUMNS: [&str; 6] = ["ID", "KIND", "STATE", "AUTO", "PID", "VERSION"];

pub fn table(registry: &Registry) -> String {
    let headers: Vec<String> = COLUMNS.iter().map(|column| column.to_string()).collect();
    let rows: Vec<Vec<String>> = registry
        .values()
        .map(|service| {
            vec![
                service.id.clone(),
                service.kind.as_str().to_string(),
                service.state.as_str().to_string(),
                if service.enabled { "[auto]" } else { "-" }.to_string(),
                dash(service.pid),
                service.version.clone(),
            ]
        })
        .collect();

    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| header.chars().count())
        .collect();
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    push_row(&mut out, &headers, &widths);
    for row in &rows {
        push_row(&mut out, row, &widths);
    }
    out.trim_end().to_string()
}

fn push_row(out: &mut String, cells: &[String], widths: &[usize]) {
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        out.push_str(cell);
        if index + 1 < cells.len() {
            out.push_str(&" ".repeat(widths[index] - cell.chars().count()));
        }
    }
    out.push('\n');
}

pub fn text(status: &Status<'_>) -> String {
    let mut out = String::new();
    out.push_str("summary:\n");
    field(&mut out, "id", status.summary.id);
    field(&mut out, "kind", status.summary.kind.as_str());
    field(&mut out, "state", status.summary.state.as_str());
    field(&mut out, "enabled", status.summary.enabled);
    field(&mut out, "pid", dash(status.summary.pid));
    field(&mut out, "since", dash(status.summary.since));
    field(&mut out, "version", status.summary.version);

    out.push_str("details:\n");
    field(
        &mut out,
        "dependencies",
        if status.details.dependencies.is_empty() {
            "-".to_string()
        } else {
            status.details.dependencies.join(", ")
        },
    );
    field(&mut out, "last_exit", dash(status.details.last_exit));
    field(&mut out, "next_at", dash(status.details.next_at));
    field(&mut out, "ready_since", dash(status.details.ready_since));
    field(&mut out, "failures", status.details.failures);
    field(&mut out, "config", status.details.config.unwrap_or("-"));
    field(&mut out, "restart", status.details.restart.unwrap_or("-"));
    out.trim_end().to_string()
}

fn field(out: &mut String, name: &str, value: impl Display) {
    out.push_str(&format!("  {:<14}{}\n", format!("{}:", name), value));
}

fn dash<T: Display>(value: Option<T>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Ready, Service};
    use serde_json::Value;

    fn demo() -> Service {
        Service {
            id: "demo".into(),
            kind: Kind::Core,
            version: "1.2.3".into(),
            desc: "demo service".into(),
            deps: vec!["base".into()],
            caps: vec!["status".into()],
            exec: Some(crate::registry::Exec {
                path: "/bin/demo".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
            }),
            enabled: true,
            state: State::Running,
            pid: Some(42),
            since: Some(1_700_000_000),
            ready_since: Some(1_700_000_002),
            next_at: None,
            failures: 0,
            last_exit: Some(3),
            config: Some("/var/lib/svcd/demo/config".into()),
        }
    }

    #[test]
    fn json_has_the_fixed_summary_and_details_shape() {
        let service = demo();
        let value = serde_json::to_value(Status::of(&service)).expect("serialize");

        assert_eq!(
            value,
            serde_json::json!({
                "summary": {
                    "id": "demo",
                    "kind": "core",
                    "state": "running",
                    "enabled": true,
                    "pid": 42,
                    "since": 1_700_000_000u64,
                    "version": "1.2.3"
                },
                "details": {
                    "dependencies": ["base"],
                    "last_exit": 3,
                    "next_at": null,
                    "ready_since": 1_700_000_002u64,
                    "failures": 0,
                    "config": "/var/lib/svcd/demo/config",
                    "restart": null
                }
            })
        );

        let object = value.as_object().expect("object");
        assert_eq!(object.len(), 2);
        assert!(object.contains_key("summary"));
        assert!(object.contains_key("details"));
    }

    #[test]
    fn stopped_service_serializes_null_runtime_fields() {
        let mut service = demo();
        service.state = State::Stopped;
        service.enabled = false;
        service.pid = None;
        service.since = None;
        service.ready_since = None;
        service.last_exit = None;
        service.config = None;

        let value = serde_json::to_value(Status::of(&service)).expect("serialize");

        assert_eq!(value["summary"]["state"], "stopped");
        assert_eq!(value["summary"]["enabled"], false);
        assert!(value["summary"]["pid"].is_null());
        assert!(value["summary"]["since"].is_null());
        assert!(value["details"]["last_exit"].is_null());
        assert!(value["details"]["config"].is_null());
        assert!(value["details"]["restart"].is_null());
    }

    #[test]
    fn list_table_has_the_specified_columns() {
        let mut registry = Registry::new();
        registry.insert(demo()).expect("insert demo");

        let rendered = table(&registry);
        let lines: Vec<&str> = rendered.lines().collect();

        assert_eq!(lines[0].split_whitespace().collect::<Vec<_>>(), COLUMNS);
        assert!(lines[1].starts_with("demo"));
        assert!(lines[1].contains("core"));
        assert!(lines[1].contains("running"));
        assert!(lines[1].contains("[auto]"));
        assert!(lines[1].contains("42"));
        assert!(lines[1].contains("1.2.3"));
    }

    #[test]
    fn status_text_has_summary_and_details_sections() {
        let service = demo();
        let rendered = text(&Status::of(&service));

        assert!(rendered.contains("summary:"));
        assert!(rendered.contains("details:"));
        assert!(rendered.contains("id:"));
        assert!(rendered.contains("demo"));
        assert!(rendered.contains("dependencies:"));
        assert!(rendered.contains("base"));
        assert!(rendered.contains("last_exit:"));
        assert!(rendered.contains("failures:"));
        assert!(rendered.contains("restart:"));
    }

    #[test]
    fn a_missing_optional_value_renders_as_dash() {
        let mut service = demo();
        service.pid = None;
        service.next_at = None;
        service.config = None;

        let rendered = text(&Status::of(&service));
        let value: Value = serde_json::to_value(Status::of(&service)).expect("serialize");

        assert!(rendered.contains("pid:"));
        assert!(rendered.contains("-"));
        assert!(value["summary"]["pid"].is_null());
    }
}
