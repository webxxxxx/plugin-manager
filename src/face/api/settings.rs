//! Settings screen endpoints (#263): read the whole documented surface with
//! its provenance, and write the subset the console may edit.
//!
//! `GET` reports, per key, what the file says, whether an environment override
//! wins, what the daemon actually runs with, and the metadata the console shows
//! in the tooltip. `PUT` validates against a throw-away `Settings`, writes the
//! document in one read-modify-write (comments and untouched keys survive), and
//! asks the daemon to restart.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use super::*;

// `settings` is this module; the parser lives in the platform layer.
use crate::platform::config_edit as conf;
use crate::platform::settings;

impl ApiFace {
    pub(super) fn settings(&self, _call: &ApiCall<'_>) -> Response {
        self.settings_rpc()
    }

    pub(super) fn settings_put(&self, call: &ApiCall<'_>) -> Response {
        self.apply_settings_rpc(call.budget(), call.body())
    }

    fn settings_rpc(&self) -> Response {
        let path = self.config.settings_path.as_deref();
        let text = path
            .and_then(|path| fs::read_to_string(path).ok())
            .unwrap_or_default();
        let file = conf::file_values(&text);
        let bind = self
            .lan
            .lock()
            .ok()
            .map(|state| state.status().bind.to_string());
        let keys: Vec<Value> = crate::platform::config_doc::CONF_KEYS
            .iter()
            .map(|entry| {
                let env = self.config.env.get(entry.key).cloned();
                let source = if env.is_some() {
                    "env"
                } else if file.contains_key(entry.key) {
                    "file"
                } else {
                    "derived"
                };
                let value = env.or_else(|| file.get(entry.key).cloned());
                json!({
                    "key": entry.key,
                    "group": entry.group.title(),
                    "kind": entry.kind.code(),
                    "default": entry.default,
                    "doc": entry.doc,
                    "recommend": entry.recommend,
                    "editable": entry.editable_in_console,
                    "needsRestart": entry.needs_restart,
                    "source": source,
                    "value": value,
                    "effective": effective(&self.config, entry.key, bind.as_deref()),
                })
            })
            .collect();
        Response::json(
            200,
            &json!({
                "path": path.map(|path| path.to_string_lossy()),
                "keys": keys,
            }),
        )
    }

