//! Installation of `svcd` itself with the host init system.
//!
//! Each target is a data-only [`InstallPlan`]: a list of file and command
//! operations that the small executor below applies in order. Plans are pure
//! functions of an [`Layout`], so tests can assert their exact contents without
//! touching the host. `--dry-run` prints a plan and never executes it.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::paths;

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_USAGE: i32 = 64;

const SYSTEMD_UNIT: &str = "/etc/systemd/system/svcd.service";
const LAUNCHD_LABEL: &str = "com.webxxxxx.svcd";
const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/com.webxxxxx.svcd.plist";
const INITTAB: &str = "/etc/inittab";
const PROCD_SCRIPT: &str = "/etc/init.d/svcd";
const SCM_NAME: &str = "svcd";
const SCM_DISPLAY_NAME: &str = "svcd core supervisor";

/// A host init system that can own `svcd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Systemd,
    Launchd,
    Scm,
    Busybox,
    Procd,
}

impl Target {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "systemd" => Some(Target::Systemd),
            "launchd" => Some(Target::Launchd),
            "scm" => Some(Target::Scm),
            "busybox" => Some(Target::Busybox),
            "procd" => Some(Target::Procd),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Target::Systemd => "systemd",
            Target::Launchd => "launchd",
            Target::Scm => "scm",
            Target::Busybox => "busybox",
            Target::Procd => "procd",
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Install,
    Uninstall,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Install => "install-service",
            Action::Uninstall => "uninstall-service",
        }
    }

    fn past(self) -> &'static str {
        match self {
            Action::Install => "installed",
            Action::Uninstall => "uninstalled",
        }
    }
}

/// Operating-system family, injected so detection stays a pure function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Mac,
    Windows,
    Other,
}

/// Facts detection needs, injected instead of probed inside the decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    pub os: Os,
    pub systemd_runtime: bool,
    pub rc_common: bool,
    pub initd_writable: bool,
}

impl Probe {
    pub fn current() -> Self {
        Probe {
            os: current_os(),
            systemd_runtime: Path::new("/run/systemd/system").is_dir(),
            rc_common: Path::new("/etc/rc.common").exists(),
            initd_writable: path_writable(Path::new("/etc/init.d")),
        }
    }
}

fn current_os() -> Os {
    if cfg!(target_os = "windows") {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else if cfg!(target_os = "linux") {
        Os::Linux
    } else {
        Os::Other
    }
}

/// `/run/systemd/system` wins on Linux, then procd, then BusyBox inittab.
pub fn detect(probe: &Probe) -> Target {
    match probe.os {
        Os::Windows => Target::Scm,
        Os::Mac => Target::Launchd,
        Os::Linux => {
            if probe.systemd_runtime {
                Target::Systemd
            } else if probe.rc_common && probe.initd_writable {
                Target::Procd
            } else {
                Target::Busybox
            }
        }
        Os::Other => Target::Busybox,
    }
}

#[cfg(unix)]
fn path_writable(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(not(unix))]
fn path_writable(path: &Path) -> bool {
    path.is_dir()
}

/// Everything a plan needs to spell out paths and command lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub program: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
}

impl Layout {
    pub fn current() -> Self {
        Layout {
            program: default_program(),
            data_dir: paths::data_dir(),
            config_dir: paths::config_dir(),
        }
    }
}

fn default_program() -> PathBuf {
    if cfg!(windows) {
        return std::env::current_exe().unwrap_or_else(|_| PathBuf::from("svcd.exe"));
    }
    // Packaging installs the multicall binary in /usr/sbin; a development
    // build falls back to the executable that is running right now.
    let installed = PathBuf::from("/usr/sbin/svcd");
    if installed.is_file() {
        installed
    } else {
        std::env::current_exe().unwrap_or(installed)
    }
}

/// One reversible step of an installation plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    WriteFile {
        path: PathBuf,
        content: String,
        mode: u32,
    },
    EnsureLine {
        path: PathBuf,
        line: String,
    },
    RemoveLine {
        path: PathBuf,
        line: String,
    },
    RemoveFile {
        path: PathBuf,
    },
    RemoveDir {
        path: PathBuf,
    },
    /// `allow_codes`: exit codes that still count as success. `None` means any
    /// exit code is accepted (best effort).
    Run {
        argv: Vec<String>,
        allow_codes: Option<Vec<i32>>,
    },
    AssertFileContains {
        path: PathBuf,
        needle: String,
    },
}

