//! `svc config tune`: the configuration this machine suggests.
//!
//! One `key = value` line per derived limit, with the derivation chain in a
//! trailing comment: machine → memory budget → workload → key. `--write` puts
//! the same text in a delimited block inside `svcd.conf`, so the next start
//! runs with exactly those numbers; deleting the block returns to derivation.
//!
//! Disk figures are listed separately: free space sizes the package and temp
//! areas, it never enters the memory budget.

use serde_json::Value;

use crate::platform::limits::{describe_snapshot, Limits, ResourceReport};
use crate::platform::resources::bytes_text;

/// The delimiters of the block `svc config tune --write` owns.
pub const BLOCK_BEGIN: &str = "# BEGIN svcd config tune";
pub const BLOCK_END: &str = "# END svcd config tune";

/// The recommended configuration as a `svcd.conf` fragment.
pub fn config_text(limits: &Limits, report: &ResourceReport) -> String {
    let budget = report.budget_bytes();
    let workload = report.workload.as_str();
    let mut out = String::new();
    out.push_str("# svcd suggested configuration for this machine.\n");
    out.push_str(&format!("# machine: {}\n", report.profile.describe()));
    out.push_str(&format!(
        "# memory budget: {} usable (raw {}, {} source); build cap {}\n",
        bytes_text(budget),
        bytes_text(report.budget.raw),
        report.budget.source.as_str(),
        bytes_text(report.cap)
    ));
    out.push_str(&format!(
        "# workload: {} ({}); the shape spends the budget, it does not change it\n",
        workload,
        report.workload_source.as_str()
    ));
    out.push_str(
        "# Every line below may be deleted: an absent key is derived from the memory\n\
         # budget and the workload again on the next start. A value outside\n\
         # [absolute floor, min(share x budget, absolute ceiling)] is rewritten and\n\
         # reported instead of being applied silently.\n",
    );

    out.push_str(&text_line(
        "workload",
        workload,
        "tiny below 512 MiB, interactive below 2 GiB, else streaming",
    ));
    out.push_str(&count_line(
        "mem_budget_bytes",
        budget,
        &format!(
            "this machine's {} quantized; `auto` re-probes at every start",
            bytes_text(budget)
        ),
    ));

    let byte_keys: &[(&str, u64)] = &[
        ("request_stack_bytes", limits.request_stack_bytes as u64),
        ("io_stack_bytes", limits.io_stack_bytes as u64),
        ("max_body_bytes", limits.max_body),
        ("max_json_body_bytes", limits.max_json_body),
        ("max_complete_body_bytes", limits.max_complete_body),
        ("max_header_bytes", limits.max_header as u64),
        ("stream_buffer_bytes", limits.stream_buffer as u64),
        ("log_max_bytes", limits.log_max_bytes),
        ("max_package_bytes", limits.max_package_bytes),
        ("ws_max_message_bytes", limits.ws_max_message_bytes as u64),
        ("ws_inflight_bytes", limits.ws_inflight_bytes as u64),
        (
            "ws_buffer_shrink_bytes",
            limits.ws_buffer_shrink_bytes as u64,
        ),
        ("ws_raw_max_bytes", limits.ws_raw_max_bytes as u64),
    ];
    for (key, value) in byte_keys {
        out.push_str(&bytes_line(key, *value, budget, workload));
    }

    let count_keys: &[(&str, u64)] = &[
        ("blocking_threads", limits.blocking_threads as u64),
        ("blocking_queue", limits.blocking_queue as u64),
        ("max_streams", limits.max_streams as u64),
        ("max_connections", limits.max_connections as u64),
        (
            "max_connections_per_ip",
            limits.max_connections_per_ip as u64,
        ),
        ("max_subscriptions", limits.max_subscriptions as u64),
        (
            "max_subscribers_per_plugin",
            limits.max_subscribers_per_plugin as u64,
        ),
        ("http_idle_timeout_secs", limits.idle_timeout.as_secs()),
        ("log_rotations", limits.log_rotations as u64),
        ("transfer_timeout_secs", limits.transfer_timeout.as_secs()),
        ("control_timeout_secs", limits.control_timeout.as_secs()),
        ("default_timeout", limits.default_timeout.as_millis() as u64),
        ("max_timeout", limits.max_timeout.as_millis() as u64),
        ("ws_inflight_requests", limits.ws_inflight_requests as u64),
        ("ws_raw_queue", limits.ws_raw_queue as u64),
        ("pending_queue", limits.pending_queue as u64),
        ("progress_queue", limits.progress_queue as u64),
    ];
    for (key, value) in count_keys {
        out.push_str(&count_line(
            key,
            *value,
            &format!(
                "derived at the {workload} shape from the {} budget",
                bytes_text(budget)
            ),
        ));
    }

    out.push_str(&count_line(
        "cpu_budget_percent",
        limits.cpu_budget_percent as u64,
        "0 disables the CPU governor; 1..100 caps this process with a kernel \
         cap plus admission shedding",
    ));

    out.push_str("#\n");
    out.push_str("# disk (does not enter the memory budget):\n");
    out.push_str(&format!(
        "# install volume: {}\n",
        report
            .profile
            .install_free_disk
            .map(bytes_text)
            .unwrap_or_else(|| "unknown".into())
    ));
    out.push_str(&format!(
        "# temp volume:    {} (packages and temporary files; retention stays under\n\
         #                   temp_retention_secs / temp_cleanup_interval_secs)\n",
        report
            .profile
            .temp_free_disk
            .map(bytes_text)
            .unwrap_or_else(|| "unknown".into())
    ));
    out
}

