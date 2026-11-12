use crate::definition;
use crate::paths;
use crate::status;

pub const EXIT_OK: i32 = 0;
pub const EXIT_NOT_FOUND: i32 = 1;
pub const EXIT_DAEMON_UNREACHABLE: i32 = 2;
pub const EXIT_USAGE: i32 = 64;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status { id: String, json: bool },
    Daemon { verb: &'static str, id: String },
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
        Command::Daemon { verb, id } => daemon_unreachable(verb, &id),
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
                "start" => "start",
                "stop" => "stop",
                "enable" => "enable",
                "disable" => "disable",
                "logs" => "logs",
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
    let loaded = definition::load_services(&paths::services_dir());
    for issue in &loaded.issues {
        eprintln!("svc: warning: {}", issue);
    }
    println!("{}", status::table(&loaded.registry));
    EXIT_OK
}

fn show_status(id: &str, json: bool) -> i32 {
    let loaded = definition::load_services(&paths::services_dir());

    if let Some(service) = loaded.registry.get(id) {
        let payload = status::Status::of(service);
        if json {
            let rendered = serde_json::to_string_pretty(&payload)
                .expect("status payload contains only serializable values");
            println!("{}", rendered);
        } else {
            println!("{}", status::text(&payload));
        }
        return EXIT_OK;
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

// The management channel arrives with the supervisor (#29). Until then every
// command that needs live state reports the daemon as unreachable.
fn daemon_unreachable(verb: &str, id: &str) -> i32 {
    eprintln!(
        "svc: svcd unreachable: cannot {} {} yet (no management channel)",
        verb, id
    );
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
                verb: "start",
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