impl fmt::Display for Op {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Op::WriteFile {
                path,
                content,
                mode,
            } => {
                write!(f, "write {} (mode {:04o})", path.display(), mode)?;
                for line in content.lines() {
                    write!(f, "\n    | {}", line)?;
                }
                Ok(())
            }
            Op::EnsureLine { path, line } => {
                write!(f, "ensure line in {}: {}", path.display(), line)
            }
            Op::RemoveLine { path, line } => {
                write!(f, "remove line from {}: {}", path.display(), line)
            }
            Op::RemoveFile { path } => write!(f, "remove file {}", path.display()),
            Op::RemoveDir { path } => write!(f, "remove directory {}", path.display()),
            Op::Run { argv, allow_codes } => {
                let note = match allow_codes {
                    None => " (best effort)".to_string(),
                    Some(codes) if codes.len() == 1 && codes[0] == 0 => String::new(),
                    Some(codes) => format!(" (allow exit {:?})", codes),
                };
                write!(f, "run {}{}", argv.join(" "), note)
            }
            Op::AssertFileContains { path, needle } => {
                write!(f, "assert {} contains {:?}", path.display(), needle)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub target: Target,
    pub action: Action,
    pub ops: Vec<Op>,
}

impl fmt::Display for InstallPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "svcd: {} plan for {} (dry run)",
            self.action.as_str(),
            self.target
        )?;
        for op in &self.ops {
            writeln!(f, "  {}", op)?;
        }
        Ok(())
    }
}

pub fn install_plan(target: Target, layout: &Layout) -> InstallPlan {
    InstallPlan {
        target,
        action: Action::Install,
        ops: match target {
            Target::Systemd => systemd_install(layout),
            Target::Launchd => launchd_install(layout),
            Target::Scm => scm_install(layout),
            Target::Busybox => busybox_install(layout),
            Target::Procd => procd_install(layout),
        },
    }
}

pub fn uninstall_plan(target: Target, layout: &Layout, purge: bool) -> InstallPlan {
    let mut ops = match target {
        Target::Systemd => systemd_uninstall(),
        Target::Launchd => launchd_uninstall(),
        Target::Scm => scm_uninstall(),
        Target::Busybox => busybox_uninstall(layout),
        Target::Procd => procd_uninstall(),
    };
    if purge {
        // The data and config trees may not exist yet; RemoveDir is a no-op
        // then. Removing data first also covers platforms where config nests
        // inside data (Windows, macOS).
        ops.push(Op::RemoveDir {
            path: layout.data_dir.clone(),
        });
        ops.push(Op::RemoveDir {
            path: layout.config_dir.clone(),
        });
    }
    InstallPlan {
        target,
        action: Action::Uninstall,
        ops,
    }
}

fn systemd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(SYSTEMD_UNIT);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: systemd_unit(&layout.program),
            mode: 0o644,
        },
        Op::AssertFileContains {
            path,
            needle: "Type=notify".into(),
        },
        run(&["systemctl", "daemon-reload"]),
        run(&["systemctl", "enable", "--now", "svcd"]),
    ]
}

fn systemd_uninstall() -> Vec<Op> {
    vec![
        run_allowing(&["systemctl", "disable", "--now", "svcd"], &[0, 1, 3, 4, 5]),
        Op::RemoveFile {
            path: PathBuf::from(SYSTEMD_UNIT),
        },
        run(&["systemctl", "daemon-reload"]),
        run_allowing(&["systemctl", "reset-failed", "svcd"], &[0, 1, 3, 4, 5]),
    ]
}

fn launchd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(LAUNCHD_PLIST);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: launchd_plist(&layout.program),
            mode: 0o644,
        },
        Op::AssertFileContains {
            path,
            needle: LAUNCHD_LABEL.into(),
        },
        run_allowing(
            &["launchctl", "bootstrap", "system", LAUNCHD_PLIST],
            &[0, 5, 37],
        ),
        run(&["launchctl", "enable", &launchd_domain()]),
        run(&["launchctl", "kickstart", "-k", &launchd_domain()]),
    ]
}

fn launchd_uninstall() -> Vec<Op> {
    vec![
        run_allowing(
            &["launchctl", "bootout", &launchd_domain()],
            &[0, 3, 5, 37, 112, 113],
        ),
        Op::RemoveFile {
            path: PathBuf::from(LAUNCHD_PLIST),
        },
    ]
}