/// `svc status --resources`: the last start's snapshot, explained.
pub fn resources_text(
    snapshot: &Value,
    origin: &str,
    stated: &crate::platform::resources::ProbeOverrides,
) -> String {
    let mut out = format!("source:  {}\n", origin);
    for line in describe_snapshot(snapshot) {
        out.push_str(&line);
        out.push('\n');
    }
    let names = stated.stated_names();
    out.push_str(&format!(
        "stated:  {}\n",
        if names.is_empty() {
            "none (every input was probed)".to_string()
        } else {
            names.join(", ")
        }
    ));
    out
}

fn text_line(key: &str, value: &str, comment: &str) -> String {
    row(&format!("{} = {}", key, value), comment)
}

fn count_line(key: &str, value: u64, comment: &str) -> String {
    text_line(key, &value.to_string(), comment)
}

/// A byte key: its share of the budget is the readable part of the derivation.
fn bytes_line(key: &str, value: u64, budget: u64, workload: &str) -> String {
    let share = if budget == 0 {
        "the budget".to_string()
    } else {
        format!(
            "{:.1}% of the {} budget",
            (value as f64 / budget as f64) * 100.0,
            bytes_text(budget)
        )
    };
    row(
        &format!("{} = {}", key, value),
        &format!("{} at the {} shape", share, workload),
    )
}

