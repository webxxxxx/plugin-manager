use serde_json::json;

use crate::wire::control::channel;

pub fn shutdown() -> i32 {
    shutdown_inner(true)
}

pub fn shutdown_quiet() -> i32 {
    shutdown_inner(false)
}

fn shutdown_inner(verbose: bool) -> i32 {
    // Stopping only needs the control endpoint; a broken `svcd.conf` must not
    // make `svcd stop` unusable.
    let paths = crate::platform::config::ambient_paths(None);
    match channel::request(paths.control_dir(), "svcd.shutdown", json!({})) {
        Ok(response) => match response.error {
            Some(error) => {
                if verbose {
                    eprintln!("svcd: {}", error.message);
                }
                1
            }
            None => 0,
        },
        Err(error) if shutdown_reply_lost(&error) => {
            // The request reached a daemon that then exited. On Windows the
            // per-connection control thread can lose the race with process exit
            // and close the pipe before its reply frame is written; the stop
            // hooks have already run. Reporting "unreachable" here would turn a
            // successful stop into a failure.
            0
        }
        Err(error) => {
            if verbose {
                eprintln!("svcd: svcd unreachable: {}", error);
            }
            2
        }
    }
}

pub(super) fn shutdown_reply_lost(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}
