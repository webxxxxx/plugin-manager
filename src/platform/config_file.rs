//! Materializing the directory roots into `svcd.conf`.
//!
//! ADR-0023 makes the file, not the runtime, the single source of defaults.
//! This module fills missing roots once, generates a complete document when
//! none exists, and reports the writes that could not be persisted. It never
//! reads the environment: the caller decides whether this is the ordinary
//! bootstrap or the installer's clean boot environment.

use std::path::{Path, PathBuf};

use crate::platform::host::dirs;
use crate::platform::settings::{self, Settings};

/// Fill the three roots that are absent. `install_dir` is folded in here and
/// nowhere else: it supplies `<install_dir>/{config,data,logs}`, so a prefixed
/// install is self-contained. Explicit roots still win.
///
/// Returns the keys that were filled, in document order. The caller writes
/// exactly those keys back; explicit roots are never rewritten.
pub fn fill_missing(settings: &mut Settings) -> Vec<&'static str> {
    let (config, data, logs) = match settings.install_dir.as_ref() {
        Some(prefix) => (
            prefix.join("config"),
            prefix.join("data"),
            prefix.join("logs"),
        ),
        None => (
            dirs::default_config_dir(),
            dirs::default_data_dir(),
            dirs::default_logs_dir(),
        ),
    };
    fill_missing_with(settings, config, data, logs)
}

/// The same fill with the caller's already-resolved defaults.
///
/// The daemon passes the `SVCD_*`-aware [`crate::platform::paths::Paths`]
/// roots here: an environment override is the actual root and must be written
/// into the generated document, not replaced by a platform default.
pub fn fill_missing_with(
    settings: &mut Settings,
    config_default: PathBuf,
    data_default: PathBuf,
    logs_default: PathBuf,
) -> Vec<&'static str> {
    let mut filled = Vec::new();
    if settings.config_dir.is_none() {
        settings.config_dir = Some(config_default);
        filled.push("config_dir");
    }
    if settings.data_dir.is_none() {
        settings.data_dir = Some(data_default);
        filled.push("data_dir");
    }
    if settings.logs_dir.is_none() {
        settings.logs_dir = Some(logs_default);
        filled.push("logs_dir");
    }
    filled
}

/// The defaults a caller should pass to [`fill_missing_with`].
///
/// The already-resolved paths carry the `SVCD_*` overrides; a variable that is
/// set is the actual root. Otherwise `install_dir` supplies the missing
/// config/data/logs defaults, and finally the path resolver's default.
pub fn defaults_from_paths(
    settings: &Settings,
    paths: &crate::platform::paths::Paths,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> (PathBuf, PathBuf, PathBuf) {
    let config = if env("SVCD_CONFIG_DIR").is_some() {
        paths.config_dir.clone()
    } else {
        settings
            .install_dir
            .as_ref()
            .filter(|_| settings.config_dir.is_none())
            .map(|prefix| prefix.join("config"))
            .unwrap_or_else(|| paths.config_dir.clone())
    };
    let data = if env("SVCD_DATA_DIR").is_some() {
        paths.data_dir.clone()
    } else {
        settings
            .install_dir
            .as_ref()
            .filter(|_| settings.data_dir.is_none())
            .map(|prefix| prefix.join("data"))
            .unwrap_or_else(|| paths.data_dir.clone())
    };
    let logs = if env("SVCD_LOG_DIR").is_some() {
        paths.logs_dir.clone()
    } else {
        settings
            .install_dir
            .as_ref()
            .filter(|_| settings.logs_dir.is_none())
            .map(|prefix| prefix.join("logs"))
            .unwrap_or_else(|| paths.logs_dir.clone())
    };
    (config, data, logs)
}

/// Write the filled roots back to an existing document, preserving comments
/// and every key the operator wrote. A failed write is a warning: the in-memory
/// value still drives this run, and the next start retries.
pub fn update_filled(path: &Path, settings: &Settings, filled: &[&'static str]) -> Vec<String> {
    let mut warnings = Vec::new();
    for key in filled {
        let value = match *key {
            "config_dir" => settings.config_dir.as_ref(),
            "data_dir" => settings.data_dir.as_ref(),
            _ => settings.logs_dir.as_ref(),
        };
        let Some(value) = value else {
            continue;
        };
        if let Err(error) = settings::update_value(path, key, &value.display().to_string()) {
            warnings.push(format!(
                "cannot materialize {} in {}: {}; this run uses {}",
                key,
                path.display(),
                error,
                value.display()
            ));
        }
    }
    warnings
}

/// Which of the three roots the document on disk does not name.
///
/// Environment overrides are intentionally invisible here: a root set only in
/// the environment is still missing from the file, so the bootstrap writes the
/// resolved value once and the next boot without that variable stays put.
pub fn missing_roots(path: &Path) -> Vec<&'static str> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(_) => return vec!["config_dir", "data_dir", "logs_dir"],
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut found = [false; 3];
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "config_dir" => found[0] = true,
            "data_dir" => found[1] = true,
            "logs_dir" => found[2] = true,
            _ => {}
        }
    }
    ["config_dir", "data_dir", "logs_dir"]
        .into_iter()
        .zip(found)
        .filter_map(|(key, present)| (!present).then_some(key))
        .collect()
}