/// One aligned row with its trailing comment.
fn row(assignment: &str, comment: &str) -> String {
    const COLUMN: usize = 40;
    let padding = COLUMN.saturating_sub(assignment.chars().count()).max(1);
    format!("{}{}# {}\n", assignment, " ".repeat(padding), comment)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::limits::derive_with;
    use crate::platform::resources::{ResourceProfile, Workload};
    use crate::platform::settings::{self, Settings};
    use std::path::{Path, PathBuf};

    fn derived(profile: ResourceProfile, settings: Settings) -> (Limits, ResourceReport) {
        let derived = derive_with(&settings, &profile);
        (derived.limits, derived.report)
    }

    fn machine() -> ResourceProfile {
        ResourceProfile {
            available_memory: Some(4 * 1024 * 1024 * 1024),
            total_memory: Some(8 * 1024 * 1024 * 1024),
            cpus: Some(4),
            install_free_disk: Some(20 * 1024 * 1024 * 1024),
            temp_free_disk: Some(10 * 1024 * 1024 * 1024),
            ..ResourceProfile::default()
        }
    }

    /// Records every volume the real probe asks about.
    #[derive(Default)]
    struct Recording {
        disks: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl Recording {
        fn disks(&self) -> Vec<PathBuf> {
            self.disks.lock().expect("recording").clone()
        }
    }

    impl crate::platform::resources::Machine for Recording {
        fn read(&self, _path: &Path) -> Option<String> {
            None
        }

        fn physical_memory(&self) -> Option<(u64, u64)> {
            Some((16 * 1024 * 1024 * 1024, 32 * 1024 * 1024 * 1024))
        }

        fn cpu_count(&self) -> Option<usize> {
            Some(4)
        }

        fn free_disk(&self, path: &Path) -> Option<u64> {
            self.disks
                .lock()
                .expect("recording")
                .push(path.to_path_buf());
            Some(10 * 1024 * 1024 * 1024)
        }
    }

    #[test]
    fn materialized_roots_probe_data_dir_and_data_temp_for_both_views() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-probe-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let mut settings = Settings {
            install_dir: Some(root.join("prefix")),
            ..Settings::default()
        };
        crate::platform::config_file::fill_missing(&mut settings);
        let data = settings.data_dir.clone().expect("data root");
        let logs = settings.logs_dir.clone().expect("logs root");
        let recording = Recording::default();
        let config = crate::platform::config::Config::bootstrap_with(&settings, None, &recording)
            .expect("bootstrap");

        assert_eq!(config.paths.data_dir, data);
        assert_eq!(config.paths.temp_dir, data.join("temp"));
        assert_eq!(
            recording.disks(),
            vec![data.clone(), data.join("temp")],
            "the install figure uses <data>, the temp figure uses <data>/temp"
        );
        assert!(
            !recording.disks().iter().any(|path| path.starts_with(&logs)),
            "logs_dir must not be a probe root"
        );

        let tuned = config_text(&config.limits, &config.resources);
        assert!(tuned.contains("# temp volume:"), "{tuned}");
        let status = resources_text(
            &config.resources.to_json(),
            "this process",
            &settings.probe_overrides(),
        );
        assert!(status.contains("this process"), "{status}");
    }

    #[test]
    fn the_tuning_output_names_the_chain_from_the_machine_to_each_key() {
        let (limits, report) = derived(machine(), Settings::default());
        let text = config_text(&limits, &report);

        assert!(text.contains("# machine: free memory 4.0 GiB, 50% of 8.0 GiB total, 4 cpus"));
        assert!(text.contains("memory budget: 1.0 GiB usable"));
        assert!(text.contains("# workload: interactive (derived)"));
        assert!(text.contains("workload = interactive"));
        assert!(text.contains(&format!("mem_budget_bytes = {}", 1024 * 1024 * 1024)));
        assert!(text.contains(&format!("max_connections = {}", limits.max_connections)));
        assert!(text.contains("% of the 1.0 GiB budget"));
        assert!(text.contains("# disk (does not enter the memory budget):"));
        assert!(text.contains("# install volume: 20.0 GiB"));
    }

    #[test]
    fn the_tuning_output_names_the_cpu_budget_and_the_blocking_queue() {
        // Both keys were added after the first version of this output; naming
        // them here keeps the block complete as new limits land.
        let (limits, report) = derived(machine(), Settings::default());
        let text = config_text(&limits, &report);
        assert!(
            text.contains(&format!(
                "cpu_budget_percent = {}",
                limits.cpu_budget_percent
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("blocking_queue = {}", limits.blocking_queue)),
            "{text}"
        );
    }

    #[test]
    fn the_tuning_output_parses_as_a_real_document() {
        // The trailing comment must be part of the format, not something the
        // operator has to strip before the file loads.
        let (limits, report) = derived(machine(), Settings::default());
        let text = config_text(&limits, &report);
        let parsed = settings::parse(&text, std::path::Path::new("/etc/svcd"))
            .expect("tune output must load as svcd.conf");

        assert_eq!(parsed.workload, Some(Workload::Interactive));
        assert_eq!(parsed.mem_budget_bytes, Some(limits_budget(&report)));
        assert_eq!(parsed.max_connections, Some(limits.max_connections));
        assert_eq!(parsed.pending_queue, Some(limits.pending_queue));
    }

    fn limits_budget(report: &ResourceReport) -> u64 {
        report.budget_bytes()
    }

    #[test]
    fn writing_the_output_back_reproduces_the_same_limits() {
        // A tuned config that names every key must derive the same limits the
        // running process has: "写入后运行时以 svcd.conf 为准" has to be true.
        let (limits, report) = derived(machine(), Settings::default());
        let text = config_text(&limits, &report);
        let parsed = settings::parse(&text, std::path::Path::new("/etc/svcd")).expect("parse");
        let again = derive_with(&parsed, &ResourceProfile::default());

        assert_eq!(again.limits.max_connections, limits.max_connections);
        assert_eq!(again.limits.max_complete_body, limits.max_complete_body);
        assert_eq!(again.limits.pending_queue, limits.pending_queue);
        assert_eq!(again.report.refusal, None);
    }

    #[test]
    fn the_status_text_comes_from_the_snapshot() {
        let (_limits, report) = derived(machine(), Settings::default());
        let text = resources_text(
            &report.to_json(),
            "last start",
            &crate::platform::resources::ProbeOverrides::default(),
        );

        assert!(text.contains("source:  last start"));
        assert!(text.contains("workload: interactive (derived)"));
        assert!(text.contains("overrides: none"));
        assert!(text.contains("stated:  none"));
    }

    #[test]
    fn a_stated_probe_input_is_named_in_the_status_text() {
        let (_limits, report) = derived(machine(), Settings::default());
        let stated = crate::platform::resources::ProbeOverrides {
            available_memory: Some(4 * 1024 * 1024 * 1024),
            ..Default::default()
        };
        let text = resources_text(&report.to_json(), "last start", &stated);

        assert!(text.contains("stated:  probe_available_memory"), "{text}");
    }

    #[test]
    fn an_override_shows_up_in_the_status_text() {
        let settings = Settings {
            max_body_bytes: Some(1024 * 1024 * 1024),
            ..Settings::default()
        };
        let (_limits, report) = derived(machine(), settings);
        let text = resources_text(
            &report.to_json(),
            "last start",
            &crate::platform::resources::ProbeOverrides::default(),
        );

        assert!(text.contains("override: max_body_bytes: 1073741824 ->"));
        assert!(text.contains("(above-upper-bound)"));
    }
}
