use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};

use crate::channel;
use crate::definition;
use crate::log;
use crate::paths;
use crate::rpc;
use crate::status::{self, Status};

pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_FOUND: i32 = 1;
/// The status contract only reserves 0/1/2/64; start/stop failures use 1 as the
/// generic service error, while `EXIT_NOT_FOUND` is kept for unknown ids.
pub const EXIT_SERVICE_ERROR: i32 = 1;
pub const EXIT_DAEMON_UNREACHABLE: i32 = 2;
pub const EXIT_USAGE: i32 = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonVerb {
    Start,
    Stop,
    Enable,
    Disable,
}

impl DaemonVerb {
    fn as_str(self) -> &'static str {
        match self {
            DaemonVerb::Start => "start",
            DaemonVerb::Stop => "stop",
            DaemonVerb::Enable => "enable",
            DaemonVerb::Disable => "disable",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LanAction {
    Status,
    Enable,
    Disable,
    Rotate,
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status {
        id: String,
        json: bool,
    },
    Daemon {
        verb: DaemonVerb,
        id: String,
    },
    Logs {
        id: String,
        follow: bool,
        lines: usize,
    },
    Lan {
        action: LanAction,
        bind: Option<String>,
    },
}

pub fn run(args: Vec<String>) -> i32 {
    let command = match parse(&args) {
        Ok(command) => command,
        Err(()) => {
            usage();
            return EXIT_USAGE;
        }
    };

    match command {
        Command::List => list(),
        Command::Status { id, json } => show_status(&id, json),
        Command::Daemon { verb, id } => daemon_command(verb, &id),
        Command::Logs { id, follow, lines } => show_logs(&id, follow, lines),
        Command::Lan { action, bind } => lan_command(action, bind.as_deref()),
    }
}

fn parse(args: &[String]) -> Result<Command, ()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(());
    };

    match command {
        "list" => {
            if args.len() == 1 {
                Ok(Command::List)
            } else {
                Err(())
            }
        }
        "status" => {
            let mut id = None;
            let mut json = false;
            for arg in &args[1..] {
                if arg == "--json" {
                    json = true;
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
            }
            id.map(|id| Command::Status { id, json }).ok_or(())
        }
        "start" | "stop" | "enable" | "disable" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let verb = match command {
                "start" => DaemonVerb::Start,
                "stop" => DaemonVerb::Stop,
                "enable" => DaemonVerb::Enable,
                "disable" => DaemonVerb::Disable,
                _ => unreachable!("matched daemon command"),
            };
            Ok(Command::Daemon {
                verb,
                id: args[1].clone(),
            })
        }
        "logs" => parse_logs(&args[1..]),
        "lan" => parse_lan(&args[1..]),
        _ => Err(()),
    }
}

fn parse_lan(args: &[String]) -> Result<Command, ()> {
    match args {
        [action] if action == "status" => Ok(Command::Lan {
            action: LanAction::Status,
            bind: None,
        }),
        [action] if action == "disable" => Ok(Command::Lan {
            action: LanAction::Disable,
            bind: None,
        }),
        [first, second] if first == "token" && second == "rotate" => Ok(Command::Lan {
            action: LanAction::Rotate,
            bind: None,
        }),
        [action] if action == "enable" => Ok(Command::Lan {
            action: LanAction::Enable,
            bind: None,
        }),
        [action, flag, value] if action == "enable" && flag == "--bind" => Ok(Command::Lan {
            action: LanAction::Enable,
            bind: Some(value.clone()),
        }),
        [action, value] if action == "enable" && value.starts_with("--bind=") => {
            let bind = value.trim_start_matches("--bind=").to_string();
            if bind.is_empty() {
                Err(())
            } else {
                Ok(Command::Lan {
                    action: LanAction::Enable,
                    bind: Some(bind),
                })
            }
        }
        _ => Err(()),
    }
}

fn parse_logs(args: &[String]) -> Result<Command, ()> {
    let mut id = None;
    let mut follow = false;
    let mut lines = 200usize;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if arg == "--follow" {
            follow = true;
        } else if arg == "--lines" {
            index += 1;
            let value = args.get(index).ok_or(())?;
            lines = parse_lines(value)?;
        } else if let Some(value) = arg.strip_prefix("--lines=") {
            lines = parse_lines(value)?;
        } else if arg.starts_with('-') || id.is_some() {
            return Err(());
        } else {
            id = Some(arg.clone());
        }
        index += 1;
    }
    let id = id.ok_or(())?;
    Ok(Command::Logs { id, follow, lines })
}

fn parse_lines(value: &str) -> Result<usize, ()> {
    let lines = value.parse::<usize>().map_err(|_| ())?;
    if lines > 2000 {
        return Err(());
    }
    Ok(lines)
}

fn list() -> i32 {
    if let Some(statuses) = live_list() {
        println!("{}", status::table(&statuses));
        return EXIT_OK;
    }

    let loaded = definition::load_services(&paths::services_dir());
    for issue in &loaded.issues {
        eprintln!("svc: warning: {}", issue);
    }
    let statuses: Vec<Status> = loaded.registry.values().map(Status::of).collect();
    println!("{}", status::table(&statuses));
    EXIT_OK
}

