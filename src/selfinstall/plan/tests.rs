#![cfg(test)]

use super::*;

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

fn layout() -> Layout {
    Layout {
        program: PathBuf::from("/usr/sbin/svcd"),
        data_dir: PathBuf::from("/var/lib/svcd"),
        config_dir: PathBuf::from("/etc/svcd"),
        conf: None,
    }
}

fn layout_with_conf() -> Layout {
    Layout {
        conf: Some(PathBuf::from("/etc/svcd/custom.conf")),
        ..layout()
    }
}

fn plan_text(plan: &InstallPlan) -> String {
    let mut text = String::new();
    for op in &plan.ops {
        match op {
            Op::WriteFile { content, .. } => text.push_str(content),
            Op::EnsureLine { line, .. } => text.push_str(line),
            Op::Run { argv, .. } => text.push_str(&argv.join(" ")),
            _ => {}
        }
        text.push('\n');
    }
    text
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
fn generated_conf_uses_the_materialized_roots() {
    let layout = layout();
    let settings = crate::platform::settings::Settings {
        config_dir: Some(layout.config_dir.clone()),
        data_dir: Some(layout.data_dir.clone()),
        logs_dir: Some(PathBuf::from("/var/log/svcd")),
        ..crate::platform::settings::Settings::default()
    };
    let text = crate::platform::config_file::generated(&settings);
    assert!(text.contains("bind = 127.0.0.1:8787"), "{text}");
    assert!(text.contains("allow_insecure_install = false"), "{text}");
    assert!(
        text.contains(&format!("config_dir = {}", layout.config_dir.display())),
        "{text}"
    );
    assert!(text.contains("svc lan passwd"), "{text}");
    assert!(text.contains("blocking_threads = 4 / 8 / 16"), "{text}");
    assert!(
        text.contains(&format!("data_dir = {}", layout.data_dir.display())),
        "{text}"
    );
    assert!(text.contains("logs_dir = /var/log/svcd"), "{text}");
    // Derived directories appear only in comments, never as keys.
    assert!(text.contains("#   plugins    = "), "{text}");
    assert!(text.contains("#   services.d = "), "{text}");
    assert!(text.contains("#   temp       = "), "{text}");
    assert!(!text.contains("\nplugins_dir = "), "{text}");
    assert!(!text.contains("\nservices_dir = "), "{text}");
    assert!(!text.contains("\ntemp_dir = "), "{text}");
    assert!(text.contains("temp_retention_secs = 7200"), "{text}");
    assert!(text.contains("temp_cleanup_interval_secs = 300"), "{text}");
    assert!(text.contains("control_timeout_secs = 30"), "{text}");
}

#[test]
fn self_install_services_use_the_installed_conf() {
    let install_root = PathBuf::from("/opt/svcd");
    let runtime = RuntimeDirs {
        config: install_root.join("config"),
        data: install_root.join("data"),
        logs: install_root.join("logs"),
    };
    let installed = runtime.conf();
    let plan = plan_self_install(SelfInstallInputs {
        install_root: &install_root,
        runtime,
        config_path: Some(installed.as_path()),
        manual: false,
        no_service: false,
        record_install_dir: true,
        service_target: Target::Systemd,
    });
    let text = plan_text(plan.service.as_ref().expect("service plan"));
    assert!(
        text.contains(&format!("--conf \"{}\"", installed.display())),
        "{text}"
    );
}

#[test]
fn service_plans_pass_the_explicit_conf() {
    let layout = layout_with_conf();
    for target in [
        Target::Systemd,
        Target::Launchd,
        Target::Scm,
        Target::Busybox,
        Target::Procd,
    ] {
        let text = plan_text(&install_plan(target, &layout));
        assert!(
            text.contains("--conf") && text.contains("/etc/svcd/custom.conf"),
            "{} plan is missing --conf:\n{}",
            target.as_str(),
            text
        );
    }
}

#[test]
fn example_conf_file_documents_the_keys() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/svcd.conf.example"))
        .expect("svcd.conf.example");
    for key in [
        "bind",
        "allow_insecure_install",
        "config_dir",
        "data_dir",
        "temp_retention_secs",
        "temp_cleanup_interval_secs",
        "logs_dir",
        "log_max_bytes",
        "log_rotations",
        "log_to_file",
        "request_stack_bytes",
        "io_stack_bytes",
        "blocking_threads",
        "max_connections",
        "max_connections_per_ip",
        "max_subscriptions",
        "max_subscribers_per_plugin",
        "http_idle_timeout_secs",
        "max_package_bytes",
        "control_timeout_secs",
        "auth_passwd",
    ] {
        assert!(text.contains(key), "example is missing {key}");
    }
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
    assert!(plan
        .ops
        .iter()
        .any(|op| matches!(op, Op::RemoveFile { path } if path == &PathBuf::from(SYSTEMD_UNIT))));
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
fn scm_plan_tolerates_net_exit_two_then_proves_the_group() {
    // `net.exe` returns 2 both for "already exists" (1379) and for real
    // failures, so the plan cannot stop at the add; the verify op is what
    // keeps a refused group creation from reaching `sc create`.
    let plan = install_plan(Target::Scm, &layout());
    let group: Vec<&Op> = plan
        .ops
        .iter()
        .filter(|op| matches!(op, Op::Run { argv, .. } if argv.first().map(String::as_str) == Some("net")))
        .collect();
    assert_eq!(group.len(), 2, "{group:?}");
    match group[0] {
        Op::Run { argv, allow_codes } => {
            assert_eq!(argv, &["net", "localgroup", SCM_GROUP, "/add"]);
            assert_eq!(allow_codes.as_deref(), Some(&[0, 2][..]));
        }
        _ => unreachable!("filtered Run"),
    }
    match group[1] {
        Op::Run { argv, allow_codes } => {
            assert_eq!(argv, &["net", "localgroup", SCM_GROUP]);
            assert_eq!(allow_codes.as_deref(), Some(&[0][..]));
        }
        _ => unreachable!("filtered Run"),
    }
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
    assert!(uninstall
        .ops
        .iter()
        .any(|op| matches!(op, Op::RemoveFile { path } if path == &PathBuf::from(PROCD_SCRIPT))));
}

#[test]
fn purge_removes_data_and_config_while_default_keeps_them() {
    let keep = uninstall_plan(Target::Systemd, &layout(), false);
    assert!(!keep.ops.iter().any(|op| matches!(op, Op::RemoveDir { .. })));

    let purge = uninstall_plan(Target::Systemd, &layout(), true);
    assert!(purge
        .ops
        .iter()
        .any(|op| matches!(op, Op::RemoveDir { path } if path == &PathBuf::from("/var/lib/svcd"))));
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
            program: None,
            conf: None,
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
            program: None,
            conf: None,
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
#[test]
fn the_self_install_plan_is_the_only_decision() {
    let root = Path::new("/opt/svcd");
    let runtime = || RuntimeDirs {
        config: PathBuf::from("/etc/svcd"),
        data: PathBuf::from("/var/lib/svcd"),
        logs: PathBuf::from("/var/log/svcd"),
    };
    let planned = |manual: bool, no_service: bool| {
        plan_self_install(SelfInstallInputs {
            install_root: root,
            runtime: runtime(),
            config_path: None,
            manual,
            no_service,
            record_install_dir: true,
            service_target: Target::Systemd,
        })
    };

    let program = if cfg!(windows) { "svcd.exe" } else { "svcd" };
    let launcher = if cfg!(windows) {
        "svcd_daemon.bat"
    } else {
        "svcd_daemon.sh"
    };
    let plain = planned(false, false);
    assert!(plain.record_install_dir);
    assert_eq!(plain.target, root.join(program));
    assert!(
        plain.service.is_some(),
        "a plain install registers a service"
    );
    assert!(plain.previous_service.is_some());
    assert!(plain.launcher.is_none());

    let manual = planned(true, false);
    assert!(manual.service.is_none(), "--manual registers nothing");
    assert!(manual.previous_service.is_none());
    let expected_launcher = root.join(launcher);
    assert_eq!(
        manual.launcher.as_deref(),
        Some(expected_launcher.as_path())
    );

    let bare = planned(false, true);
    assert!(bare.service.is_none(), "--no-service registers nothing");
    assert!(bare.launcher.is_none());
}