/// The complete document written when no `svcd.conf` exists.
///
/// The roots are absolute; derived directories appear only in comments, so a
/// reader can answer "where does this process write?" without reading code.
pub fn generated(settings: &Settings) -> String {
    let config_dir = settings
        .config_dir
        .as_ref()
        .expect("generated config needs a config root");
    let data_dir = settings
        .data_dir
        .as_ref()
        .expect("generated config needs a data root");
    let logs_dir = settings
        .logs_dir
        .as_ref()
        .expect("generated config needs a logs root");
    let install = settings
        .install_dir
        .as_ref()
        .map(|prefix| format!("install_dir = {}\n", prefix.display()))
        .unwrap_or_default();
    format!(
        "# Generated by svcd. Edit and restart the service to apply.\n\
         # Relative paths resolve against the directory containing this file.\n\
         # Every key can be overridden by SVCD_<KEY> in the environment.\n\
         # The three roots below are the only configurable directories.\n\
         # Everything else derives from them; those names are not keys.\n\
         bind = 127.0.0.1:8787\n\
         allow_insecure_install = false\n\
         config_dir = {config_dir}\n\
         data_dir = {data_dir}\n\
         logs_dir = {logs_dir}\n\
         {install}\
         temp_retention_secs = 7200\n\
         temp_cleanup_interval_secs = 300\n\
         # Derived, read-only layout:\n\
         #   services.d = {services}\n\
         #   trust.d    = {trust}\n\
         #   plugins    = {plugins}\n\
         #   temp       = {temp}\n\
         #   run        = {run}\n\
         # Optional resource limits. On every start svcd derives a memory\n\
         # budget from this machine (free memory, the cgroup v2 limit, CPU\n\
         # count) and spends it according to a workload shape. MEM_LEVEL\n\
         # (low|middle|high, default high) is the ceiling the budget can never\n\
         # exceed: 128 MiB / 512 MiB / 2 GiB. Values below are comments only;\n\
         # uncomment to override. Expected values at each shape's reference\n\
         # budget (tiny@128MiB, interactive@512MiB, streaming@2GiB):\n\
         # log_max_bytes = 65536 / 65536 / 65536\n\
         # log_rotations = 3 / 3 / 3\n\
         # log_to_file = on\n\
         # mem_budget_bytes = auto (25% of free memory, capped by MEM_LEVEL)\n\
         # workload = tiny | interactive | streaming (default: from the budget,\n\
         #   below 2 GiB of budget is interactive, below 512 MiB is tiny)\n\
         # Every probe input may be stated instead of read; a stated item wins\n\
         # and the matching OS read is skipped (0 means unknown):\n\
         # probe_available_memory / probe_cgroup_remaining / probe_cpus /\n\
         # probe_install_free_disk / probe_temp_free_disk\n\
         # request_stack_bytes = 131072 / 196608 / 262144\n\
         # io_stack_bytes = 65536 / 98304 / 131072\n\
         # blocking_threads = 2 (default; clamped 2..8). The blocking pool\n\
         # runs face handlers that cannot run on the server loop; a full\n\
         # queue answers 503 busy. Unix only in this slice; Windows keeps\n\
         # the per-connection fallback. The default Unix topology is four\n\
         # daemon threads: the supervisor loop (which also polls plugin\n\
         # stdio), the server loop and these two workers. Raising the value\n\
         # is an operator choice.\n\
         # max_body_bytes = 8388608 / 67108864 / 536870912\n\
         # max_json_body_bytes = 65536 / 262144 / 1048576\n\
         # max_complete_body_bytes = 65536 / 1048576 / 2097152\n\
         # max_header_bytes = 8192 / 16384 / 65536\n\
         # max_streams = 8 / 16 / 64\n\
         # stream_buffer_bytes = 8192 / 32768 / 131072\n\
         # max_connections = 24 / 128 / 682\n\
         # max_connections_per_ip = 6 / 32 / 170\n\
         # max_subscriptions = 4 / 16 / 64\n\
         # max_subscribers_per_plugin = 4 / 16 / 64\n\
         # http_idle_timeout_secs = 30 / 60 / 120\n\
         # max_package_bytes = 8388608 / 67108864 / 536870912\n\
         # transfer_timeout_secs = 120 / 120 / 120\n\
         # control_timeout_secs = 30 (clamped 1..300). A management route's\n\
         # control round trip returns 504 when the supervisor does not answer\n\
         # in time; 503 still means only that the blocking queue is full.\n\
         # Requires a daemon restart.\n\
         # default_timeout = 30000 / 30000 / 30000\n\
         # max_timeout = 300000 / 300000 / 300000\n\
         # ws_max_message_bytes = 1048576 / 1048576 / 1048576\n\
         # ws_inflight_bytes = 2097152 / 8388608 / 67108864\n\
         # ws_inflight_requests = 4 / 16 / 128\n\
         # ws_buffer_shrink_bytes = 65536\n\
         # ws_raw_enabled = on\n\
         # ws_raw_max_bytes = 1048576\n\
         # ws_raw_queue = 8 / 32 / 128\n\
         # pending_queue = 32 / 128 / 512 (management requests queued between\n\
         #   the server loop and the supervisor loop)\n\
         # progress_queue = 16 / 64 / 256 (progress events queued for one MCP\n\
         #   stream)\n\
         # A value outside [absolute floor, min(share x budget, absolute\n\
         # ceiling)] is rewritten and logged as an override; the worst case is\n\
         # checked against the budget before serving, and a configuration that\n\
         # cannot fit refuses to start. `svc config tune` prints the\n\
         # configuration this machine suggests; `svc status --resources` shows\n\
         # what the last start ran with.\n\
         # ws_raw_* gate the protocol-3 raw transport; on Windows /raw answers\n\
         # 501 not_supported. See svcd.conf.example for the full semantics.\n\
         # On Windows the HTTP fallback keeps one thread per admitted\n\
         # connection, so max_connections is its thread cap; the final Unix\n\
         # four-thread default budget does not apply there.\n\
         # max_streams caps long-lived and transfer streams; one-shot plugin UI\n\
         # static assets are exempt, so a page with more assets still loads.\n\
         # stream_buffer_bytes bounds one handler's output buffer.\n\
         # Set the web/API password with: svc lan passwd <password>\n\
         # auth_passwd = pbkdf2-sha256$...\n",
        config_dir = config_dir.display(),
        data_dir = data_dir.display(),
        logs_dir = logs_dir.display(),
        install = install,
        services = config_dir.join("services.d").display(),
        trust = config_dir.join("trust.d").display(),
        plugins = data_dir.join("plugins").display(),
        temp = data_dir.join("temp").display(),
        run = data_dir.join("run").display(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::settings::Settings;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = crate::test_support::temp_dir().join(format!(
            "svcd-config-file-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            name
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn fill_uses_the_install_prefix_for_all_three_roots() {
        let mut settings = Settings {
            install_dir: Some(std::path::PathBuf::from("/srv/svcd")),
            ..Settings::default()
        };
        let filled = fill_missing(&mut settings);
        assert_eq!(filled, vec!["config_dir", "data_dir", "logs_dir"]);
        assert_eq!(
            settings.config_dir,
            Some(std::path::PathBuf::from("/srv/svcd/config"))
        );
        assert_eq!(
            settings.data_dir,
            Some(std::path::PathBuf::from("/srv/svcd/data"))
        );
        assert_eq!(
            settings.logs_dir,
            Some(std::path::PathBuf::from("/srv/svcd/logs"))
        );
    }

    #[test]
    fn defaults_from_paths_maps_the_install_prefix_until_a_root_is_explicit() {
        let settings = Settings {
            install_dir: Some(std::path::PathBuf::from("/srv/svcd")),
            ..Settings::default()
        };
        let paths = crate::platform::paths::Paths::resolve_with(&settings, |_| None);
        let (config, data, logs) = defaults_from_paths(&settings, &paths, |_| None);
        assert_eq!(config, std::path::PathBuf::from("/srv/svcd/config"));
        assert_eq!(data, std::path::PathBuf::from("/srv/svcd/data"));
        assert_eq!(logs, std::path::PathBuf::from("/srv/svcd/logs"));

        let explicit = Settings {
            config_dir: Some(std::path::PathBuf::from("/etc/svcd")),
            data_dir: Some(std::path::PathBuf::from("/var/lib/svcd")),
            logs_dir: Some(std::path::PathBuf::from("/var/log/svcd")),
            install_dir: Some(std::path::PathBuf::from("/srv/svcd")),
            ..Settings::default()
        };
        let paths = crate::platform::paths::Paths::resolve_with(&explicit, |_| None);
        let (config, data, logs) = defaults_from_paths(&explicit, &paths, |_| None);
        assert_eq!(config, std::path::PathBuf::from("/etc/svcd"));
        assert_eq!(data, std::path::PathBuf::from("/var/lib/svcd"));
        assert_eq!(logs, std::path::PathBuf::from("/var/log/svcd"));
    }

    #[test]
    fn fill_keeps_explicit_roots_and_reports_only_the_gaps() {
        let mut settings = Settings {
            config_dir: Some(std::path::PathBuf::from("/conf")),
            data_dir: Some(std::path::PathBuf::from("/data")),
            ..Settings::default()
        };
        let filled = fill_missing(&mut settings);
        assert_eq!(filled, vec!["logs_dir"]);
        assert_eq!(settings.config_dir, Some(std::path::PathBuf::from("/conf")));
        assert_eq!(settings.data_dir, Some(std::path::PathBuf::from("/data")));
    }

    #[test]
    fn update_writes_only_the_filled_roots_and_keeps_comments() {
        let dir = temp("update");
        let path = dir.join("svcd.conf");
        std::fs::write(&path, "# keep me\nbind = 127.0.0.1:1\n").expect("write");
        let config = dir.join("config");
        let data = dir.join("data");
        let logs = dir.join("logs");
        let mut settings = Settings::default();
        let filled = fill_missing_with(&mut settings, config.clone(), data.clone(), logs.clone());
        assert_eq!(filled, vec!["config_dir", "data_dir", "logs_dir"]);
        assert!(update_filled(&path, &settings, &filled).is_empty());
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("# keep me\n"), "{text}");
        assert!(
            text.contains(&format!("config_dir = {}\n", config.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("data_dir = {}\n", data.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("logs_dir = {}\n", logs.display())),
            "{text}"
        );
        assert!(text.contains("bind = 127.0.0.1:1\n"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generated_writes_roots_and_derives_the_rest_in_comments() {
        let config = std::path::PathBuf::from("conf");
        let data = std::path::PathBuf::from("data");
        let logs = std::path::PathBuf::from("logs");
        let prefix = std::path::PathBuf::from("prefix");
        let settings = Settings {
            config_dir: Some(config.clone()),
            data_dir: Some(data.clone()),
            logs_dir: Some(logs.clone()),
            install_dir: Some(prefix.clone()),
            ..Settings::default()
        };
        let text = generated(&settings);
        assert!(
            text.contains(&format!("config_dir = {}\n", config.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("data_dir = {}\n", data.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("logs_dir = {}\n", logs.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!("install_dir = {}\n", prefix.display())),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "#   services.d = {}",
                config.join("services.d").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "#   plugins    = {}",
                data.join("plugins").display()
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!("#   run        = {}", data.join("run").display())),
            "{text}"
        );
        assert!(!text.contains("\nplugins_dir = "), "{text}");
        assert!(!text.contains("\nservices_dir = "), "{text}");
        assert!(!text.contains("\ntemp_dir = "), "{text}");
    }
}
