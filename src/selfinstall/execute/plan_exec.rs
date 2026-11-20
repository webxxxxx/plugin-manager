use super::*;

pub(super) fn usage_self(uninstall: bool) {
    if uninstall {
        eprintln!("usage: svcd --uninstall [--prefix <dir>] [--no-service] [--purge] [--dry-run]");
    } else {
        eprintln!(
            "usage: svcd --install [--prefix <dir>] [--conf|--config <file>] [--manual] [--no-service] [--target <init>] [--dry-run]"
        );
        eprintln!(
            "       --prefix is recorded as install_dir; --manual skips init registration and writes svcd_daemon.sh/.bat"
        );
    }
}

pub(super) fn dispatch(action: Action, args: &[String]) -> i32 {
    let request = match parse_request(action, args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage(action);
            return EXIT_USAGE;
        }
    };

    let target = request.target.unwrap_or_else(|| detect(&Probe::current()));
    let mut layout = Layout::current();
    if let Some(program) = request.program {
        layout.program = program;
    }
    layout.conf = request.conf;
    let plan = match action {
        Action::Install => install_plan(target, &layout),
        Action::Uninstall => uninstall_plan(target, &layout, request.purge),
    };

    if request.dry_run {
        print!("{}", plan);
        return EXIT_OK;
    }

    if !crate::platform::is_privileged() {
        eprintln!("svcd: {} requires root / Administrator", action.as_str());
        return EXIT_ERROR;
    }

    match execute(&plan) {
        Ok(()) => {
            println!("svcd: {} {}", action.past(), target);
            EXIT_OK
        }
        Err(error) => {
            eprintln!("svcd: {} failed: {}", action.as_str(), error);
            EXIT_ERROR
        }
    }
}

fn usage(action: Action) {
    match action {
        Action::Install => eprintln!(
            "usage: svcd install-service [--target systemd|launchd|scm|busybox|procd] [--program <path>] [--conf <file>] [--dry-run]"
        ),
        Action::Uninstall => eprintln!(
            "usage: svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--program <path>] [--purge] [--dry-run]"
        ),
    }
}

pub(super) fn execute(plan: &InstallPlan) -> Result<(), String> {
    for op in &plan.ops {
        execute_op(op).map_err(|error| format!("{}: {}", describe(op), error))?;
    }
    Ok(())
}

fn describe(op: &Op) -> String {
    match op {
        Op::WriteFile { path, .. } => format!("write {}", path.display()),
        Op::EnsureLine { path, .. } => format!("ensure line in {}", path.display()),
        Op::RemoveLine { path, .. } => format!("remove line from {}", path.display()),
        Op::RemoveFile { path } => format!("remove file {}", path.display()),
        Op::RemoveDir { path } => format!("remove directory {}", path.display()),
        Op::Run { argv, .. } => format!("run {}", argv.join(" ")),
        Op::AssertFileContains { path, .. } => format!("assert {} contains", path.display()),
    }
}

fn execute_op(op: &Op) -> Result<(), String> {
    match op {
        Op::WriteFile {
            path,
            content,
            mode,
        } => write_file(path, content, *mode),
        Op::EnsureLine { path, line } => ensure_line(path, line),
        Op::RemoveLine { path, line } => remove_line(path, line),
        Op::RemoveFile { path } => remove_file(path),
        Op::RemoveDir { path } => remove_dir(path),
        Op::Run { argv, allow_codes } => run_command(argv, allow_codes.as_deref()),
        Op::AssertFileContains { path, needle } => assert_file_contains(path, needle),
    }
}
