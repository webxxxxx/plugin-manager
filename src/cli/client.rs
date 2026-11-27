//! Shared control-channel client for the `svc` and `plugin` entry points.
//!
//! Both binaries speak the same NDJSON channel and render the same `Status`
//! value; only the error prefix and the verb table differ.

use serde_json::{json, Value};

use crate::cli::status;
use crate::kernel::projection::Status;
use crate::platform::config::Config;
use crate::wire::control::channel;

pub(crate) const EXIT_OK: i32 = 0;
pub(crate) const EXIT_NOT_FOUND: i32 = 1;
/// The status contract only reserves 0/1/2/64; start/stop failures use 1 as the
/// generic service error, while `EXIT_NOT_FOUND` is kept for unknown ids.
pub(crate) const EXIT_SERVICE_ERROR: i32 = 1;
pub(crate) const EXIT_DAEMON_UNREACHABLE: i32 = 2;
pub(crate) const EXIT_USAGE: i32 = 64;

/// Ask the daemon for one service status. `probe` asks the daemon to run the
/// capability probe as part of the reply.
pub(crate) fn status(config: &Config, id: &str, probe: bool, label: &str) -> Result<Status, i32> {
    let mut params = json!({ "id": id });
    if probe {
        params["probe"] = json!(true);
    }
    request(config, "svc.status", params, label)
}

/// One management request; the label is the CLI prefix (`svc` / `plugin`).
pub(crate) fn request(
    config: &Config,
    method: &str,
    params: Value,
    label: &str,
) -> Result<Status, i32> {
    let response = match channel::request(config.paths.control_dir(), method, params) {
        Ok(response) => response,
        Err(error) => return Err(unreachable(&error, label)),
    };
    if let Some(error) = response.error {
        report_error(label, &error);
        return Err(EXIT_SERVICE_ERROR);
    }
    let result = match response.result {
        Some(result) => result,
        None => {
            eprintln!("{}: empty response", label);
            return Err(EXIT_SERVICE_ERROR);
        }
    };
    serde_json::from_value(result).map_err(|error| {
        eprintln!("{}: invalid status response: {}", label, error);
        EXIT_SERVICE_ERROR
    })
}

pub(crate) fn print_status(status: Status, json: bool, label: &str) -> i32 {
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(rendered) => println!("{}", rendered),
            Err(error) => {
                eprintln!("{}: cannot encode status: {}", label, error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        println!("{}", status::text(&status));
    }
    EXIT_OK
}

pub(crate) fn unreachable(error: &std::io::Error, label: &str) -> i32 {
    eprintln!("{}: svcd unreachable: {}", label, error);
    EXIT_DAEMON_UNREACHABLE
}

/// Print one daemon error the way every CLI verb prints it.
///
/// The daemon stamps the request's call number into `error.data.call` (#343);
/// carrying it on the same line is what makes `grep call=7 svcd.log` possible
/// after a failed install. A response without that key renders exactly as it
/// did before, so no existing output changes.
pub(crate) fn report_error(label: &str, error: &crate::wire::message::rpc::Error) {
    eprintln!("{}", error_line(label, error));
}

fn error_line(label: &str, error: &crate::wire::message::rpc::Error) -> String {
    let call = error
        .data
        .as_ref()
        .and_then(|data| data.get("call"))
        .and_then(Value::as_u64);
    match call {
        Some(call) => format!("{}: {} (call={})", label, error.message, call),
        None => format!("{}: {}", label, error.message),
    }
}

#[cfg(test)]
mod tests {
    use super::error_line;
    use crate::wire::message::rpc;

    #[test]
    fn a_daemon_error_carries_the_call_number_on_the_same_line() {
        let error = rpc::Error {
            code: -32006,
            message: "post-install hook failed".into(),
            data: Some(serde_json::json!({ "call": 7, "kind": "internal" })),
        };
        assert_eq!(
            error_line("plugin", &error),
            "plugin: post-install hook failed (call=7)"
        );
    }

    #[test]
    fn an_error_without_a_call_number_renders_as_before() {
        let error = rpc::Error {
            code: -32006,
            message: "post-install hook failed".into(),
            data: Some(serde_json::json!({ "kind": "internal" })),
        };
        assert_eq!(
            error_line("plugin", &error),
            "plugin: post-install hook failed"
        );
        let bare = rpc::Error {
            code: -32006,
            message: "post-install hook failed".into(),
            data: None,
        };
        assert_eq!(error_line("plugin", &bare), "plugin: post-install hook failed");
    }
}