fn scm_install(layout: &Layout) -> Vec<Op> {
    let bin_path = format!("\"{}\" run --foreground", layout.program.display());
    vec![
        Op::Run {
            argv: vec![
                "sc".into(),
                "create".into(),
                SCM_NAME.into(),
                "binPath=".into(),
                bin_path.clone(),
                "start=".into(),
                "auto".into(),
                "DisplayName=".into(),
                SCM_DISPLAY_NAME.into(),
            ],
            // 1073: ERROR_SERVICE_EXISTS — a reinstall converges with `sc config`.
            allow_codes: Some(vec![0, 1073]),
        },
        Op::Run {
            argv: vec![
                "sc".into(),
                "description".into(),
                SCM_NAME.into(),
                SCM_DISPLAY_NAME.into(),
            ],
            allow_codes: Some(vec![0]),
        },
        Op::Run {
            argv: vec![
                "sc".into(),
                "failure".into(),
                SCM_NAME.into(),
                "reset=".into(),
                "86400".into(),
                "actions=".into(),
                "restart/5000/restart/10000/restart/30000".into(),
            ],
            allow_codes: Some(vec![0]),
        },
        Op::Run {
            argv: vec![
                "sc".into(),
                "config".into(),
                SCM_NAME.into(),
                "binPath=".into(),
                bin_path,
                "start=".into(),
                "auto".into(),
            ],
            allow_codes: Some(vec![0]),
        },
        // 1056: ERROR_SERVICE_ALREADY_RUNNING.
        run_allowing(&["sc", "start", SCM_NAME], &[0, 1056]),
    ]
}

fn scm_uninstall() -> Vec<Op> {
    vec![
        // 1051: not started, 1060: no such service, 1062: not active.
        run_allowing(&["sc", "stop", SCM_NAME], &[0, 1051, 1060, 1062]),
        // 1060: no such service, 1072: marked for deletion.
        run_allowing(&["sc", "delete", SCM_NAME], &[0, 1060, 1072]),
    ]
}

fn busybox_install(layout: &Layout) -> Vec<Op> {
    let line = format!("::respawn:{} run --foreground", layout.program.display());
    vec![
        Op::EnsureLine {
            path: PathBuf::from(INITTAB),
            line: line.clone(),
        },
        Op::AssertFileContains {
            path: PathBuf::from(INITTAB),
            needle: line,
        },
        run_best_effort(&["kill", "-HUP", "1"]),
    ]
}

fn busybox_uninstall(layout: &Layout) -> Vec<Op> {
    let line = format!("::respawn:{} run --foreground", layout.program.display());
    vec![
        Op::RemoveLine {
            path: PathBuf::from(INITTAB),
            line,
        },
        run_best_effort(&["kill", "-HUP", "1"]),
    ]
}

fn procd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(PROCD_SCRIPT);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: procd_script(&layout.program),
            mode: 0o755,
        },
        Op::AssertFileContains {
            path,
            needle: "USE_PROCD=1".into(),
        },
        run(&["chmod", "0755", PROCD_SCRIPT]),
        run(&[PROCD_SCRIPT, "enable"]),
        run(&[PROCD_SCRIPT, "start"]),
    ]
}

fn procd_uninstall() -> Vec<Op> {
    vec![
        run_allowing(&[PROCD_SCRIPT, "stop"], &[0, 1]),
        run_allowing(&[PROCD_SCRIPT, "disable"], &[0, 1]),
        Op::RemoveFile {
            path: PathBuf::from(PROCD_SCRIPT),
        },
    ]
}

