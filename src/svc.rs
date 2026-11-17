use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::wire::control::channel;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::platform::log;
use crate::platform::config::Config;
use crate::wire::message::rpc;
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
    Restart,
    Enable,
    Disable,
}

impl DaemonVerb {
    fn as_str(self) -> &'static str {
        match self {
            DaemonVerb::Start => "start",
            DaemonVerb::Stop => "stop",
            DaemonVerb::Restart => "restart",
            DaemonVerb::Enable => "enable",
            DaemonVerb::Disable => "disable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthAction {
    Status,
    Passwd(String),
}

#[derive(Debug, PartialEq, Eq)]
enum TrustAction {
    List,
    Add(PathBuf),
    Remove(String),
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
    Reload,
    Compat {
        json: bool,
    },
    CompatInit {
        device: String,
        dry_run: bool,
    },
    Logs {
        id: String,
        follow: bool,
        lines: usize,
    },
    Auth {
        action: AuthAction,
    },
    Trust {
        action: TrustAction,
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

    let Some(config) = Config::bootstrap_or_report("svc") else {
        return EXIT_SERVICE_ERROR;
    };
    match command {
        Command::List => list(&config),
        Command::Status { id, json } => show_status(&config, &id, json),
        Command::Daemon { verb, id } => daemon_command(&config, verb, &id),
        Command::Reload => reload(&config),
        Command::Compat { json } => compat_show(&config, json),
        Command::CompatInit { device, dry_run } => compat_init(&config, &device, dry_run),
        Command::Logs { id, follow, lines } => show_logs(&config, &id, follow, lines),
        Command::Auth { action } => auth_command(&config, action),
        Command::Trust { action } => trust_command(&config, action),
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
        "start" | "stop" | "restart" | "enable" | "disable" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let verb = match command {
                "start" => DaemonVerb::Start,
                "stop" => DaemonVerb::Stop,
                "restart" => DaemonVerb::Restart,
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
        "compat" => parse_compat(&args[1..]),
        "reload" if args.len() == 1 => Ok(Command::Reload),
        "auth" | "lan" => parse_auth(&args[1..]),
        "trust" => parse_trust(&args[1..]),
        _ => Err(()),
    }
}

fn parse_trust(args: &[String]) -> Result<Command, ()> {
    let action = match args {
        [action] if action == "list" => TrustAction::List,
        [action, path] if action == "add" && !path.starts_with('-') => {
            TrustAction::Add(PathBuf::from(path))
        }
        [action, id] if action == "remove" && !id.starts_with('-') => {
            TrustAction::Remove(id.clone())
        }
        _ => return Err(()),
    };
    Ok(Command::Trust { action })
}

fn parse_auth(args: &[String]) -> Result<Command, ()> {
    match args {
        [action] if action == "status" => Ok(Command::Auth {
            action: AuthAction::Status,
        }),
        [action, password]
            if action == "passwd" && !password.is_empty() && !password.starts_with('-') =>
        {
            Ok(Command::Auth {
                action: AuthAction::Passwd(password.clone()),
            })
        }
        _ => Err(()),
    }
}

fn parse_compat(args: &[String]) -> Result<Command, ()> {
    let Some(first) = args.first().map(String::as_str) else {
        return Ok(Command::Compat { json: false });
    };
    if first == "--json" {
        return if args.len() == 1 {
            Ok(Command::Compat { json: true })
        } else {
            Err(())
        };
    }
    if first != "init" {
        return Err(());
    }
    let mut device = None;
    let mut dry_run = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--device" => {
                index += 1;
                let value = args.get(index).ok_or(())?;
                device = Some(value.clone());
            }
            "--dry-run" => dry_run = true,
            other => {
                if let Some(value) = other.strip_prefix("--device=") {
                    if value.is_empty() {
                        return Err(());
                    }
                    device = Some(value.to_string());
                } else {
                    return Err(());
                }
            }
        }
        index += 1;
    }
    device
        .map(|device| Command::CompatInit { device, dry_run })
        .ok_or(())
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

/// `svc list` reads live state only. It used to fall back to reading
/// `services.d` when the daemon could not be reached, which made "daemon is
/// down" and "there are no services" look identical; the fallback is gone and an
/// unreachable daemon is now reported and exits non-zero.
fn list(config: &Config) -> i32 {
    match live_list(config) {
        Ok(statuses) => {
            println!("{}", status::table(&statuses));
            EXIT_OK
        }
        Err(ListError::Unreachable(error)) => {
            eprintln!("svc: svcd unreachable: {}", error);
            explain_unreachable(config);
            EXIT_DAEMON_UNREACHABLE
        }
        Err(ListError::Daemon(message)) => {
            eprintln!("svc: {}", message);
            EXIT_SERVICE_ERROR
        }
    }
}

/// A user-mode CLI cannot open the machine-wide control pipe: the pipe name
/// hashes the config directory, and the SCM service uses the machine-wide one.
/// The pipe ACL is the real gate (Administrators only become effective after
/// elevation; the `svcd` group is the read-only tier), so point at that instead
/// of leaving "pipe is not available" unexplained.
fn explain_unreachable(config: &Config) {
    if std::env::var_os("SVCD_CONFIG_DIR").is_some() {
        return;
    }
    let machine = crate::platform::paths::platform_config_dir();
    if machine == config.paths.config_dir || !machine_wide_install_present(&machine) {
        return;
    }
    if cfg!(windows) {
        eprintln!(
            "svc: hint: a machine-wide svcd is installed at {}; run this command from an elevated terminal, or add your account to the \"svcd\" group for read-only commands",
            machine.display()
        );
    } else {
        eprintln!(
            "svc: hint: a machine-wide svcd is installed at {}; run this command as root",
            machine.display()
        );
    }
}

/// A non-elevated administrator cannot read the protected machine-wide config
/// directory after hardening, so also accept its visible parent (`%ProgramData%\svcd`),
/// which the installer and service both create.
fn machine_wide_install_present(machine: &Path) -> bool {
    if machine.join("svcd.conf").is_file() {
        return true;
    }
    cfg!(windows) && machine.parent().is_some_and(|parent| parent.is_dir())
}

/// A live listing failed. An unreachable daemon is an environment problem (exit
/// 2); a daemon that answered with an error is a service problem (exit 1).
enum ListError {
    Unreachable(String),
    Daemon(String),
}

fn live_list(config: &Config) -> Result<Vec<Status>, ListError> {
    let response = channel::request(&config.paths.config_dir, "svc.list", json!({}))
        .map_err(|error| ListError::Unreachable(error.to_string()))?;
    if let Some(error) = response.error {
        return Err(ListError::Daemon(error.message));
    }
    let services = response
        .result
        .and_then(|result| result.get("services").cloned())
        .ok_or_else(|| ListError::Daemon("empty list response".into()))?;
    serde_json::from_value::<Vec<Status>>(services)
        .map_err(|error| ListError::Daemon(format!("invalid list response: {}", error)))
}

/// `svc status` reads live state only, for the same reason as `svc list`: the
/// offline fallback made an unreachable daemon look like a stopped service.
fn show_status(config: &Config, id: &str, json: bool) -> i32 {
    let response = match channel::request(
        &config.paths.config_dir,
        "svc.status",
        json!({ "id": id, "probe": true }),
    ) {
        Ok(response) => response,
        Err(error) => {
            eprintln!("svc: svcd unreachable: {}", error);
            explain_unreachable(config);
            return EXIT_DAEMON_UNREACHABLE;
        }
    };
    match response {
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
    }
}

fn daemon_command(config: &Config, verb: DaemonVerb, id: &str) -> i32 {
    let method = match verb {
        DaemonVerb::Start => "svc.start",
        DaemonVerb::Stop => "svc.stop",
        DaemonVerb::Restart => "svc.restart",
        DaemonVerb::Enable => "svc.enable",
        DaemonVerb::Disable => "svc.disable",
    };
    match channel::request(&config.paths.config_dir, method, json!({ "id": id })) {
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
            explain_unreachable(config);
            EXIT_DAEMON_UNREACHABLE
        }
    }
}

