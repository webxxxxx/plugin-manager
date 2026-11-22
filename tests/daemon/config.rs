//! config daemon tests.

use super::*;

/// The boot wait for a late-mounted `services.d` was removed (ADR-0005
/// amendment): a fresh config tree must not stall every start. Before the
/// change this test failed after the daemon burned the budget.
#[test]
fn a_missing_services_dir_does_not_stall_boot() {
    let mut sandbox = Sandbox::new();
    fs::remove_dir_all(sandbox.config.join("services.d")).expect("remove services.d");
    // A stale document still names the removed key; it must be inert.
    fs::write(
        sandbox.config.join("svcd.conf"),
        "boot_scan_wait_ms = 30000\n",
    )
    .expect("write svcd.conf");

    let started = Instant::now();
    sandbox.start_daemon_ready();

    assert!(
        started.elapsed() < Duration::from_secs(5),
        "boot waited for a services.d that will never appear: {:?}\n{}",
        started.elapsed(),
        sandbox.daemon_log()
    );
}

/// #161: the daemon writes what it derived at startup, and `svc status
/// --resources` shows it back. Memory and cores are both injected through the
/// document, so the assertions do not depend on the machine running the suite;
/// the workload shape is derived, not stated.
///
/// The configuration is over budget: the invariant charges a stack per loop and
/// blocking thread, never per connection, so the pinned 4096 connections are a
/// memory cost and the report has exactly two overrides: the body's upper bound
/// and one invariant shrink.
#[test]
fn the_daemon_reports_the_memory_budget_it_derived() {
    let mut sandbox = Sandbox::new();
    // 8 GiB of free memory -> a 2 GiB budget (auto takes a quarter), i.e. the
    // streaming shape. The body is above that shape's share and the pinned
    // 4096 connections sit on the spec's own ceiling.
    fs::write(
        sandbox.config.join("svcd.conf"),
        "probe_available_memory = 8589934592\nprobe_cpus = 4\nmax_connections = 4096\nmax_body_bytes = 1073741824\n",
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
            .any(|line| line.starts_with("max_body_bytes 1073741824 ")
                && line.ends_with("above-upper-bound")),
        "{summary:?}"
    );
    // The second override is the invariant's: it pays with the connection count.
    let mut invariant = summary
        .iter()
        .filter(|line| line.ends_with("invariant-shrink"));
    let shrink = invariant.next().unwrap_or_else(|| panic!("{summary:?}"));
    assert!(invariant.next().is_none(), "{summary:?}");
    assert!(
        shrink.starts_with("max_complete_body_bytes ") || shrink.starts_with("max_connections "),
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
        "override: max_body_bytes: 1073741824 -> 536870912 (above-upper-bound)",
        "stated:  probe_available_memory, probe_cpus",
    ] {
        assert!(shown.contains(needle), "missing {needle} in:\n{shown}");
    }
    assert_eq!(shown.matches("override: ").count(), 2, "{shown}");
    assert!(shown.contains("(invariant-shrink)"), "{shown}");

    // The same report is in the startup log, once per override.
    let log = sandbox.daemon_log();
    assert!(
        log.contains("svcd memory:"),
        "log misses the budget:\n{log}"
    );
    assert!(
        log.contains("svcd workload: streaming (derived)"),
        "log misses the shape:\n{log}"
    );
    assert_eq!(
        log.matches("svcd override: ").count(),
        2,
        "each override is logged once:\n{log}"
    );
    assert!(
        log.contains("(invariant-shrink)"),
        "log misses the invariant shrink:\n{log}"
    );
}

/// `log = off` is the master switch: the daemon writes no log file at all.
#[test]
fn log_off_writes_no_log_file() {
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.conf"), "log = off\n").expect("write svcd.conf");
    sandbox.start_daemon_ready();

    let log = sandbox.logs.join("svcd.log");
    assert!(!log.exists(), "log = off still created {}", log.display());
    let stderr = fs::read_to_string(&sandbox.daemon_stderr).unwrap_or_default();
    assert!(stderr.is_empty(), "log = off still wrote stderr: {stderr}");
}

/// The write-side filter drops INFO records before they reach the file.
#[test]
fn log_level_error_drops_info_lines() {
    let mut sandbox = Sandbox::new();
    fs::write(sandbox.config.join("svcd.conf"), "log_level = error\n").expect("write svcd.conf");
    sandbox.start_daemon_ready();

    let text = fs::read_to_string(sandbox.logs.join("svcd.log")).expect("read svcd.log");
    assert!(
        !text.contains(" INFO "),
        "INFO leaked past the filter: {text}"
    );
    assert!(!text.contains("starting"), "startup line leaked: {text}");
}
