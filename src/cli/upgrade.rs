//! `svcd upgrade`: check, or apply a local/remote release.
//!
//! The download half lives in `plugin-store`; this module only parses the
//! flags, orchestrates through `selfinstall::upgrade`, and renders the answer.

/// Parse the flags and run the command. `usage` is the caller's top-level help;
/// an unknown flag prints it instead of inventing a second copy.
pub(crate) fn run(args: &[String], usage: impl Fn()) -> i32 {
    let mut check = false;
    let mut json = false;
    let mut file: Option<std::path::PathBuf> = None;
    let mut sum: Option<std::path::PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.as_str() {
            "--check" => check = true,
            "--json" => json = true,
            "--file" | "--sum" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("svcd: {} needs a path", arg);
                    return 64;
                };
                if arg == "--file" {
                    file = Some(std::path::PathBuf::from(value));
                } else {
                    sum = Some(std::path::PathBuf::from(value));
                }
            }
            _ if arg.starts_with("--file=") => {
                file = Some(std::path::PathBuf::from(&arg["--file=".len()..]));
            }
            _ if arg.starts_with("--sum=") => {
                sum = Some(std::path::PathBuf::from(&arg["--sum=".len()..]));
            }
            other => {
                eprintln!("svcd: unknown upgrade argument: {}", other);
                usage();
                return 64;
            }
        }
        index += 1;
    }
    let Some(config) = crate::platform::config::Config::bootstrap_or_report("svcd") else {
        return 1;
    };
    if let Some(path) = file {
        return match crate::selfinstall::upgrade::apply_file(&config, &path, sum.as_deref()) {
            Ok(applied) => report_applied(&applied, json),
            Err(error) => {
                eprintln!("svcd: upgrade: {}", error);
                1
            }
        };
    }
    if check {
        return match crate::selfinstall::upgrade::check(&config) {
            Ok(found) => report_check(&found, json),
            Err(error) => {
                eprintln!("svcd: upgrade check: {}", error);
                1
            }
        };
    }
    match crate::selfinstall::upgrade::apply_remote(&config) {
        Ok(applied) => report_applied(&applied, json),
        Err(error) => {
            eprintln!("svcd: upgrade: {}", error);
            1
        }
    }
}

fn report_check(found: &crate::selfinstall::upgrade::Check, json: bool) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&found.to_json()).unwrap_or_default()
        );
        return 0;
    }
    if !found.store {
        eprintln!(
            "svcd {} is installed, but plugin-store is not: install it from the plugin store, \
             or upgrade offline with `svcd upgrade --file <archive>`",
            found.current
        );
        return 1;
    }
    let latest = found.latest.clone().unwrap_or_default();
    println!("svcd {} -> {}", found.current, latest);
    if found.newer {
        println!("a newer release is available; run `svcd upgrade`");
    } else if latest == found.current {
        println!("already at the latest release");
    } else {
        println!("the latest release is older than the running build (downgrade is allowed)");
    }
    0
}

fn report_applied(applied: &crate::selfinstall::upgrade::Applied, json: bool) -> i32 {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&applied.to_json()).unwrap_or_default()
        );
        return 0;
    }
    println!(
        "upgraded to {} ({})",
        applied.version,
        applied.binary.display()
    );
    match (&applied.restarting, &applied.hint) {
        (true, _) => println!("restarting the daemon"),
        (false, Some(hint)) => println!("{}", hint),
        (false, None) => println!("restart the daemon for this to take effect"),
    }
    0
}
