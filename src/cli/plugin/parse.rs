use super::*;

#[allow(clippy::large_enum_variant)] // Parse-only command AST; the large Bundle/Import variants are cold.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    Status {
        id: String,
        json: bool,
    },
    Start {
        id: String,
    },
    Stop {
        id: String,
    },
    Restart {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Install {
        path: PathBuf,
        strategy: String,
        allow_hooks: bool,
    },
    Verify {
        path: PathBuf,
        json: bool,
        /// Check the signature against this PEM even when the key is not in
        /// `trust.d`. CI has no trust root; this is how it checks the release
        /// signature without pretending the key is trusted.
        pubkey: Option<PathBuf>,
    },
    Uninstall {
        id: String,
        purge: bool,
    },
    EditConfig {
        id: String,
        print: bool,
        file: Option<PathBuf>,
        reset: bool,
    },
    Export {
        id: String,
        config_only: bool,
        out: Option<PathBuf>,
    },
    Import {
        path: PathBuf,
        config_id: Option<String>,
        no_config: bool,
        force: bool,
    },
    Bundle(crate::manage::ops::bundle::Options),
}

pub(super) fn parse(args: &[String]) -> Result<Command, ()> {
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
        "start" | "stop" | "restart" | "enable" | "disable" => {
            if args.len() != 2 || args[1].starts_with('-') {
                return Err(());
            }
            let id = args[1].clone();
            match command {
                "start" => Ok(Command::Start { id }),
                "stop" => Ok(Command::Stop { id }),
                "restart" => Ok(Command::Restart { id }),
                "enable" => Ok(Command::Enable { id }),
                _ => Ok(Command::Disable { id }),
            }
        }
        "install" => {
            let mut path = None;
            let mut strategy = "replace".to_string();
            let mut allow_hooks = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--strategy" {
                    index += 1;
                    strategy = args.get(index).ok_or(())?.clone();
                } else if let Some(value) = arg.strip_prefix("--strategy=") {
                    strategy = value.to_string();
                } else if arg == "--allow-hooks" {
                    allow_hooks = true;
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
            path.map(|path| Command::Install {
                path,
                strategy,
                allow_hooks,
            })
            .ok_or(())
        }
        "verify" => {
            let mut path = None;
            let mut json = false;
            let mut pubkey = None;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--json" {
                    json = true;
                } else if arg == "--pubkey" {
                    index += 1;
                    pubkey = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if let Some(value) = arg.strip_prefix("--pubkey=") {
                    pubkey = Some(PathBuf::from(value));
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            path.map(|path| Command::Verify { path, json, pubkey })
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
        "editconfig" => {
            let mut id = None;
            let mut print = false;
            let mut file = None;
            let mut reset = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--print" {
                    print = true;
                } else if arg == "--reset" {
                    reset = true;
                } else if arg == "--file" {
                    index += 1;
                    file = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
                index += 1;
            }
            if print && (file.is_some() || reset) || file.is_some() && reset {
                return Err(());
            }
            id.map(|id| Command::EditConfig {
                id,
                print,
                file,
                reset,
            })
            .ok_or(())
        }
        "export" => {
            let mut id = None;
            let mut config_only = false;
            let mut out = None;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--config-only" {
                    config_only = true;
                } else if arg == "--out" {
                    index += 1;
                    out = Some(PathBuf::from(args.get(index).ok_or(())?));
                } else if let Some(value) = arg.strip_prefix("--out=") {
                    out = Some(PathBuf::from(value));
                } else if arg.starts_with('-') || id.is_some() {
                    return Err(());
                } else {
                    id = Some(arg.clone());
                }
                index += 1;
            }
            id.map(|id| Command::Export {
                id,
                config_only,
                out,
            })
            .ok_or(())
        }
        "import" => {
            let mut path = None;
            let mut config_id = None;
            let mut no_config = false;
            let mut force = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                if arg == "--no-config" {
                    no_config = true;
                } else if arg == "--force" {
                    force = true;
                } else if arg == "--config" {
                    index += 1;
                    config_id = Some(args.get(index).ok_or(())?.clone());
                } else if let Some(value) = arg.strip_prefix("--config=") {
                    config_id = Some(value.to_string());
                } else if arg.starts_with('-') || path.is_some() {
                    return Err(());
                } else {
                    path = Some(PathBuf::from(arg));
                }
                index += 1;
            }
            path.map(|path| Command::Import {
                path,
                config_id,
                no_config,
                force,
            })
            .ok_or(())
        }
        "bundle" => {
            let mut dir = None;
            let mut out = None;
            let mut key = None;
            let mut key_id = None;
            let mut id = None;
            let mut name = None;
            let mut version = None;
            let mut exec = None;
            let mut desc = None;
            let mut from_core = false;
            let mut interpreter = None;
            let mut interpreter_version = None;
            let mut interpreter_args = Vec::new();
            let mut compat = Vec::new();
            let mut platform = None;
            let mut unsigned = false;
            let mut index = 1;
            while index < args.len() {
                let arg = &args[index];
                match arg.as_str() {
                    "-o" | "--out" => {
                        index += 1;
                        out = Some(PathBuf::from(args.get(index).ok_or(())?));
                    }
                    "--key" => {
                        index += 1;
                        key = Some(PathBuf::from(args.get(index).ok_or(())?));
                    }
                    "--unsigned" => unsigned = true,
                    "--key-id" => {
                        index += 1;
                        key_id = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--id" => {
                        index += 1;
                        id = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--name" => {
                        index += 1;
                        name = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--version" => {
                        index += 1;
                        version = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--exec" => {
                        index += 1;
                        exec = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--desc" => {
                        index += 1;
                        desc = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--from-core" => from_core = true,
                    "--interpreter" => {
                        index += 1;
                        interpreter = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--interpreter-version" => {
                        index += 1;
                        interpreter_version = Some(args.get(index).ok_or(())?.clone());
                    }
                    "--interpreter-arg" => {
                        index += 1;
                        interpreter_args.push(args.get(index).ok_or(())?.clone());
                    }
                    "--compat" => {
                        index += 1;
                        compat.extend(
                            args.get(index)
                                .ok_or(())?
                                .split(',')
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        );
                    }
                    "--platform" => {
                        index += 1;
                        platform = Some(args.get(index).ok_or(())?.clone());
                    }
                    _ if arg.starts_with("--out=") => {
                        out = Some(PathBuf::from(
                            arg.strip_prefix("--out=").expect("matched prefix"),
                        ));
                    }
                    _ if arg.starts_with("--key-id=") => {
                        key_id = Some(
                            arg.strip_prefix("--key-id=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--key=") => {
                        key = Some(PathBuf::from(
                            arg.strip_prefix("--key=").expect("matched prefix"),
                        ));
                    }
                    _ if arg.starts_with("--id=") => {
                        id = Some(
                            arg.strip_prefix("--id=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--name=") => {
                        name = Some(
                            arg.strip_prefix("--name=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--version=") => {
                        version = Some(
                            arg.strip_prefix("--version=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--exec=") => {
                        exec = Some(
                            arg.strip_prefix("--exec=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--desc=") => {
                        desc = Some(
                            arg.strip_prefix("--desc=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter=") => {
                        interpreter = Some(
                            arg.strip_prefix("--interpreter=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter-version=") => {
                        interpreter_version = Some(
                            arg.strip_prefix("--interpreter-version=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--interpreter-arg=") => {
                        interpreter_args.push(
                            arg.strip_prefix("--interpreter-arg=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with("--compat=") => {
                        compat.extend(
                            arg.strip_prefix("--compat=")
                                .expect("matched prefix")
                                .split(',')
                                .filter(|value| !value.is_empty())
                                .map(str::to_string),
                        );
                    }
                    _ if arg.starts_with("--platform=") => {
                        platform = Some(
                            arg.strip_prefix("--platform=")
                                .expect("matched prefix")
                                .to_string(),
                        );
                    }
                    _ if arg.starts_with('-') => return Err(()),
                    _ if dir.is_none() => dir = Some(PathBuf::from(arg)),
                    _ => return Err(()),
                }
                index += 1;
            }
            dir.map(|dir| {
                Command::Bundle(crate::manage::ops::bundle::Options {
                    dir,
                    out,
                    key,
                    key_id,
                    id,
                    name,
                    version,
                    exec,
                    desc,
                    from_core,
                    interpreter,
                    interpreter_version,
                    interpreter_args,
                    compat,
                    platform,
                    unsigned,
                })
            })
            .ok_or(())
        }
        _ => Err(()),
    }
}
