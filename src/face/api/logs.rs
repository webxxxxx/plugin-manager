//! Log endpoints: the plugin tail (`/api/v1/logs/{id}`) and the daemon tail
//! (`/api/v1/logs`), plus the clear routes that go through the writer.
//!
//! Kept out of `api.rs` because that file is at its line budget; the methods
//! are still inherent on [`ApiFace`], so the route table in the parent module
//! names them like every other handler.

use super::*;

impl ApiFace {
    /// Parse the `lines` / `level` query and tail one log file. The plugin and
    /// daemon routes share it, so the two surfaces cannot drift.
    fn tail_log_rpc(&self, path: &std::path::Path, label: &str, query: &str) -> Response {
        let lines = match support::query_value(query, "lines") {
            None => 200,
            Some(value) => match value.parse::<usize>() {
                Ok(lines) if lines <= 2000 => lines,
                _ => {
                    return error_response(
                        400,
                        "bad_request",
                        "lines must be an integer between 0 and 2000",
                    )
                }
            },
        };
        let level = match support::query_value(query, "level") {
            None => None,
            Some(value) => match Level::parse(&value) {
                Some(level) => Some(level),
                None => {
                    return error_response(
                        400,
                        "bad_request",
                        "level must be one of error, warn, info, debug, trace",
                    )
                }
            },
        };
        match log::tail(path, lines) {
            Ok(text) => {
                let text = match level {
                    Some(threshold) => log::filter_text(&text, threshold),
                    None => text,
                };
                Response::new(
                    200,
                    "text/plain; charset=utf-8",
                    cap_log(text, self.config.limits.log_max_bytes).into_bytes(),
                )
            }
            Err(cause) if cause.kind() == io::ErrorKind::NotFound => {
                error_response(404, "not_found", &format!("no log for {}", label))
            }
            Err(cause) => error_response(500, "internal", &format!("cannot read log: {}", cause)),
        }
    }

    fn logs_rpc(&self, budget: &CallBudget, id: &str, query: &str) -> Response {
        if let Err(response) = self.require_plugin(budget, id) {
            return response;
        }
        let path = self.config.paths.logs_dir.join(format!("{}.log", id));
        self.tail_log_rpc(&path, id, query)
    }

    fn daemon_logs_rpc(&self, query: &str) -> Response {
        let path = self.config.paths.logs_dir.join("svcd.log");
        self.tail_log_rpc(&path, "svcd", query)
    }

    /// Clear one log (plugin when `id` is set, the daemon log otherwise). The
    /// supervisor reports how many files it removed; zero means there was
    /// nothing to clear, which the API states as 404.
    fn clear_logs_rpc(&self, budget: &CallBudget, id: Option<&str>) -> Response {
        let params = match id {
            Some(id) => json!({ "id": id }),
            None => json!({}),
        };
        match self.rpc(budget, "svc.logs.clear", params) {
            Ok(value) => {
                if value.get("files").and_then(Value::as_u64).unwrap_or(0) == 0 {
                    let label = id.unwrap_or("svcd");
                    return error_response(404, "not_found", &format!("no log for {}", label));
                }
                Response::json(200, &value)
            }
            Err(response) => response,
        }
    }

    pub(super) fn logs(&self, call: &ApiCall<'_>) -> Response {
        self.logs_rpc(call.budget(), &call.param("id"), call.query())
    }

    pub(super) fn logs_clear(&self, call: &ApiCall<'_>) -> Response {
        let id = call.param("id");
        if let Err(response) = self.require_plugin(call.budget(), &id) {
            return response;
        }
        self.clear_logs_rpc(call.budget(), Some(&id))
    }

    pub(super) fn daemon_logs(&self, call: &ApiCall<'_>) -> Response {
        self.daemon_logs_rpc(call.query())
    }

    pub(super) fn daemon_logs_clear(&self, call: &ApiCall<'_>) -> Response {
        self.clear_logs_rpc(call.budget(), None)
    }
}