fn systemd_unit(program: &Path) -> String {
    let program = program.display();
    format!(
        "[Unit]\n\
         Description=svcd core supervisor\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         StartLimitIntervalSec=10\n\
         StartLimitBurst=5\n\
         \n\
         [Service]\n\
         Type=notify\n\
         NotifyAccess=main\n\
         ExecStart={program} run --foreground\n\
         ExecStop=-{program} shutdown\n\
         Restart=on-failure\n\
         RestartSec=1\n\
         TimeoutStopSec=40\n\
         KillMode=control-group\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

fn launchd_domain() -> String {
    format!("system/{}", LAUNCHD_LABEL)
}

fn launchd_plist(program: &Path) -> String {
    let label = LAUNCHD_LABEL;
    let program = xml_escape(&program.display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\
         \t<string>{label}</string>\n\
         \t<key>ProgramArguments</key>\n\
         \t<array>\n\
         \t\t<string>{program}</string>\n\
         \t\t<string>run</string>\n\
         \t\t<string>--foreground</string>\n\
         \t</array>\n\
         \t<key>RunAtLoad</key>\n\
         \t<true/>\n\
         \t<key>KeepAlive</key>\n\
         \t<true/>\n\
         \t<key>ThrottleInterval</key>\n\
         \t<integer>10</integer>\n\
         \t<key>ProcessType</key>\n\
         \t<string>Background</string>\n\
         \t<key>ExitTimeOut</key>\n\
         \t<integer>30</integer>\n\
         </dict>\n\
         </plist>\n"
    )
}

fn procd_script(program: &Path) -> String {
    let program = program.display();
    format!(
        "#!/bin/sh /etc/rc.common\n\
         # Managed by `svcd install-service`; reinstall overwrites this file.\n\
         USE_PROCD=1\n\
         START=95\n\
         STOP=10\n\
         \n\
         start_service() {{\n\
         \tprocd_open_instance\n\
         \tprocd_set_param command {program} run --foreground\n\
         \tprocd_set_param respawn 3600 5 5\n\
         \tprocd_set_param term_timeout 30\n\
         \tprocd_close_instance\n\
         }}\n"
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn run(argv: &[&str]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: Some(vec![0]),
    }
}

fn run_allowing(argv: &[&str], codes: &[i32]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: Some(codes.to_vec()),
    }
}

fn run_best_effort(argv: &[&str]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: None,
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Request {
    target: Option<Target>,
    dry_run: bool,
    purge: bool,
}

fn set_target(request: &mut Request, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("--target requires a value".into());
    }
    let target = Target::parse(value).ok_or_else(|| {
        format!(
            "unknown target: {} (expected systemd, launchd, scm, busybox or procd)",
            value
        )
    })?;
    if request.target.replace(target).is_some() {
        return Err("--target specified more than once".into());
    }
    Ok(())
}

fn parse_request(action: Action, args: &[String]) -> Result<Request, String> {
    let mut request = Request::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" => {
                if request.dry_run {
                    return Err("--dry-run specified more than once".into());
                }
                request.dry_run = true;
            }
            "--purge" => {
                if action != Action::Uninstall {
                    return Err("--purge only applies to uninstall-service".into());
                }
                if request.purge {
                    return Err("--purge specified more than once".into());
                }
                request.purge = true;
            }
            "--target" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--target requires a value".to_string())?;
                set_target(&mut request, value)?;
            }
            other => match other.strip_prefix("--target=") {
                Some(value) => set_target(&mut request, value)?,
                None => return Err(format!("unexpected argument: {}", other)),
            },
        }
    }
    Ok(request)
}

pub fn install_service(args: &[String]) -> i32 {
    dispatch(Action::Install, args)
}

pub fn uninstall_service(args: &[String]) -> i32 {
    dispatch(Action::Uninstall, args)
}

fn dispatch(action: Action, args: &[String]) -> i32 {
    let request = match parse_request(action, args) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("svcd: {}", message);
            usage(action);
            return EXIT_USAGE;
        }
    };

    let target = request.target.unwrap_or_else(|| detect(&Probe::current()));
    let layout = Layout::current();
    let plan = match action {
        Action::Install => install_plan(target, &layout),
        Action::Uninstall => uninstall_plan(target, &layout, request.purge),
    };

    if request.dry_run {
        print!("{}", plan);
        return EXIT_OK;
    }

    if !is_privileged() {
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
            "usage: svcd install-service [--target systemd|launchd|scm|busybox|procd] [--dry-run]"
        ),
        Action::Uninstall => eprintln!(
            "usage: svcd uninstall-service [--target systemd|launchd|scm|busybox|procd] [--purge] [--dry-run]"
        ),
    }
}

