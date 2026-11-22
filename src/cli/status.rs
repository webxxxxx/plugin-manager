//! `svc status` text projection.
//!
//! The payload itself is a kernel value ([`crate::kernel::projection::Status`]);
//! this module only renders it for a terminal.

use std::fmt::Display;

use crate::kernel::projection::Status;

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
    if let Some(name) = &status.summary.display_name {
        field(&mut out, "name", name);
    }
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
    use crate::kernel::fixtures::test_service;
    use serde_json::Value;

    #[test]
    fn list_table_has_the_specified_columns() {
        let rendered = table(&[Status::of(&test_service())]);
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
        let rendered = text(&Status::of(&test_service()));

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
        let mut service = test_service();
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
