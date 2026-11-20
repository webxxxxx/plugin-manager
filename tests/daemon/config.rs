//! config daemon tests.

use super::*;

/// #161: the daemon writes what it derived at startup, and `svc status
/// --resources` shows it back. The resource description is injected through the
/// document, so the assertions do not depend on the machine running the suite;
/// two over-the-band values produce exactly two readable overrides.
#[test]
fn the_daemon_reports_the_memory_budget_it_derived() {
    let mut sandbox = Sandbox::new();
    // 8 GiB of free memory -> a 2 GiB budget (the build ceiling), i.e. the
    // streaming shape. The two explicit values are above that shape's share.
    fs::write(
        sandbox.config.join("svcd.conf"),
        "probe_available_memory = 8589934592\nmax_connections = 4096\nmax_body_bytes = 1073741824\n",
    )
    .expect("write svcd.conf");
    sandbox.start_daemon_ready();

    let snapshot = sandbox
        .data
        .join("run")
        .join("resources")
        .join("startup.json");
    let text = wait_for_file(&snapshot, || sandbox.daemon_log());
    let report: Value = serde_json::from_str(&text).expect("resource snapshot");
    for key in [
        "budget_raw",
        "budget_quantized",
        "budget_source",
        "build_cap",
        "workload",
        "workload_source",
        "worst_case",
        "notes",
    ] {
        assert!(report.get(key).is_some(), "snapshot misses {key}: {text}");
    }
    assert_eq!(report["budget_quantized"], 2u64 * 1024 * 1024 * 1024);
    assert_eq!(report["workload"], "streaming");
    assert!(
        report["worst_case"].as_u64().unwrap() <= report["budget_quantized"].as_u64().unwrap(),
        "the derivation must fit its own budget: {text}"
    );

    // Exactly the two rewrites, with their reason and both numbers.
    let notes = report["notes"].as_array().expect("notes");
    let summary: Vec<String> = notes
        .iter()
        .map(|note| {
            format!(
                "{} {} {} {}",
                note["key"].as_str().unwrap_or("?"),
                note["configured"],
                note["effective"],
                note["reason"].as_str().unwrap_or("?")
            )
        })
        .collect();
    assert_eq!(notes.len(), 2, "{summary:?}");
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("max_connections 4096 ")
                && line.ends_with("above-upper-bound")),
        "{summary:?}"
    );
    assert!(
        summary
            .iter()
            .any(|line| line.starts_with("max_body_bytes 1073741824 ")
                && line.ends_with("above-upper-bound")),
        "{summary:?}"
    );

    let output = sandbox.run_svc(&["status", "--resources"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shown = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "source:  last start",
        "memory:",
        "workload: streaming (derived)",
        "worst case:",
        "override: max_connections: 4096 ->",
        "override: max_body_bytes: 1073741824 ->",
        "stated:  probe_available_memory",
    ] {
        assert!(shown.contains(needle), "missing {needle} in:\n{shown}");
    }

    // The same report is in the startup log, once.
    let log = sandbox.daemon_log();
    assert!(
        log.contains("svcd: memory:"),
        "log misses the budget:\n{log}"
    );
    assert!(
        log.contains("svcd: workload: streaming (derived)"),
        "log misses the shape:\n{log}"
    );
    let overrides = log.matches("svcd: override: max_connections:").count();
    assert_eq!(overrides, 1, "each override is logged once:\n{log}");
}