fn execute(plan: &InstallPlan) -> Result<(), String> {
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

fn write_file(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {}", parent.display(), error))?;
        }
    }
    let temporary = temporary_path(path)?;
    let result =
        write_temporary(&temporary, content, mode).and_then(|()| replace_file(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn temporary_path(path: &Path) -> Result<PathBuf, String> {
    let name = path
        .file_name()
        .ok_or_else(|| format!("invalid path: {}", path.display()))?;
    Ok(path.with_file_name(format!(
        ".{}.tmp.{}",
        name.to_string_lossy(),
        std::process::id()
    )))
}

fn write_temporary(path: &Path, content: &str, mode: u32) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|error| format!("cannot open {}: {}", path.display(), error))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("cannot write {}: {}", path.display(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|error| format!("cannot set mode on {}: {}", path.display(), error))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {}", path.display(), error))?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    // Windows cannot rename over an existing file; the replace is not atomic
    // there, but the destination is recreated immediately.
    if path.exists() {
        fs::remove_file(path)
            .map_err(|error| format!("cannot remove {}: {}", path.display(), error))?;
    }
    fs::rename(temporary, path)
        .map_err(|error| format!("cannot replace {}: {}", path.display(), error))
}

#[cfg(not(windows))]
fn replace_file(temporary: &Path, path: &Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("cannot replace {}: {}", path.display(), error))
}

fn read_text(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("cannot read {}: {}", path.display(), error)),
    }
}

fn existing_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o777)
            .unwrap_or(0o644)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

fn ensure_line(path: &Path, line: &str) -> Result<(), String> {
    let existing = read_text(path)?;
    if let Some(text) = &existing {
        if text.lines().any(|candidate| candidate == line) {
            return Ok(());
        }
    }
    let mut content = existing.unwrap_or_default();
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(line);
    content.push('\n');
    write_file(path, &content, existing_mode(path))
}

fn remove_line(path: &Path, line: &str) -> Result<(), String> {
    let Some(text) = read_text(path)? else {
        return Ok(());
    };
    if !text.lines().any(|candidate| candidate == line) {
        return Ok(());
    }
    let kept: Vec<&str> = text
        .lines()
        .filter(|candidate| *candidate != line)
        .collect();
    let mut content = kept.join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    write_file(path, &content, existing_mode(path))
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {}", path.display(), error)),
    }
}

fn remove_dir(path: &Path) -> Result<(), String> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {}", path.display(), error)),
    }
}

fn assert_file_contains(path: &Path, needle: &str) -> Result<(), String> {
    let content = read_text(path)?
        .ok_or_else(|| format!("{} does not exist after the write", path.display()))?;
    if content.contains(needle) {
        Ok(())
    } else {
        Err(format!("{} does not contain {:?}", path.display(), needle))
    }
}

fn run_command(argv: &[String], allow_codes: Option<&[i32]>) -> Result<(), String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "empty command".to_string())?;
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("cannot run {}: {}", argv.join(" "), error))?;
    if output.status.success() {
        return Ok(());
    }
    if allow_codes.is_none() {
        return Ok(());
    }
    if let (Some(allowed), Some(code)) = (allow_codes, output.status.code()) {
        if allowed.contains(&code) {
            return Ok(());
        }
    }
    let status = match output.status.code() {
        Some(code) => format!("exit {}", code),
        None => "a signal".to_string(),
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        Err(format!("`{}` failed with {}", argv.join(" "), status))
    } else {
        Err(format!(
            "`{}` failed with {}: {}",
            argv.join(" "),
            status,
            detail
        ))
    }
}

