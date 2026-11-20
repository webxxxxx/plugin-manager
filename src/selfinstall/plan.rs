//! 安装计划：一组有序的宿主操作，以及由它生成的服务定义文本。
//!
//! Everything in this file is a pure function of its inputs: given a
//! [`Layout`] / [`Probe`] / [`Request`], the same plan comes out. Reading the
//! environment, the filesystem or the clock belongs to [`super::execute`],
//! which turns a plan into effects (#207).

use std::fmt;
use std::path::{Path, PathBuf};

pub(crate) const EXIT_OK: i32 = 0;

pub(crate) const EXIT_ERROR: i32 = 1;

pub(crate) const EXIT_USAGE: i32 = 64;

pub(crate) const SYSTEMD_UNIT: &str = "/etc/systemd/system/svcd.service";

pub(crate) const LAUNCHD_LABEL: &str = "com.webxxxxx.svcd";

pub(crate) const LAUNCHD_PLIST: &str = "/Library/LaunchDaemons/com.webxxxxx.svcd.plist";

pub(crate) const INITTAB: &str = "/etc/inittab";

pub(crate) const PROCD_SCRIPT: &str = "/etc/init.d/svcd";

pub(crate) const SCM_NAME: &str = "svcd";

pub(crate) const SCM_DISPLAY_NAME: &str = "svcd";

/// The local group that shares the control channel's read endpoint. The channel
/// authorizes by endpoint ACL, so the group has to exist before the service
/// starts.
pub(crate) const SCM_GROUP: &str = "svcd";

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
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Action::Install => "install-service",
            Action::Uninstall => "uninstall-service",
        }
    }

    pub(crate) fn past(self) -> &'static str {
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

pub(crate) fn current_os() -> Os {
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

/// Everything a plan needs to spell out paths and command lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub program: PathBuf,
    pub data_dir: PathBuf,
    pub config_dir: PathBuf,
    pub conf: Option<PathBuf>,
}

pub(crate) fn conf_arg(layout: &Layout) -> String {
    layout
        .conf
        .as_ref()
        .map(|path| format!(" --conf \"{}\"", path.display()))
        .unwrap_or_default()
}

/// The directories the installed daemon will use at runtime. This is a view
/// of the same [`crate::platform::paths::Paths`] the daemon resolves; there is
/// no installer-specific precedence chain (ADR-0023).
pub(crate) struct RuntimeDirs {
    pub(crate) config: PathBuf,
    pub(crate) data: PathBuf,
    pub(crate) logs: PathBuf,
}

impl RuntimeDirs {
    pub(crate) fn from_paths(paths: &crate::platform::paths::Paths) -> Self {
        RuntimeDirs {
            config: paths.config_dir.clone(),
            data: paths.data_dir.clone(),
            logs: paths.logs_dir.clone(),
        }
    }

    pub(crate) fn conf(&self) -> PathBuf {
        self.config.join("svcd.conf")
    }
}

/// The manual launcher name for the host this installer runs on.
pub(crate) fn launcher_name() -> &'static str {
    if cfg!(windows) {
        "svcd_daemon.bat"
    } else {
        "svcd_daemon.sh"
    }
}