    /// `{"values": {"<key>": "<value>" | null}}`; `null` (or `""`) removes the
    /// key from the document, which is how a field returns to its default.
    fn apply_settings_rpc(&self, budget: &CallBudget, body: &[u8]) -> Response {
        let Some(path) = self.config.settings_path.clone() else {
            return error_response(500, "internal", "svcd.conf path is unavailable");
        };
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return error_response(400, "bad_request", "body must be a JSON object");
        };
        let Some(values) = value.get("values").and_then(Value::as_object) else {
            return error_response(
                400,
                "bad_request",
                "body must look like {\"values\": {\"key\": \"value\"}}",
            );
        };
        let base = path.parent().unwrap_or(Path::new("."));
        let mut set = BTreeMap::new();
        let mut unset = Vec::new();
        let mut saved = BTreeSet::new();
        for (key, value) in values {
            let Some(entry) = crate::platform::config_doc::CONF_KEYS
                .iter()
                .find(|entry| entry.key == key)
            else {
                return error_response(400, "bad_request", &format!("unknown key: {}", key));
            };
            if !entry.editable_in_console {
                return error_response(
                    400,
                    "bad_request",
                    &format!("{} is read-only in the console", key),
                );
            }
            let text = match value {
                Value::Null => None,
                Value::String(text) if text.is_empty() => None,
                Value::String(text) => Some(text.clone()),
                _ => {
                    return error_response(
                        400,
                        "bad_request",
                        &format!("{} must be a string or null", key),
                    )
                }
            };
            match text {
                None => unset.push(key.clone()),
                Some(text) if key == "auth_passwd" => {
                    // The console sends the plaintext over the loopback API; the
                    // document only ever holds the pbkdf2 hash.
                    match crate::platform::auth::hash_password(&text) {
                        Ok(hash) => {
                            set.insert(key.clone(), hash);
                        }
                        Err(error) => {
                            return error_response(
                                400,
                                "bad_request",
                                &format!("password: {}", error),
                            )
                        }
                    }
                }
                Some(text) => {
                    // A value is one document line; a newline would smuggle a
                    // second assignment (including a read-only key) past the
                    // per-key validation below.
                    if text.contains(['\n', '\r', '\0']) {
                        return error_response(
                            400,
                            "bad_request",
                            &format!("{} must be a single line", key),
                        );
                    }
                    if let Err(error) = settings::parse(&format!("{} = {}", key, text), base) {
                        return error_response(400, "bad_request", &error);
                    }
                    set.insert(key.clone(), text);
                }
            }
            saved.insert(key.clone());
        }
        if saved.is_empty() {
            return Response::json(200, &json!({"saved": [], "restarting": false}));
        }
        // Validate the merged document, not just the submitted lines: the
        // mutually exclusive budget keys may already be half-set on disk.
        let mut merged = conf::file_values(&fs::read_to_string(&path).unwrap_or_default());
        for key in &unset {
            merged.remove(key);
        }
        for (key, value) in &set {
            merged.insert(key.clone(), value.clone());
        }
        if merged.contains_key("mem_budget_bytes") && merged.contains_key("mem_budget_percent") {
            return error_response(
                400,
                "bad_request",
                "mem_budget_bytes and mem_budget_percent are mutually exclusive; clear one first",
            );
        }
        // Build the next document before writing it: only the named keys may
        // change, and the result must still parse as the same document.
        let current = fs::read_to_string(&path).unwrap_or_default();
        let next = conf::rewrite_text(&current, &set, &unset);
        if let Err(error) = settings::parse(&next, base) {
            return error_response(400, "bad_request", &error);
        }
        let before = conf::file_values(&current);
        let after = conf::file_values(&next);
        let only_named = before.iter().all(|(key, value)| {
            if unset.iter().any(|candidate| candidate == key) {
                !after.contains_key(key)
            } else if set.contains_key(key) {
                true
            } else {
                after.get(key) == Some(value)
            }
        }) && after
            .keys()
            .all(|key| before.contains_key(key) || set.contains_key(key));
        if !only_named {
            return error_response(
                400,
                "bad_request",
                "the edit would change a key the request did not name",
            );
        }
        if let Err(error) = fs::write(&path, &next) {
            return error_response(
                500,
                "internal",
                &format!("write {}: {}", path.display(), error),
            );
        }
        let saved: Vec<&String> = saved.iter().collect();
        // Every file-backed change needs a restart (nothing is hot except
        // `svc lan passwd`, which does not go through this route).
        match self.rpc(budget, "svcd.restart", json!({})) {
            Ok(restart) => {
                let mut response = json!({
                    "saved": saved,
                    "restarting": restart.get("restarting").and_then(Value::as_bool).unwrap_or(false),
                });
                if let Some(hint) = restart.get("hint") {
                    response["hint"] = hint.clone();
                }
                Response::json(200, &response)
            }
            Err(_) => Response::json(
                200,
                &json!({
                    "saved": saved,
                    "restarting": false,
                    "hint": "saved; restart the daemon for this to take effect",
                }),
            ),
        }
    }
}