fn live_list() -> Option<Vec<Status>> {
    let response = channel::request(&paths::config_dir(), "svc.list", json!({})).ok()?;
    if let Some(error) = response.error {
        eprintln!("svc: {}", error.message);
        return None;
    }
    let services = response.result?.get("services")?.clone();
    match serde_json::from_value::<Vec<Status>>(services) {
        Ok(statuses) => Some(statuses),
        Err(error) => {
            eprintln!("svc: invalid list response: {}", error);
            None
        }
    }
}

fn show_status(id: &str, json: bool) -> i32 {
    if let Ok(response) = channel::request(
        &paths::config_dir(),
        "svc.status",
        json!({ "id": id, "probe": true }),
    ) {
        return match response {
            rpc::Response {
                error: Some(error), ..
            } => {
                eprintln!("svc: {}", error.message);
                EXIT_NOT_FOUND
            }
            rpc::Response {
                result: Some(result),
                ..
            } => print_status_value(result, json),
            _ => {
                eprintln!("svc: empty status response");
                EXIT_NOT_FOUND
            }
        };
    }

    let loaded = definition::load_services(&paths::services_dir());
    if let Some(service) = loaded.registry.get(id) {
        return print_status(Status::of(service), json);
    }

    let definition_file = format!("{}.json", id);
    if let Some(issue) = loaded
        .issues
        .iter()
        .find(|issue| issue.id == id || issue.file.as_deref() == Some(definition_file.as_str()))
    {
        eprintln!("svc: {}", issue);
    } else {
        eprintln!("svc: unknown service: {}", id);
    }
    EXIT_NOT_FOUND
}

fn daemon_command(verb: DaemonVerb, id: &str) -> i32 {
    let method = match verb {
        DaemonVerb::Start => "svc.start",
        DaemonVerb::Stop => "svc.stop",
        DaemonVerb::Enable => "svc.enable",
        DaemonVerb::Disable => "svc.disable",
    };
    match channel::request(&paths::config_dir(), method, json!({ "id": id })) {
        Ok(response) => match response.error {
            Some(error) => {
                eprintln!("svc: {}", error.message);
                EXIT_SERVICE_ERROR
            }
            None => match response.result {
                Some(result) => print_status_value(result, false),
                None => {
                    eprintln!("svc: empty {} response", verb.as_str());
                    EXIT_SERVICE_ERROR
                }
            },
        },
        Err(error) => {
            eprintln!("svc: svcd unreachable: {}", error);
            EXIT_DAEMON_UNREACHABLE
        }
    }
}

fn lan_command(action: LanAction, bind: Option<&str>) -> i32 {
    if matches!(
        action,
        LanAction::Enable | LanAction::Disable | LanAction::Rotate
    ) {
        if let Err(error) = crate::lan::require_root() {
            eprintln!("svc: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    }
    let (method, params) = match action {
        LanAction::Status => ("lan.status", json!({})),
        LanAction::Enable => ("lan.enable", json!({ "bind": bind.unwrap_or("0.0.0.0") })),
        LanAction::Disable => ("lan.disable", json!({})),
        LanAction::Rotate => ("lan.token.rotate", json!({})),
    };
    let response = match channel::request(&paths::config_dir(), method, params) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("svc: svcd unreachable: {}", error);
            return EXIT_DAEMON_UNREACHABLE;
        }
    };
    if let Some(error) = response.error {
        eprintln!("svc: {}", error.message);
        return EXIT_SERVICE_ERROR;
    }
    let result = response.result.unwrap_or(Value::Null);
    match action {
        LanAction::Status => {
            println!(
                "enabled: {}",
                result
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            );
            println!(
                "bind: {}",
                result.get("bind").and_then(Value::as_str).unwrap_or("-")
            );
            println!(
                "token: {}",
                if result
                    .get("tokenSet")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    "set"
                } else {
                    "unset"
                }
            );
        }
        LanAction::Enable | LanAction::Disable => {
            let enabled = result
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let bind = result.get("bind").and_then(Value::as_str).unwrap_or("-");
            println!("LAN {}", if enabled { "enabled" } else { "disabled" });
            println!("bind: {}", bind);
            if let Some(token) = result.get("token").and_then(Value::as_str) {
                println!("token: {}", token);
                eprintln!("warning: this token is equivalent to root; store it securely");
                eprintln!("warning: it is shown only once; rotate with `svc lan token rotate`");
            }
        }
        LanAction::Rotate => {
            if let Some(token) = result.get("token").and_then(Value::as_str) {
                println!("token: {}", token);
                eprintln!(
                    "warning: this token is equivalent to root; the old token is now invalid"
                );
            } else {
                eprintln!("svc: empty token response");
                return EXIT_SERVICE_ERROR;
            }
        }
    }
    EXIT_OK
}