/// The launcher written next to the binary by `--manual`.
///
/// It exports the paths `svcd.conf` cannot carry (`SVCD_DATA_DIR`,
/// `SVCD_LOG_DIR`) and `PATH`, then starts the daemon. On Unix it `exec`s the
/// daemon so a BusyBox `::respawn:` entry supervises the daemon itself rather
/// than a wrapper shell.
pub(crate) fn daemon_launcher(program: &Path, dirs: &RuntimeDirs) -> String {
    let program_dir = program.parent().unwrap_or(Path::new("."));
    let launcher = program.with_file_name(launcher_name());
    if cfg!(windows) {
        format!(
            "@echo off\r\n\
             rem Generated by `svcd --install --manual`.\r\n\
             rem Run this file by hand or from the firmware; it exports the paths below.\r\n\
             set \"PATH={program_dir};%PATH%\"\r\n\
             set \"SVCD_DATA_DIR={data}\"\r\n\
             set \"SVCD_LOG_DIR={logs}\"\r\n\
             \"{program}\" run --foreground --conf \"{conf}\"\r\n",
            program_dir = program_dir.display(),
            data = dirs.data.display(),
            logs = dirs.logs.display(),
            program = program.display(),
            conf = dirs.conf().display(),
        )
    } else {
        format!(
            "#!/bin/sh\n\
             # Generated by `svcd --install --manual`. Add this file to the\n\
             # firmware image or run it by hand, for example:\n\
             #   ::respawn:{launcher}\n\
             # The exports keep data and logs on the persistent tree when /usr\n\
             # is read-only and /var is not persistent.\n\
             export PATH=\"{program_dir}:$PATH\"\n\
             export SVCD_DATA_DIR=\"{data}\"\n\
             export SVCD_LOG_DIR=\"{logs}\"\n\
             exec \"{program}\" run --foreground --conf \"{conf}\"\n",
            launcher = launcher.display(),
            program_dir = program_dir.display(),
            data = dirs.data.display(),
            logs = dirs.logs.display(),
            program = program.display(),
            conf = dirs.conf().display(),
        )
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

pub(crate) fn systemd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(SYSTEMD_UNIT);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: systemd_unit(layout),
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

pub(crate) fn systemd_uninstall() -> Vec<Op> {
    vec![
        run_allowing(&["systemctl", "disable", "--now", "svcd"], &[0, 1, 3, 4, 5]),
        Op::RemoveFile {
            path: PathBuf::from(SYSTEMD_UNIT),
        },
        run(&["systemctl", "daemon-reload"]),
        run_allowing(&["systemctl", "reset-failed", "svcd"], &[0, 1, 3, 4, 5]),
    ]
}

pub(crate) fn launchd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(LAUNCHD_PLIST);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: launchd_plist(layout),
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

pub(crate) fn launchd_uninstall() -> Vec<Op> {
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

pub(crate) fn scm_install(layout: &Layout) -> Vec<Op> {
    let bin_path = format!(
        "\"{}\" run --foreground{}",
        layout.program.display(),
        conf_arg(layout)
    );
    vec![
        // The read endpoint is shared with this group, so it must exist before
        // the daemon binds. `net.exe` exits 2 for every failure; 1379
        // (ERROR_ALIAS_EXISTS) only appears in the message, so a reinstall
        // cannot key off the exit code alone. Tolerate exit 2 here and prove
        // the group below: a real failure (access denied, no SAM) still fails
        // the verify step and aborts before the service is created.
        Op::Run {
            argv: vec![
                "net".into(),
                "localgroup".into(),
                SCM_GROUP.into(),
                "/add".into(),
            ],
            allow_codes: Some(vec![0, 2]),
        },
        Op::Run {
            argv: vec!["net".into(), "localgroup".into(), SCM_GROUP.into()],
            allow_codes: Some(vec![0]),
        },
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

pub(crate) fn scm_uninstall() -> Vec<Op> {
    vec![
        // 1051: not started, 1060: no such service, 1062: not active.
        run_allowing(&["sc", "stop", SCM_NAME], &[0, 1051, 1060, 1062]),
        // 1060: no such service, 1072: marked for deletion.
        run_allowing(&["sc", "delete", SCM_NAME], &[0, 1060, 1072]),
    ]
}

pub(crate) fn busybox_install(layout: &Layout) -> Vec<Op> {
    let line = format!(
        "::respawn:{} run --foreground{}",
        layout.program.display(),
        conf_arg(layout)
    );
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

pub(crate) fn busybox_uninstall(layout: &Layout) -> Vec<Op> {
    let line = format!(
        "::respawn:{} run --foreground{}",
        layout.program.display(),
        conf_arg(layout)
    );
    vec![
        Op::RemoveLine {
            path: PathBuf::from(INITTAB),
            line,
        },
        run_best_effort(&["kill", "-HUP", "1"]),
    ]
}

pub(crate) fn procd_install(layout: &Layout) -> Vec<Op> {
    let path = PathBuf::from(PROCD_SCRIPT);
    vec![
        Op::WriteFile {
            path: path.clone(),
            content: procd_script(layout),
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

pub(crate) fn procd_uninstall() -> Vec<Op> {
    vec![
        run_allowing(&[PROCD_SCRIPT, "stop"], &[0, 1]),
        run_allowing(&[PROCD_SCRIPT, "disable"], &[0, 1]),
        Op::RemoveFile {
            path: PathBuf::from(PROCD_SCRIPT),
        },
    ]
}

pub(crate) fn systemd_unit(layout: &Layout) -> String {
    let program = layout.program.display();
    let conf = conf_arg(layout);
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
         ExecStart={program} run --foreground{conf}\n\
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

pub(crate) fn launchd_domain() -> String {
    format!("system/{}", LAUNCHD_LABEL)
}

pub(crate) fn launchd_plist(layout: &Layout) -> String {
    let label = LAUNCHD_LABEL;
    let program = xml_escape(&layout.program.display().to_string());
    let conf = layout
        .conf
        .as_ref()
        .map(|path| {
            format!(
                "\t\t<string>--conf</string>\n\t\t<string>{}</string>\n",
                xml_escape(&path.display().to_string())
            )
        })
        .unwrap_or_default();
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
         {conf}\t</array>\n\
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

pub(crate) fn procd_script(layout: &Layout) -> String {
    let program = layout.program.display();
    let conf = conf_arg(layout);
    format!(
        "#!/bin/sh /etc/rc.common\n\
         # Managed by `svcd install-service`; reinstall overwrites this file.\n\
         USE_PROCD=1\n\
         START=95\n\
         STOP=10\n\
         \n\
         start_service() {{\n\
         \tprocd_open_instance\n\
         \tprocd_set_param command {program} run --foreground{conf}\n\
         \tprocd_set_param respawn 3600 5 5\n\
         \tprocd_set_param term_timeout 30\n\
         \tprocd_close_instance\n\
         }}\n"
    )
}

pub(crate) fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(crate) fn run(argv: &[&str]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: Some(vec![0]),
    }
}

pub(crate) fn run_allowing(argv: &[&str], codes: &[i32]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: Some(codes.to_vec()),
    }
}

pub(crate) fn run_best_effort(argv: &[&str]) -> Op {
    Op::Run {
        argv: strings(argv),
        allow_codes: None,
    }
}

pub(crate) fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) target: Option<Target>,
    pub(crate) dry_run: bool,
    pub(crate) purge: bool,
    pub(crate) program: Option<PathBuf>,
    pub(crate) conf: Option<PathBuf>,
}

pub(crate) fn set_target(request: &mut Request, value: &str) -> Result<(), String> {
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

pub(crate) fn parse_request(action: Action, args: &[String]) -> Result<Request, String> {
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
            "--program" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--program requires a value".to_string())?;
                request.program = Some(PathBuf::from(value));
            }
            "--conf" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--conf requires a value".to_string())?;
                if request.conf.replace(PathBuf::from(value)).is_some() {
                    return Err("--conf specified more than once".into());
                }
            }
            other => match other.strip_prefix("--target=") {
                Some(value) => set_target(&mut request, value)?,
                None => match other.strip_prefix("--program=") {
                    Some(value) if !value.is_empty() => {
                        request.program = Some(PathBuf::from(value));
                    }
                    Some(_) => return Err("--program requires a value".into()),
                    None => match other.strip_prefix("--conf=") {
                        Some(value) if !value.is_empty() => {
                            if request.conf.replace(PathBuf::from(value)).is_some() {
                                return Err("--conf specified more than once".into());
                            }
                        }
                        Some(_) => return Err("--conf requires a value".into()),
                        None => return Err(format!("unexpected argument: {}", other)),
                    },
                },
            },
        }
    }
    Ok(request)
}

pub(crate) fn alias_names() -> Vec<&'static str> {
    if cfg!(windows) {
        vec!["svc.exe", "plugin.exe", "mcp.exe"]
    } else {
        vec!["svc", "plugin", "mcp"]
    }
}

/// The plan for registering (or removing) the daemon with the host init
/// system: install-service / uninstall-service and `--install` share it.
pub(crate) fn service_plan(
    action: Action,
    target: Target,
    layout: &Layout,
    purge: bool,
) -> InstallPlan {
    match action {
        Action::Install => install_plan(target, layout),
        Action::Uninstall => uninstall_plan(target, layout, purge),
    }
}

/// Everything `--install` decides before it touches the host.
///
/// `--dry-run` prints this value and the real run executes the same value, so
/// the printed plan and the executed plan cannot drift (#207).
pub(crate) struct SelfInstallPlan {
    pub(crate) install_root: PathBuf,
    pub(crate) target: PathBuf,
    pub(crate) aliases: Vec<String>,
    pub(crate) runtime: RuntimeDirs,
    pub(crate) record_install_dir: bool,
    pub(crate) launcher: Option<PathBuf>,
    /// Registration plan; `None` for `--manual` and `--no-service`.
    pub(crate) service: Option<InstallPlan>,
    /// The removal that runs before a reinstall, with the same target and
    /// layout as [`SelfInstallPlan::service`].
    pub(crate) previous_service: Option<InstallPlan>,
}

/// The inputs of [`plan_self_install`]: every value that needs the environment
/// is resolved by the caller (`super::execute`).
pub(crate) struct SelfInstallInputs<'a> {
    pub(crate) install_root: &'a Path,
    pub(crate) runtime: RuntimeDirs,
    pub(crate) config_path: Option<&'a Path>,
    pub(crate) manual: bool,
    pub(crate) no_service: bool,
    pub(crate) record_install_dir: bool,
    /// The init system to register with, already detected by the caller.
    pub(crate) service_target: Target,
}

/// Pick the install root: explicit `--prefix`, then the `install_dir` recorded
/// in the target config, then the host default.
pub(crate) fn resolve_install_root(
    prefix: Option<&Path>,
    recorded: Option<&Path>,
    default_prefix: PathBuf,
) -> PathBuf {
    prefix
        .map(Path::to_path_buf)
        .or_else(|| recorded.map(Path::to_path_buf))
        .unwrap_or(default_prefix)
}

pub(crate) fn plan_self_install(input: SelfInstallInputs<'_>) -> SelfInstallPlan {
    let target_name = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let target = input.install_root.join(target_name);
    let launcher = input
        .manual
        .then(|| input.install_root.join(launcher_name()));
    let register_service = !input.no_service && !input.manual;
    let layout = Layout {
        program: target.clone(),
        data_dir: input.runtime.data.clone(),
        config_dir: input.runtime.config.clone(),
        conf: input.config_path.map(Path::to_path_buf),
    };
    let service = register_service
        .then(|| service_plan(Action::Install, input.service_target, &layout, false));
    let previous_service = register_service
        .then(|| service_plan(Action::Uninstall, input.service_target, &layout, false));
    SelfInstallPlan {
        install_root: input.install_root.to_path_buf(),
        target,
        aliases: alias_names().into_iter().map(str::to_string).collect(),
        runtime: input.runtime,
        record_install_dir: input.record_install_dir,
        launcher,
        service,
        previous_service,
    }
}

#[cfg(test)]
mod tests;