/// What the running daemon actually uses for one key. `null` means "this key is
/// not a derived runtime value" (paths, probe inputs, the password hash).
fn effective(config: &crate::platform::config::Config, key: &str, bind: Option<&str>) -> Value {
    let limits = &config.limits;
    let settings = &config.settings;
    let paths = &config.paths;
    match key {
        "mem_budget_bytes" => json!(config.resources.budget_bytes()),
        "workload" => json!(config.resources.workload.as_str()),
        "mem_budget_percent" => json!(settings.mem_budget_percent),
        "cpu_budget_percent" => json!(limits.cpu_budget_percent),
        "request_stack_bytes" => json!(limits.request_stack_bytes),
        "io_stack_bytes" => json!(limits.io_stack_bytes),
        "blocking_threads" => json!(limits.blocking_threads),
        "blocking_queue" => json!(limits.blocking_queue),
        "max_body_bytes" => json!(limits.max_body),
        "max_json_body_bytes" => json!(limits.max_json_body),
        "max_complete_body_bytes" => json!(limits.max_complete_body),
        "max_header_bytes" => json!(limits.max_header),
        "max_streams" => json!(limits.max_streams),
        "stream_buffer_bytes" => json!(limits.stream_buffer),
        "max_connections" => json!(limits.max_connections),
        "max_connections_per_ip" => json!(limits.max_connections_per_ip),
        "max_subscriptions" => json!(limits.max_subscriptions),
        "max_subscribers_per_plugin" => json!(limits.max_subscribers_per_plugin),
        "pending_queue" => json!(limits.pending_queue),
        "progress_queue" => json!(limits.progress_queue),
        "max_package_bytes" => json!(limits.max_package_bytes),
        "http_idle_timeout_secs" => json!(limits.idle_timeout.as_secs()),
        "transfer_timeout_secs" => json!(limits.transfer_timeout.as_secs()),
        "control_timeout_secs" => json!(limits.control_timeout.as_secs()),
        "default_timeout" => json!(limits.default_timeout.as_millis() as u64),
        "max_timeout" => json!(limits.max_timeout.as_millis() as u64),
        "ws_max_message_bytes" => json!(limits.ws_max_message_bytes),
        "ws_inflight_bytes" => json!(limits.ws_inflight_bytes),
        "ws_inflight_requests" => json!(limits.ws_inflight_requests),
        "ws_buffer_shrink_bytes" => json!(limits.ws_buffer_shrink_bytes),
        "ws_raw_enabled" => json!(limits.ws_raw_enabled),
        "ws_raw_max_bytes" => json!(limits.ws_raw_max_bytes),
        "ws_raw_queue" => json!(limits.ws_raw_queue),
        "log_max_bytes" => json!(limits.log_max_bytes),
        "log_rotations" => json!(limits.log_rotations),
        "log_to_file" => json!(limits.log_to_file),
        "log" => json!(limits.log_enabled),
        // `as_str` is the padded log column ("INFO "); the screen shows the name.
        // `as_str` is the padded, upper-case log column ("INFO "); the screen
        // shows the name the document uses.
        "log_level" => json!(limits.log_level.as_str().trim().to_ascii_lowercase()),
        "rescan_interval_ms" => json!(settings.rescan_interval_ms()),
        "temp_retention_secs" => json!(settings.temp_retention().as_secs()),
        "temp_cleanup_interval_secs" => json!(settings.temp_cleanup_interval().as_secs()),
        "bind" => json!(bind),
        "mcp_streaming" => json!(settings.mcp_streaming.unwrap_or(false)),
        "allow_insecure_install" => json!(settings.allow_insecure_install),
        "auth_passwd" => json!(settings.auth_passwd.is_some()),
        "config_dir" => json!(paths.config_dir.to_string_lossy()),
        "data_dir" => json!(paths.data_dir.to_string_lossy()),
        "logs_dir" => json!(paths.logs_dir.to_string_lossy()),
        "install_dir" => json!(settings
            .install_dir
            .as_ref()
            .map(|path| path.to_string_lossy())),
        "probe_available_memory" => json!(settings.probe_available_memory),
        "probe_cgroup_remaining" => json!(settings.probe_cgroup_remaining),
        "probe_cpus" => json!(settings.probe_cpus),
        "probe_install_free_disk" => json!(settings.probe_install_free_disk),
        "probe_temp_free_disk" => json!(settings.probe_temp_free_disk),
        _ => Value::Null,
    }
}