fn reload(config: &Config) -> i32 {
    let response = match channel::request(&config.paths.config_dir, "svc.reload", json!({})) {
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
    let ids = |key: &str| {
        result
            .get(key)
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    for (key, label) in [
        ("added", "added"),
        ("removed", "removed"),
        ("updated", "updated"),
    ] {
        let list = ids(key);
        if !list.is_empty() {
            println!("{}: {}", label, list);
        }
    }
    if ids("added").is_empty() && ids("removed").is_empty() && ids("updated").is_empty() {
        println!("reloaded: no changes");
    }
    EXIT_OK
}

fn compat_show(config: &Config, json: bool) -> i32 {
    let facts = crate::platform::compat::detect(&config.paths, MAX_SCHEMA, MAX_PROTOCOL);
    if json {
        match serde_json::to_string_pretty(&facts) {
            Ok(text) => println!("{}", text),
            Err(error) => {
                eprintln!("svc: cannot encode compat facts: {}", error);
                return EXIT_SERVICE_ERROR;
            }
        }
        return EXIT_OK;
    }
    println!("platform:      {}", facts.platform);
    match (&facts.device, &facts.device_source) {
        (Some(device), Some(source)) => println!("device:        {} ({})", device, source),
        _ => println!("device:        - (not provisioned)"),
    }
    println!(
        "kernel:        {}",
        facts.kernel_release.as_deref().unwrap_or("-")
    );
    println!("svcd:          {}", facts.svcd_version);
    println!("schema_max:    {}", facts.schema_max);
    println!("protocol_max:  {}", facts.protocol_max);
    println!("marker:        {}", facts.marker_path);
    for warning in &facts.warnings {
        eprintln!("svc: warning: {}", warning);
    }
    EXIT_OK
}

fn compat_init(config: &Config, device: &str, dry_run: bool) -> i32 {
    if let Err(error) = crate::plugin::root_required() {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    if let Err(error) = crate::platform::compat::validate_device_id(device) {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    let path = crate::platform::compat::marker_path(&config.paths);
    if dry_run {
        println!("would write {}:", path.display());
        println!("device = {}", device);
        return EXIT_OK;
    }
    if let Err(error) = crate::platform::compat::write_device(&path, device) {
        eprintln!("svc: {}", error);
        return EXIT_SERVICE_ERROR;
    }
    println!("wrote {} (device = {})", path.display(), device);
    compat_show(config, false)
}

fn auth_command(config: &Config, action: AuthAction) -> i32 {
    if matches!(&action, AuthAction::Passwd(_)) {
        if let Err(error) = crate::lan::require_root() {
            eprintln!("svc: {}", error);
            return EXIT_SERVICE_ERROR;
        }
    }
    let (method, params) = match &action {
        AuthAction::Status => ("auth.status", json!({})),
        AuthAction::Passwd(password) => ("auth.passwd", json!({ "password": password })),
    };
    let response = match channel::request(&config.paths.config_dir, method, params) {
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
    if matches!(&action, AuthAction::Status) {
        println!(
            "bind: {}",
            result.get("bind").and_then(Value::as_str).unwrap_or("-")
        );
        println!(
            "requested: {}",
            result
                .get("requestedBind")
                .and_then(Value::as_str)
                .unwrap_or("-")
        );
        println!(
            "auth required: {}",
            result
                .get("authRequired")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        println!(
            "password: {}",
            if result
                .get("passwordSet")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "set"
            } else {
                "unset"
            }
        );
    } else {
        println!("password: set");
        println!(
            "bind: {}",
            result.get("bind").and_then(Value::as_str).unwrap_or("-")
        );
    }
    EXIT_OK
}

fn trust_command(config: &Config, action: TrustAction) -> i32 {
    match action {
        TrustAction::List => match crate::trust::keys(&config.paths) {
            Ok(keys) if keys.is_empty() => {
                println!("no trusted keys in {}", config.paths.trust_dir.display());
                EXIT_OK
            }
            Ok(keys) => {
                for key in keys {
                    println!("{}", key);
                }
                EXIT_OK
            }
            Err(error) => {
                eprintln!("svc: {}", error);
                EXIT_SERVICE_ERROR
            }
        },
        TrustAction::Add(path) => {
            if let Err(error) = require_root_for_trust() {
                eprintln!("svc: {}", error);
                return EXIT_SERVICE_ERROR;
            }
            let pem = match std::fs::read(&path) {
                Ok(pem) => pem,
                Err(error) => {
                    eprintln!("svc: cannot read {}: {}", path.display(), error);
                    return EXIT_SERVICE_ERROR;
                }
            };
            let key_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            match crate::trust::add(&config.paths, key_id, &pem) {
                Ok(target) => {
                    println!("trusted {} ({})", key_id, target.display());
                    EXIT_OK
                }
                Err(error) => {
                    eprintln!("svc: {}", error);
                    EXIT_SERVICE_ERROR
                }
            }
        }
        TrustAction::Remove(key_id) => {
            if let Err(error) = require_root_for_trust() {
                eprintln!("svc: {}", error);
                return EXIT_SERVICE_ERROR;
            }
            match crate::trust::remove(&config.paths, &key_id) {
                Ok(true) => {
                    println!("removed {}", key_id);
                    EXIT_OK
                }
                Ok(false) => {
                    eprintln!("svc: unknown trusted key: {}", key_id);
                    EXIT_NOT_FOUND
                }
                Err(error) => {
                    eprintln!("svc: {}", error);
                    EXIT_SERVICE_ERROR
                }
            }
        }
    }
}

fn require_root_for_trust() -> Result<(), String> {
    if crate::platform::is_privileged() || crate::platform::paths::user_mode() {
        Ok(())
    } else {
        Err("root / Administrator is required to change trust roots".into())
    }
}

fn show_logs(config: &Config, id: &str, follow: bool, lines: usize) -> i32 {
    let path = if id == "svcd" {
        config.paths.logs_dir.join("svcd.log")
    } else {
        config.paths.logs_dir.join(format!("{}.log", id))
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
    eprintln!("       svc <start|stop|restart|enable|disable> <id>");
    eprintln!("       svc reload");
    eprintln!("       svc compat [--json]");
    eprintln!("       svc compat init --device <id> [--dry-run]");
    eprintln!("       svc logs <id> [--follow] [--lines N]");
    eprintln!("       svc lan status|passwd <password>");
    eprintln!("       svc auth ...  (deprecated alias of svc lan)");
    eprintln!("       svc trust list|add <file.pub>|remove <keyId>");
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
        assert_eq!(
            parse(&args(&["restart", "demo"])).expect("restart"),
            Command::Daemon {
                verb: DaemonVerb::Restart,
                id: "demo".into()
            }
        );
        assert!(parse(&args(&["start"])).is_err());
        assert!(parse(&args(&["restart"])).is_err());
        assert!(parse(&args(&["stop", "demo", "extra"])).is_err());
        assert!(parse(&args(&["logs", "--follow"])).is_err());
    }

    #[test]
    fn parses_reload_and_rejects_extra_arguments() {
        assert_eq!(parse(&args(&["reload"])).expect("reload"), Command::Reload);
        assert!(parse(&args(&["reload", "--force"])).is_err());
    }

    #[test]
    fn parses_compat_and_rejects_unknown_forms() {
        assert_eq!(
            parse(&args(&["compat"])).expect("compat"),
            Command::Compat { json: false }
        );
        assert_eq!(
            parse(&args(&["compat", "--json"])).expect("compat json"),
            Command::Compat { json: true }
        );
        assert_eq!(
            parse(&args(&["compat", "init", "--device", "sr6110"])).expect("init"),
            Command::CompatInit {
                device: "sr6110".into(),
                dry_run: false
            }
        );
        assert_eq!(
            parse(&args(&["compat", "init", "--device=sr6110", "--dry-run"])).expect("init"),
            Command::CompatInit {
                device: "sr6110".into(),
                dry_run: true
            }
        );
        assert!(parse(&args(&["compat", "init"])).is_err());
        assert!(parse(&args(&["compat", "--json", "init"])).is_err());
        assert!(parse(&args(&["compat", "init", "--device"])).is_err());
        assert!(parse(&args(&["compat", "bogus"])).is_err());
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
    fn parses_lan_commands_and_the_deprecated_auth_alias() {
        assert_eq!(
            parse(&args(&["lan", "status"])).expect("lan status"),
            Command::Auth {
                action: AuthAction::Status
            }
        );
        assert_eq!(
            parse(&args(&["lan", "passwd", "secret"])).expect("lan passwd"),
            Command::Auth {
                action: AuthAction::Passwd("secret".into())
            }
        );
        // Existing scripts keep working while the docs move to `svc lan`.
        assert_eq!(
            parse(&args(&["auth", "status"])).expect("auth alias"),
            Command::Auth {
                action: AuthAction::Status
            }
        );
        assert!(parse(&args(&["lan", "enable", "--bind"])).is_err());
        assert!(parse(&args(&["lan", "bogus"])).is_err());
        assert!(parse(&args(&["auth", "passwd"])).is_err());
        assert!(parse(&args(&["lan", "passwd"])).is_err());
    }

    #[test]
    fn parses_trust_commands() {
        assert_eq!(
            parse(&args(&["trust", "list"])).expect("trust list"),
            Command::Trust {
                action: TrustAction::List
            }
        );
        assert_eq!(
            parse(&args(&["trust", "add", "sample-2026.pub"])).expect("trust add"),
            Command::Trust {
                action: TrustAction::Add(PathBuf::from("sample-2026.pub"))
            }
        );
        assert_eq!(
            parse(&args(&["trust", "remove", "sample-2026"])).expect("trust remove"),
            Command::Trust {
                action: TrustAction::Remove("sample-2026".into())
            }
        );
        assert!(parse(&args(&["trust"])).is_err());
        assert!(parse(&args(&["trust", "add"])).is_err());
        assert!(parse(&args(&["trust", "add", "--key-id"])).is_err());
        assert!(parse(&args(&["trust", "bogus"])).is_err());
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(parse(&args(&["describe", "demo"])).is_err());
        assert!(parse(&args(&["--json", "status", "demo"])).is_err());
    }
}
