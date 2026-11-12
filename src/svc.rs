use serde_json::{json, Value};

use crate::channel;
use crate::definition;
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
    Logs,
}

impl DaemonVerb {
    fn as_str(self) -> &'static str {
        match self {
            DaemonVerb::Start => "start",
            DaemonVerb::Stop => "stop",
            DaemonVerb::Enable => "enable",
            DaemonVerb::Disable => "disable",
            DaemonVerb::Logs => "logs",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status { id: String, json: bool },
    Daemon { verb: DaemonVerb, id: String },
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
        "start" | "stop" | "enable" | "disable" | "logs" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let verb = match command {
                "start" => DaemonVerb::Start,
                "stop" => DaemonVerb::Stop,
                "enable" => DaemonVerb::Enable,
                "disable" => DaemonVerb::Disable,
                "logs" => DaemonVerb::Logs,
                _ => unreachable!("matched daemon command"),
            };
            Ok(Command::Daemon {
                verb,
                id: args[1].clone(),
            })
        }
        _ => Err(()),
    }
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
    if let Ok(response) = channel::request(&paths::config_dir(), "svc.status", json!({ "id": id }))
    {
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
    match verb {
        DaemonVerb::Start | DaemonVerb::Stop | DaemonVerb::Enable | DaemonVerb::Disable => {
            let method = match verb {
                DaemonVerb::Start => "svc.start",
                DaemonVerb::Stop => "svc.stop",
                DaemonVerb::Enable => "svc.enable",
                DaemonVerb::Disable => "svc.disable",
                DaemonVerb::Logs => unreachable!("matched lifecycle verb"),
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
        DaemonVerb::Logs => {
            if channel::request(&paths::config_dir(), "svcd.ping", json!({})).is_ok() {
                eprintln!("svc: {} is not implemented yet", verb.as_str());
                EXIT_SERVICE_ERROR
            } else {
                daemon_unreachable(verb.as_str(), id)
            }
        }
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
fn daemon_unreachable(verb: &str, id: &str) -> i32 {
    eprintln!("svc: svcd unreachable: cannot {} {}", verb, id);
    EXIT_DAEMON_UNREACHABLE
}

fn usage() {
    eprintln!("usage: svc list");
    eprintln!("       svc status [--json] <id>");
    eprintln!("       svc <start|stop|enable|disable|logs> <id>");
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
    fn rejects_unknown_commands() {
        assert!(parse(&args(&["describe", "demo"])).is_err());
        assert!(parse(&args(&["--json", "status", "demo"])).is_err());
    }
}