fn show_logs(id: &str, follow: bool, lines: usize) -> i32 {
    let path = if id == "svcd" {
        paths::logs_dir().join("svcd.log")
    } else {
        paths::logs_dir().join(format!("{}.log", id))
    };
    let tail = match log::tail(&path, lines) {
        Ok(tail) => tail,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("svc: no log for {}", id);
            return EXIT_NOT_FOUND;
        }
        Err(error) => {
            eprintln!("svc: cannot read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    print!("{}", tail);
    if follow {
        let offset = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        follow_log(&path, offset);
    }
    EXIT_OK
}

fn follow_log(path: &Path, mut offset: u64) -> ! {
    use std::io::{Read, Seek, SeekFrom};

    loop {
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.len() < offset {
                offset = 0;
            }
        }
        if let Ok(mut file) = std::fs::File::open(path) {
            if file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut buffer = String::new();
                if file.read_to_string(&mut buffer).is_ok() && !buffer.is_empty() {
                    print!("{}", buffer);
                    let _ = std::io::stdout().flush();
                    offset = offset.saturating_add(buffer.len() as u64);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn print_status_value(value: Value, json: bool) -> i32 {
    match serde_json::from_value::<Status>(value) {
        Ok(status) => print_status(status, json),
        Err(error) => {
            eprintln!("svc: invalid status response: {}", error);
            EXIT_SERVICE_ERROR
        }
    }
}

fn print_status(status: Status, json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(rendered) => println!("{}", rendered),
            Err(error) => {
                eprintln!("svc: cannot encode status: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        println!("{}", status::text(&status));
    }
    EXIT_OK
}

// Only the verbs not implemented by #29 reach this helper.
fn usage() {
    eprintln!("usage: svc list");
    eprintln!("       svc status [--json] <id>");
    eprintln!("       svc <start|stop|enable|disable> <id>");
    eprintln!("       svc logs <id> [--follow] [--lines N]");
    eprintln!("       svc lan status|enable [--bind <ip>]|disable|token rotate");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_list() {
        assert_eq!(parse(&args(&["list"])).expect("list"), Command::List);
        assert!(parse(&args(&["list", "--json"])).is_err());
        assert!(parse(&args(&[])).is_err());
    }

    #[test]
    fn parses_status_with_json_in_either_position() {
        assert_eq!(
            parse(&args(&["status", "demo"])).expect("status"),
            Command::Status {
                id: "demo".into(),
                json: false
            }
        );
        assert_eq!(
            parse(&args(&["status", "--json", "demo"])).expect("status json"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
        assert_eq!(
            parse(&args(&["status", "demo", "--json"])).expect("status json"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
    }

    #[test]
    fn rejects_status_without_an_id_or_with_two_ids() {
        assert!(parse(&args(&["status"])).is_err());
        assert!(parse(&args(&["status", "--json"])).is_err());
        assert!(parse(&args(&["status", "one", "two"])).is_err());
        assert!(parse(&args(&["status", "--nope", "one"])).is_err());
    }

    #[test]
    fn parses_daemon_commands_but_rejects_missing_ids() {
        assert_eq!(
            parse(&args(&["start", "demo"])).expect("start"),
            Command::Daemon {
                verb: DaemonVerb::Start,
                id: "demo".into()
            }
        );
        assert!(parse(&args(&["start"])).is_err());
        assert!(parse(&args(&["stop", "demo", "extra"])).is_err());
        assert!(parse(&args(&["logs", "--follow"])).is_err());
    }

    #[test]
    fn parses_logs_with_flags_and_a_default_tail() {
        assert_eq!(
            parse(&args(&["logs", "demo"])).expect("logs"),
            Command::Logs {
                id: "demo".into(),
                follow: false,
                lines: 200
            }
        );
        assert_eq!(
            parse(&args(&["logs", "--follow", "demo", "--lines", "5"])).expect("logs"),
            Command::Logs {
                id: "demo".into(),
                follow: true,
                lines: 5
            }
        );
        assert_eq!(
            parse(&args(&["logs", "demo", "--lines=7"])).expect("logs"),
            Command::Logs {
                id: "demo".into(),
                follow: false,
                lines: 7
            }
        );
        assert!(parse(&args(&["logs"])).is_err());
        assert!(parse(&args(&["logs", "demo", "--lines", "nope"])).is_err());
        assert!(parse(&args(&["logs", "demo", "--lines", "2001"])).is_err());
    }

    #[test]
    fn parses_lan_commands() {
        assert_eq!(
            parse(&args(&["lan", "status"])).expect("lan status"),
            Command::Lan {
                action: LanAction::Status,
                bind: None
            }
        );
        assert_eq!(
            parse(&args(&["lan", "enable", "--bind", "0.0.0.0"])).expect("lan enable"),
            Command::Lan {
                action: LanAction::Enable,
                bind: Some("0.0.0.0".into())
            }
        );
        assert_eq!(
            parse(&args(&["lan", "token", "rotate"])).expect("lan rotate"),
            Command::Lan {
                action: LanAction::Rotate,
                bind: None
            }
        );
        assert!(parse(&args(&["lan", "enable", "--bind"])).is_err());
        assert!(parse(&args(&["lan", "bogus"])).is_err());
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse(&args(&["describe", "demo"])).is_err());
        assert!(parse(&args(&["--json", "status", "demo"])).is_err());
    }
}