#[cfg(unix)]
pub(crate) fn is_privileged() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
pub(crate) fn is_privileged() -> bool {
    use std::mem::size_of;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut _,
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn is_privileged() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn layout() -> Layout {
        Layout {
            program: PathBuf::from("/usr/sbin/svcd"),
            data_dir: PathBuf::from("/var/lib/svcd"),
            config_dir: PathBuf::from("/etc/svcd"),
        }
    }

    fn written<'a>(plan: &'a InstallPlan, path: &Path) -> Option<&'a str> {
        plan.ops.iter().find_map(|op| match op {
            Op::WriteFile {
                path: candidate,
                content,
                ..
            } if candidate == path => Some(content.as_str()),
            _ => None,
        })
    }

    fn write_mode(plan: &InstallPlan, path: &Path) -> Option<u32> {
        plan.ops.iter().find_map(|op| match op {
            Op::WriteFile {
                path: candidate,
                mode,
                ..
            } if candidate == path => Some(*mode),
            _ => None,
        })
    }

    fn has_run(plan: &InstallPlan, argv: &[&str]) -> bool {
        plan.ops.iter().any(|op| match op {
            Op::Run {
                argv: candidate, ..
            } => candidate.as_slice() == strings(argv).as_slice(),
            _ => false,
        })
    }

    fn find_run<'a>(plan: &'a InstallPlan, prefix: &[&str]) -> Option<&'a Op> {
        plan.ops.iter().find(|op| match op {
            Op::Run { argv, .. } => {
                argv.len() >= prefix.len()
                    && argv
                        .iter()
                        .zip(prefix)
                        .all(|(value, expected)| value.as_str() == *expected)
            }
            _ => false,
        })
    }

    #[test]
    fn detection_follows_the_documented_order() {
        let probe = |os, systemd_runtime, rc_common, initd_writable| Probe {
            os,
            systemd_runtime,
            rc_common,
            initd_writable,
        };

        assert_eq!(detect(&probe(Os::Linux, true, true, true)), Target::Systemd);
        assert_eq!(detect(&probe(Os::Linux, false, true, true)), Target::Procd);
        assert_eq!(
            detect(&probe(Os::Linux, false, true, false)),
            Target::Busybox
        );
        assert_eq!(
            detect(&probe(Os::Linux, false, false, false)),
            Target::Busybox
        );
        assert_eq!(
            detect(&probe(Os::Mac, false, false, false)),
            Target::Launchd
        );
        assert_eq!(
            detect(&probe(Os::Windows, false, false, false)),
            Target::Scm
        );
    }

    #[test]
    fn systemd_unit_matches_the_contract() {
        let plan = install_plan(Target::Systemd, &layout());
        let path = PathBuf::from(SYSTEMD_UNIT);
        let unit = written(&plan, &path).expect("systemd unit is written");

        for needle in [
            "Type=notify",
            "Restart=on-failure",
            "RestartSec=1",
            "TimeoutStopSec=40",
            "KillMode=control-group",
            "StartLimitIntervalSec=10",
            "StartLimitBurst=5",
            "ExecStart=/usr/sbin/svcd run --foreground",
            "WantedBy=multi-user.target",
        ] {
            assert!(unit.contains(needle), "missing {:?} in:\n{}", needle, unit);
        }
        assert_eq!(write_mode(&plan, &path), Some(0o644));
        assert!(has_run(&plan, &["systemctl", "daemon-reload"]));
        assert!(has_run(&plan, &["systemctl", "enable", "--now", "svcd"]));
    }

    #[test]
    fn systemd_uninstall_reverses_install() {
        let plan = uninstall_plan(Target::Systemd, &layout(), false);

        assert!(has_run(&plan, &["systemctl", "disable", "--now", "svcd"]));
        assert!(has_run(&plan, &["systemctl", "daemon-reload"]));
        assert!(plan.ops.iter().any(
            |op| matches!(op, Op::RemoveFile { path } if path == &PathBuf::from(SYSTEMD_UNIT))
        ));
    }

    #[test]
    fn launchd_plist_matches_the_contract() {
        let plan = install_plan(Target::Launchd, &layout());
        let path = PathBuf::from(LAUNCHD_PLIST);
        let plist = written(&plan, &path).expect("launchd plist is written");

        for needle in [
            "<string>com.webxxxxx.svcd</string>",
            "<key>KeepAlive</key>",
            "<integer>10</integer>",
            "<string>Background</string>",
            "<integer>30</integer>",
            "<string>/usr/sbin/svcd</string>",
        ] {
            assert!(
                plist.contains(needle),
                "missing {:?} in:\n{}",
                needle,
                plist
            );
        }
        assert!(has_run(
            &plan,
            &["launchctl", "bootstrap", "system", LAUNCHD_PLIST]
        ));
        assert!(has_run(
            &plan,
            &["launchctl", "enable", "system/com.webxxxxx.svcd"]
        ));
        assert!(has_run(
            &plan,
            &["launchctl", "kickstart", "-k", "system/com.webxxxxx.svcd"]
        ));
    }

    #[test]
    fn scm_plan_uses_the_service_control_manager_commands() {
        let plan = install_plan(Target::Scm, &layout());
        let create = find_run(&plan, &["sc", "create"]).expect("sc create");
        match create {
            Op::Run { argv, allow_codes } => {
                assert!(argv.contains(&"binPath=".to_string()));
                assert!(argv.iter().any(|value| value.contains("run --foreground")));
                assert_eq!(allow_codes.as_deref(), Some(&[0, 1073][..]));
            }
            _ => unreachable!("matched run"),
        }

        assert!(find_run(&plan, &["sc", "description"]).is_some());
        let failure = find_run(&plan, &["sc", "failure"]).expect("sc failure");
        match failure {
            Op::Run { argv, .. } => {
                assert!(argv.contains(&"reset=".to_string()));
                assert!(argv.contains(&"86400".to_string()));
                assert!(argv.contains(&"actions=".to_string()));
                assert!(argv.contains(&"restart/5000/restart/10000/restart/30000".to_string()));
            }
            _ => unreachable!("matched run"),
        }
        assert!(find_run(&plan, &["sc", "config"]).is_some());
        assert!(has_run(&plan, &["sc", "start", SCM_NAME]));

        let uninstall = uninstall_plan(Target::Scm, &layout(), false);
        let delete = find_run(&uninstall, &["sc", "delete"]).expect("sc delete");
        match delete {
            Op::Run { allow_codes, .. } => {
                assert!(allow_codes
                    .as_deref()
                    .is_some_and(|codes| codes.contains(&1060)));
            }
            _ => unreachable!("matched run"),
        }
    }

    #[test]
    fn busybox_plan_ensures_one_exact_respawn_line() {
        let plan = install_plan(Target::Busybox, &layout());
        let line = "::respawn:/usr/sbin/svcd run --foreground";
        assert!(plan
            .ops
            .iter()
            .any(|op| matches!(op, Op::EnsureLine { path, line: candidate } if path == &PathBuf::from(INITTAB) && candidate == line)));
        assert!(has_run(&plan, &["kill", "-HUP", "1"]));
        match find_run(&plan, &["kill", "-HUP", "1"]).expect("kill -HUP 1") {
            Op::Run { allow_codes, .. } => assert!(allow_codes.is_none()),
            _ => unreachable!("matched run"),
        }

        let uninstall = uninstall_plan(Target::Busybox, &layout(), false);
        assert!(uninstall
            .ops
            .iter()
            .any(|op| matches!(op, Op::RemoveLine { path, line: candidate } if path == &PathBuf::from(INITTAB) && candidate == line)));
    }

    #[test]
    fn procd_script_matches_the_contract() {
        let plan = install_plan(Target::Procd, &layout());
        let path = PathBuf::from(PROCD_SCRIPT);
        let script = written(&plan, &path).expect("procd script is written");

        for needle in [
            "USE_PROCD=1",
            "procd_set_param command /usr/sbin/svcd run --foreground",
            "procd_set_param respawn 3600 5 5",
            "procd_set_param term_timeout 30",
        ] {
            assert!(
                script.contains(needle),
                "missing {:?} in:\n{}",
                needle,
                script
            );
        }
        assert_eq!(write_mode(&plan, &path), Some(0o755));
        assert!(has_run(&plan, &["chmod", "0755", PROCD_SCRIPT]));
        assert!(has_run(&plan, &[PROCD_SCRIPT, "enable"]));
        assert!(has_run(&plan, &[PROCD_SCRIPT, "start"]));

        let uninstall = uninstall_plan(Target::Procd, &layout(), false);
        assert!(has_run(&uninstall, &[PROCD_SCRIPT, "stop"]));
        assert!(has_run(&uninstall, &[PROCD_SCRIPT, "disable"]));
        assert!(uninstall.ops.iter().any(
            |op| matches!(op, Op::RemoveFile { path } if path == &PathBuf::from(PROCD_SCRIPT))
        ));
    }

    #[test]
    fn purge_removes_data_and_config_while_default_keeps_them() {
        let keep = uninstall_plan(Target::Systemd, &layout(), false);
        assert!(!keep.ops.iter().any(|op| matches!(op, Op::RemoveDir { .. })));

        let purge = uninstall_plan(Target::Systemd, &layout(), true);
        assert!(purge.ops.iter().any(
            |op| matches!(op, Op::RemoveDir { path } if path == &PathBuf::from("/var/lib/svcd"))
        ));
        assert!(purge
            .ops
            .iter()
            .any(|op| matches!(op, Op::RemoveDir { path } if path == &PathBuf::from("/etc/svcd"))));
    }

    #[test]
    fn every_target_has_an_install_and_an_uninstall_plan() {
        for target in [
            Target::Systemd,
            Target::Launchd,
            Target::Scm,
            Target::Busybox,
            Target::Procd,
        ] {
            let install = install_plan(target, &layout());
            assert_eq!(install.target, target);
            assert_eq!(install.action, Action::Install);
            assert!(!install.ops.is_empty(), "{} install plan is empty", target);

            let uninstall = uninstall_plan(target, &layout(), false);
            assert_eq!(uninstall.target, target);
            assert_eq!(uninstall.action, Action::Uninstall);
            assert!(
                !uninstall.ops.is_empty(),
                "{} uninstall plan is empty",
                target
            );
        }
    }

    #[test]
    fn parses_flags_in_any_order_and_as_equals_form() {
        assert_eq!(
            parse_request(
                Action::Install,
                &args(&["--dry-run", "--target", "systemd"])
            )
            .expect("install flags"),
            Request {
                target: Some(Target::Systemd),
                dry_run: true,
                purge: false,
            }
        );
        assert_eq!(
            parse_request(
                Action::Uninstall,
                &args(&["--target=procd", "--purge", "--dry-run"])
            )
            .expect("uninstall flags"),
            Request {
                target: Some(Target::Procd),
                dry_run: true,
                purge: true,
            }
        );
    }

    #[test]
    fn rejects_usage_errors() {
        for (action, values) in [
            (Action::Install, vec!["--purge"]),
            (Action::Install, vec!["--target"]),
            (Action::Install, vec!["--target", "nope"]),
            (Action::Install, vec!["--target=systemd", "--target=procd"]),
            (Action::Install, vec!["--dry-run", "--dry-run"]),
            (Action::Install, vec!["extra"]),
            (Action::Uninstall, vec!["--target", "systemd", "extra"]),
            (Action::Uninstall, vec!["--purge", "--purge"]),
        ] {
            let values = args(&values);
            assert!(
                parse_request(action, &values).is_err(),
                "{:?} should be rejected",
                values
            );
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "svcd-install-{}-{}-{}",
                std::process::id(),
                nanos,
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_operations_are_idempotent_and_remove_is_optional() {
        let directory = TempDir::new();
        let inittab = directory.0.join("inittab");
        let line = "::respawn:/usr/sbin/svcd run --foreground";

        let install = InstallPlan {
            target: Target::Busybox,
            action: Action::Install,
            ops: vec![
                Op::EnsureLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::EnsureLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::AssertFileContains {
                    path: inittab.clone(),
                    needle: line.into(),
                },
            ],
        };
        execute(&install).expect("install executes");
        execute(&install).expect("reinstall executes");
        assert_eq!(
            fs::read_to_string(&inittab).expect("read inittab"),
            format!("{}\n", line)
        );

        let uninstall = InstallPlan {
            target: Target::Busybox,
            action: Action::Uninstall,
            ops: vec![
                Op::RemoveLine {
                    path: inittab.clone(),
                    line: line.into(),
                },
                Op::RemoveDir {
                    path: directory.0.join("absent/data"),
                },
                Op::RemoveFile {
                    path: directory.0.join("absent/file"),
                },
            ],
        };
        execute(&uninstall).expect("uninstall executes");
        execute(&uninstall).expect("uninstall is idempotent");
        assert_eq!(fs::read_to_string(&inittab).expect("read inittab"), "");
    }

    #[test]
    fn write_file_replaces_atomically_and_keeps_no_temporary_files() {
        let directory = TempDir::new();
        let path = directory.0.join("unit");

        execute(&InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![Op::WriteFile {
                path: path.clone(),
                content: "one\n".into(),
                mode: 0o644,
            }],
        })
        .expect("first write");

        execute(&InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![Op::WriteFile {
                path: path.clone(),
                content: "two\n".into(),
                mode: 0o644,
            }],
        })
        .expect("second write");

        assert_eq!(fs::read_to_string(&path).expect("read unit"), "two\n");
        let leftovers: Vec<_> = fs::read_dir(&directory.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temporary files left: {:?}",
            leftovers
        );
    }

    #[test]
    fn a_failed_assertion_stops_the_plan() {
        let directory = TempDir::new();
        let path = directory.0.join("unit");
        let plan = InstallPlan {
            target: Target::Systemd,
            action: Action::Install,
            ops: vec![
                Op::WriteFile {
                    path: path.clone(),
                    content: "Type=simple\n".into(),
                    mode: 0o644,
                },
                Op::AssertFileContains {
                    path,
                    needle: "Type=notify".into(),
                },
            ],
        };

        let error = execute(&plan).expect_err("assertion fails");
        assert!(error.contains("Type=notify"), "{}", error);
    }
}
