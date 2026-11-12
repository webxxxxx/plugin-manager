//! The `plugin` entry: plugin-only views over the same daemon channel.
//!
//! `svc` stays the generic service CLI; `plugin` refuses core ids for
//! status/start/stop and adds install/uninstall.

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::channel;
use crate::paths;
use crate::registry::Kind;
use crate::status::{self, Status};

const EXIT_OK: i32 = 0;
const EXIT_SERVICE_ERROR: i32 = 1;
const EXIT_DAEMON_UNREACHABLE: i32 = 2;
const EXIT_USAGE: i32 = 64;

#[derive(Debug, PartialEq, Eq)]
enum Command {
    List,
    Status { id: String, json: bool },
    Start { id: String },
    Stop { id: String },
    Install { path: PathBuf, strategy: String },
    Uninstall { id: String, purge: bool },
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
        Command::Status { id, json } => status_command(&id, json),
        Command::Start { id } => lifecycle("start", &id),
        Command::Stop { id } => lifecycle("stop", &id),
        Command::Install { path, strategy } => install(&path, &strategy),
        Command::Uninstall { id, purge } => uninstall(&id, purge),
    }
}

fn parse(args: &[String]) -> Result<Command, ()> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err(());
    };
    match command {
        "list" if args.len() == 1 => Ok(Command::List),
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
        "start" | "stop" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let id = args[1].clone();
            if command == "start" {
                Ok(Command::Start { id })
            } else {
                Ok(Command::Stop { id })
            }
        }
        "install" => {
            let mut path = None;
            let mut strategy = "keep-old".to_string();
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--strategy" {
                    index += 1;
                    strategy = args.get(index).ok_or(())?.clone();
                } else if let Some(value) = arg.strip_prefix("--strategy=") {
                    strategy = value.to_string();
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            if strategy != "keep-old" && strategy != "replace" {
                return Err(());
            }
            path.map(|path| Command::Install { path, strategy })
                .ok_or(())
        }
        "uninstall" => {
            let mut id = None;
            let mut purge = false;
            for arg in &args[1..] {
                if arg == "--purge" {
                    purge = true;
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
            }
            id.map(|id| Command::Uninstall { id, purge }).ok_or(())
        }
        _ => Err(()),
    }
}

fn list() -> i32 {
    let response = match channel::request(&paths::config_dir(), "svc.list", json!({})) {
        Ok(response) => response,
        Err(error) => return unreachable(&error),
    };
    if let Some(error) = response.error {
        eprintln!("plugin: {}", error.message);
        return EXIT_SERVICE_ERROR;
    }
    let result = match response.result {
        Some(result) => result,
        None => {
            eprintln!("plugin: empty list response");
            return EXIT_SERVICE_ERROR;
        }
    };
    let services = result.get("services").cloned().unwrap_or(Value::Null);
    let services: Vec<Status> = match serde_json::from_value(services) {
        Ok(services) => services,
        Err(error) => {
            eprintln!("plugin: invalid list response: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    };
    let plugins: Vec<Status> = services
        .into_iter()
        .filter(|status| status.summary.kind == Kind::Plugin)
        .collect();
    println!("{}", status::table(&plugins));
    EXIT_OK
}

fn status_command(id: &str, json: bool) -> i32 {
    let status = match fetch_status(id) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    print_status(status, json)
}

fn lifecycle(verb: &str, id: &str) -> i32 {
    let status = match fetch_status(id) {
        Ok(status) => status,
        Err(code) => return code,
    };
    if status.summary.kind != Kind::Plugin {
        eprintln!("plugin: {} is not a plugin", id);
        return EXIT_SERVICE_ERROR;
    }
    let method = format!("svc.{}", verb);
    match request(&method, json!({ "id": id })) {
        Ok(status) => print_status(status, false),
        Err(code) => code,
    }
}

fn install(path: &PathBuf, strategy: &str) -> i32 {
    let path = match std::fs::canonicalize(path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!("plugin: cannot read {}: {}", path.display(), error);
            return EXIT_SERVICE_ERROR;
        }
    };
    match request(
        "plugin.install",
        json!({ "path": path.to_string_lossy(), "strategy": strategy }),
    ) {
        Ok(status) => print_status(status, false),
        Err(code) => code,
    }
}

fn uninstall(id: &str, purge: bool) -> i32 {
    match channel::request(
        &paths::config_dir(),
        "plugin.uninstall",
        json!({ "id": id, "purge": purge }),
    ) {
        Ok(response) => match response.error {
            Some(error) => {
                eprintln!("plugin: {}", error.message);
                EXIT_SERVICE_ERROR
            }
            None => {
                println!("plugin {} uninstalled", id);
                EXIT_OK
            }
        },
        Err(error) => unreachable(&error),
    }
}

fn fetch_status(id: &str) -> Result<Status, i32> {
    request("svc.status", json!({ "id": id }))
}

fn request(method: &str, params: Value) -> Result<Status, i32> {
    let response = match channel::request(&paths::config_dir(), method, params) {
        Ok(response) => response,
        Err(error) => return Err(unreachable(&error)),
    };
    if let Some(error) = response.error {
        eprintln!("plugin: {}", error.message);
        return Err(EXIT_SERVICE_ERROR);
    }
    let result = match response.result {
        Some(result) => result,
        None => {
            eprintln!("plugin: empty response");
            return Err(EXIT_SERVICE_ERROR);
        }
    };
    serde_json::from_value(result).map_err(|error| {
        eprintln!("plugin: invalid status response: {}", error);
        EXIT_SERVICE_ERROR
    })
}

fn print_status(status: Status, json: bool) -> i32 {
    if json {
        match serde_json::to_string_pretty(&status) {
            Ok(text) => println!("{}", text),
            Err(error) => {
                eprintln!("plugin: cannot encode status: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
    } else {
        println!("{}", status::text(&status));
    }
    EXIT_OK
}

fn unreachable(error: &std::io::Error) -> i32 {
    eprintln!("plugin: svcd unreachable: {}", error);
    EXIT_DAEMON_UNREACHABLE
}

fn usage() {
    eprintln!("usage: plugin list");
    eprintln!("       plugin status [--json] <id>");
    eprintln!("       plugin start|stop <id>");
    eprintln!("       plugin install <file.spk> [--strategy keep-old|replace]");
    eprintln!("       plugin uninstall <id> [--purge]");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_plugin_commands() {
        assert_eq!(parse(&args(&["list"])).expect("list"), Command::List);
        assert_eq!(
            parse(&args(&["status", "demo", "--json"])).expect("status"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
        assert_eq!(
            parse(&args(&["install", "demo.spk", "--strategy", "replace"])).expect("install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into()
            }
        );
        assert_eq!(
            parse(&args(&["uninstall", "demo", "--purge"])).expect("uninstall"),
            Command::Uninstall {
                id: "demo".into(),
                purge: true
            }
        );
        assert!(parse(&args(&["install", "demo.spk", "--strategy", "nope"])).is_err());
        assert!(parse(&args(&["status"])).is_err());
        assert!(parse(&args(&["list", "extra"])).is_err());
    }
}
