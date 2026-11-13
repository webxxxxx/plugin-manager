//! `svc status` payload and its text projection.
//!
//! `since`, `ready_since` and `next_at` are Unix epoch seconds; `last_exit` is
//! the last recorded exit code. Missing runtime values stay `null`/`-` until the
//! supervisor (#29) supplies them.

use std::fmt::Display;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::registry::{Kind, Service, State};
use crate::supervisor::Machine;

/// The fixed `svc status` payload. `svc list` and the text output are both
/// projections of this shape; MCP `svc.list` / `svc.describe` will reuse it when
/// the bridge lands (#32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub summary: Summary,
    pub details: Details,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    pub enabled: bool,
    pub pid: Option<u32>,
    pub since: Option<u64>,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Details {
    pub dependencies: Vec<String>,
    pub last_exit: Option<i32>,
    pub next_at: Option<u64>,
    pub ready_since: Option<u64>,
    pub failures: u32,
    pub config: Option<String>,
    /// Crash-restart policy; the supervisor supplies it in #29.
    pub restart: Option<String>,
    /// `true` only when a forced stop ran out of its kill window.
    pub unkillable: bool,
    /// Supplementary detail from the optional core status action; only
    /// `svc status` probes it, and its exit code never changes the service
    /// state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
}

impl Status {
    /// The on-disk/offline projection of a service definition.
    pub fn of(service: &Service) -> Self {
        Status {
            summary: Summary {
                id: service.id.clone(),
                kind: service.kind,
                state: service.state,
                enabled: service.enabled,
                pid: service.pid,
                since: service.since,
                version: service.version.clone(),
            },
            details: Details {
                dependencies: service.deps.clone(),
                last_exit: service.last_exit,
                next_at: service.next_at,
                ready_since: service.ready_since,
                failures: service.failures,
                config: service.config.clone(),
                restart: None,
                unkillable: false,
                probe: None,
            },
        }
    }

    /// The live projection of a supervised core service. Machine timers are
    /// monotonic; `epoch_at_start` anchors them back to epoch seconds.
    pub fn live(service: &Service, machine: &Machine, epoch_at_start: SystemTime) -> Self {
        let mut status = Status::of(service);
        status.summary.state = machine.state;
        status.summary.pid = machine.pid;
        status.summary.since = epoch_secs(epoch_at_start, machine.since_ms);
        status.details.last_exit = machine.last_exit;
        status.details.next_at = epoch_secs_opt(epoch_at_start, machine.next_at_ms);
        status.details.ready_since = epoch_secs_opt(epoch_at_start, machine.ready_since_ms);
        status.details.failures = machine.failures;
        status.details.restart = Some(if machine.enabled {
            "on-failure".into()
        } else {
            "no".into()
        });
        status.details.unkillable = machine.unkillable;
        status
    }

    /// The in-process `svc` service is always running and never supervised.
    pub fn builtin(service: &Service) -> Self {
        let mut status = Status::of(service);
        status.summary.state = State::Running;
        status.summary.enabled = true;
        status.summary.pid = None;
        status.summary.since = None;
        status.details.restart = Some("no".into());
        status
    }
}

fn epoch_secs(epoch_at_start: SystemTime, ms: u64) -> Option<u64> {
    if ms == 0 {
        return None;
    }
    Some(
        epoch_at_start
            .checked_add(Duration::from_millis(ms))
            .unwrap_or(epoch_at_start)
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    )
}

fn epoch_secs_opt(epoch_at_start: SystemTime, ms: Option<u64>) -> Option<u64> {
    ms.and_then(|value| epoch_secs(epoch_at_start, value))
}

const COLUMNS: [&str; 6] = ["ID", "KIND", "STATE", "AUTO", "PID", "VERSION"];

pub fn table(statuses: &[Status]) -> String {
    let headers: Vec<String> = COLUMNS.iter().map(|column| column.to_string()).collect();
    let rows: Vec<Vec<String>> = statuses
        .iter()
        .map(|status| {
            vec![
                status.summary.id.clone(),
                status.summary.kind.as_str().to_string(),
                status.summary.state.as_str().to_string(),
                if status.summary.enabled {
                    "[auto]"
                } else {
                    "-"
                }
                .to_string(),
                dash(status.summary.pid),
                status.summary.version.clone(),
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

pub fn text(status: &Status) -> String {
    let mut out = String::new();
    out.push_str("summary:\n");
    field(&mut out, "id", &status.summary.id);
    field(&mut out, "kind", status.summary.kind.as_str());
    field(&mut out, "state", status.summary.state.as_str());
    field(&mut out, "enabled", status.summary.enabled);
    field(&mut out, "pid", dash(status.summary.pid));
    field(&mut out, "since", dash(status.summary.since));
    field(&mut out, "version", &status.summary.version);

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
    field(
        &mut out,
        "config",
        status.details.config.as_deref().unwrap_or("-"),
    );
    field(
        &mut out,
        "restart",
        status.details.restart.as_deref().unwrap_or("-"),
    );
    field(&mut out, "unkillable", status.details.unkillable);
    if let Some(probe) = &status.details.probe {
        field(&mut out, "probe", probe);
    }
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
    use crate::registry::Ready;
    use crate::registry::Service;
    use serde_json::Value;

    fn demo() -> Service {
        Service {
            id: "demo".into(),
            kind: Kind::Core,
            version: "1.2.3".into(),
            desc: "demo service".into(),
            deps: vec!["base".into()],
            caps: Vec::new(),
            exec: Some(crate::registry::Exec {
                path: "/bin/demo".into(),
                args: Vec::new(),
                env: Default::default(),
                ready: Ready::Exec,
            }),
            lifecycle: None,
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
                    "restart": null,
                    "unkillable": false
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
        assert_eq!(value["details"]["unkillable"], false);
    }

    #[test]
    fn live_status_uses_the_machine_runtime_state() {
        use crate::supervisor::Machine;

        let service = demo();
        let mut machine = Machine::new(true);
        machine.state = State::Backoff;
        machine.pid = None;
        machine.since_ms = 1_700_000_000_000;
        machine.next_at_ms = Some(1_700_000_005_000);
        machine.failures = 2;
        machine.last_exit = Some(1);
        machine.unkillable = true;

        let status = Status::live(&service, &machine, UNIX_EPOCH);

        assert_eq!(status.summary.state, State::Backoff);
        assert_eq!(status.summary.pid, None);
        assert_eq!(status.summary.since, Some(1_700_000_000));
        assert_eq!(status.details.next_at, Some(1_700_000_005));
        assert_eq!(status.details.failures, 2);
        assert_eq!(status.details.last_exit, Some(1));
        assert_eq!(status.details.restart.as_deref(), Some("on-failure"));
        assert!(status.details.unkillable);
    }

    #[test]
    fn list_table_has_the_specified_columns() {
        let service = demo();
        let rendered = table(&[Status::of(&service)]);
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
        assert!(rendered.contains("unkillable:"));
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
