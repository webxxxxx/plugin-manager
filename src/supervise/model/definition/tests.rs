#![cfg(test)]

use super::*;
use crate::platform::test_support::TempDir;
use serde_json::json;

fn services_dir(dir: &TempDir) -> PathBuf {
    dir.0.join("services.d")
}

fn write_definition(dir: &TempDir, name: &str, definition: Value) {
    fs::create_dir_all(services_dir(dir)).expect("create services.d");
    fs::write(
        services_dir(dir).join(format!("{}.json", name)),
        serde_json::to_vec_pretty(&definition).expect("serialize definition"),
    )
    .expect("write definition");
}

fn disable_definition(dir: &TempDir, id: &str) {
    fs::create_dir_all(services_dir(dir)).expect("create services.d");
    fs::write(services_dir(dir).join(format!("{}.disabled", id)), "")
        .expect("write disabled marker");
}

fn write_script(dir: &TempDir, name: &str, body: &str) {
    fs::create_dir_all(services_dir(dir)).expect("create services.d");
    let path = services_dir(dir).join(name);
    fs::write(&path, body).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }
}

fn absolute_exec() -> String {
    if cfg!(windows) {
        r"C:\Windows\System32\cmd.exe".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn script_name(id: &str) -> String {
    if cfg!(windows) {
        format!("{}.cmd", id)
    } else {
        format!("{}.sh", id)
    }
}

fn all_actions_body() -> &'static str {
    #[cfg(windows)]
    {
        "@echo off\r\n:start\r\n:stop\r\n:status\r\n"
    }
    #[cfg(not(windows))]
    {
        "start() { :; }\nstop() { :; }\nstatus() { :; }\n"
    }
}

fn start_only_body() -> &'static str {
    #[cfg(windows)]
    {
        "@echo off\r\n:start\r\n"
    }
    #[cfg(not(windows))]
    {
        "start() { :; }\n"
    }
}

fn definition(id: &str) -> Value {
    json!({
        "schema": 1,
        "id": id,
        "version": "1.0.0",
        "desc": "demo service",
        "protocol": 1,
        "exec": { "path": absolute_exec(), "args": [], "ready": "exec" },
        "dependencies": []
    })
}

#[test]
fn load_issue_display_names_the_file_or_the_directory() {
    let issue = LoadIssue {
        file: Some("demo.json".into()),
        id: "demo".into(),
        reason: "bad definition".into(),
    };
    assert_eq!(issue.to_string(), "services.d/demo.json: bad definition");

    let issue = LoadIssue {
        file: None,
        id: String::new(),
        reason: "cannot read services.d".into(),
    };
    assert_eq!(issue.to_string(), "services.d: cannot read services.d");
}

#[test]
fn loads_a_core_definition_next_to_the_builtin_svc() {
    let dir = TempDir::new();
    write_definition(&dir, "demo", definition("demo"));

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let demo = loaded.registry.get("demo").expect("demo is registered");
    assert_eq!(demo.kind, Kind::Core);
    assert_eq!(demo.state, State::Stopped);
    assert_eq!(demo.version, "1.0.0");
    assert_eq!(demo.deps, Vec::<String>::new());
    assert!(demo.contract.surface().is_empty());
    assert!(!demo
        .contract
        .failures
        .contains(&crate::kernel::failure::Class::Cancelled));
    assert_eq!(
        demo.contract.failures.len(),
        crate::kernel::failure::Class::ALL.len() - 1
    );
    assert_eq!(
        demo.contract.limits.max_in_flight,
        crate::kernel::contract::MAX_IN_FLIGHT
    );
    assert!(demo.enabled);
    assert_eq!(demo.exec.as_ref().expect("exec").ready, Ready::Exec);
    let svc = loaded.registry.get(RESERVED_ID).expect("built-in svc");
    assert_eq!(svc.kind, Kind::Core);
    assert_eq!(svc.state, State::Running);
}

#[test]
fn a_missing_services_dir_still_loads_the_builtin() {
    let dir = TempDir::new();
    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty());
    assert_eq!(loaded.registry.values().count(), 1);
    assert!(loaded.registry.get(RESERVED_ID).is_some());
}

#[test]
fn a_disabled_marker_turns_off_enabled() {
    let dir = TempDir::new();
    write_definition(&dir, "demo", definition("demo"));
    disable_definition(&dir, "demo");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    let demo = loaded.registry.get("demo").expect("demo");
    assert!(!demo.enabled);
}

#[test]
fn ignores_unknown_fields() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["future_field"] = json!({"anything": true});
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    assert!(loaded.registry.get("demo").is_some());
}

#[test]
fn rejects_every_spk_only_field() {
    for field in ["platform", "files", "hooks", "config", "ui"] {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body[field] = json!("anything");
        write_definition(&dir, "demo", body);

        let loaded = load_services(
            &services_dir(&dir),
            &crate::platform::limits::test_default(),
        );

        assert!(
            loaded.registry.get("demo").is_none(),
            "{} should be rejected",
            field
        );
        assert_eq!(loaded.issues.len(), 1, "{}: {:?}", field, loaded.issues);
        assert!(
            loaded.issues[0].reason.contains(field),
            "{} reason: {}",
            field,
            loaded.issues[0].reason
        );
        assert!(
            loaded.issues[0].reason.contains(".spk-only"),
            "{} reason: {}",
            field,
            loaded.issues[0].reason
        );
    }
}

#[test]
fn rejects_the_reserved_svc_id() {
    let dir = TempDir::new();
    write_definition(&dir, RESERVED_ID, definition(RESERVED_ID));

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert_eq!(loaded.registry.values().count(), 1);
    assert_eq!(
        loaded.registry.get(RESERVED_ID).expect("builtin").state,
        State::Running
    );
    assert!(loaded.issues[0].reason.contains("reserved"));
    assert_eq!(
        loaded.issues[0].to_string(),
        format!(
            "services.d/{}.json: {}",
            RESERVED_ID, loaded.issues[0].reason
        )
    );
}

#[test]
fn trusts_the_json_id_over_the_file_name() {
    let dir = TempDir::new();
    write_definition(&dir, "some-file", definition("demo"));

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    assert!(loaded.registry.get("demo").is_some());
}

#[test]
fn rejects_two_files_that_claim_the_same_id() {
    let dir = TempDir::new();
    write_definition(&dir, "a", definition("demo"));
    write_definition(&dir, "b", definition("demo"));

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_some());
    assert_eq!(loaded.issues.len(), 1, "issues: {:?}", loaded.issues);
    assert_eq!(loaded.issues[0].file.as_deref(), Some("b.json"));
    assert!(loaded.issues[0].reason.contains("duplicate service id"));
}

#[test]
fn rejects_a_relative_exec_path() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["exec"]["path"] = json!("payload/demo");
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_none());
    assert!(loaded.issues[0].reason.contains("must be absolute"));
}

#[test]
fn rejects_bad_schema_protocol_and_ready() {
    for (field, value, reason) in [
        ("schema", json!(2), "unsupported schema"),
        ("protocol", json!(2), "unsupported protocol"),
    ] {
        let dir = TempDir::new();
        let mut body = definition("demo");
        body[field] = value;
        write_definition(&dir, "demo", body);

        let loaded = load_services(
            &services_dir(&dir),
            &crate::platform::limits::test_default(),
        );

        assert_eq!(loaded.issues.len(), 1, "{}: {:?}", field, loaded.issues);
        assert!(
            loaded.issues[0].reason.contains(reason),
            "{}: {}",
            field,
            loaded.issues[0].reason
        );
    }

    let dir = TempDir::new();
    let mut body = definition("demo");
    body["exec"]["ready"] = json!("later");
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues[0].reason.contains("exec.ready"));
}

#[test]
fn rejects_core_capabilities() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["capabilities"] = json!([{ "name": "status", "desc": "a", "inputSchema": {} }]);
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_none());
    assert!(loaded.issues[0].reason.contains("plugin-only"));
}

#[test]
fn parses_lifecycle_hooks_and_defaults_their_timeouts() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["lifecycle"] = json!({
        "start": { "path": absolute_exec(), "args": ["start"], "timeout": 20 },
        "stop": { "path": absolute_exec(), "args": ["stop"] },
        "status": { "path": absolute_exec(), "args": ["status"] }
    });
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let lifecycle = loaded
        .registry
        .get("demo")
        .expect("demo")
        .lifecycle
        .as_ref()
        .expect("lifecycle");
    assert_eq!(lifecycle.start.as_ref().expect("start").args, ["start"]);
    assert_eq!(lifecycle.start.as_ref().expect("start").timeout_ms, 20_000);
    assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
    assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
}

#[test]
fn rejects_a_relative_lifecycle_path() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["lifecycle"] = json!({"stop": {"path": "scripts/stop"}});
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_none());
    assert!(loaded.issues[0].reason.contains("must be absolute"));
}

#[test]
fn allows_a_dependency_that_is_not_installed_yet() {
    let dir = TempDir::new();
    let mut body = definition("demo");
    body["dependencies"] = json!(["not-installed"]);
    write_definition(&dir, "demo", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    assert!(loaded.registry.get("demo").is_some());
}

#[test]
fn rejects_a_dependency_cycle_and_drops_only_its_members() {
    let dir = TempDir::new();
    let mut a = definition("a");
    a["dependencies"] = json!(["b"]);
    write_definition(&dir, "a", a);
    let mut b = definition("b");
    b["dependencies"] = json!(["a"]);
    write_definition(&dir, "b", b);
    write_definition(&dir, "c", definition("c"));

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("a").is_none());
    assert!(loaded.registry.get("b").is_none());
    assert!(loaded.registry.get("c").is_some());
    let cycle_issues: Vec<&LoadIssue> = loaded
        .issues
        .iter()
        .filter(|issue| issue.reason.contains("dependency cycle"))
        .collect();
    assert_eq!(cycle_issues.len(), 2);
    assert!(cycle_issues[0].reason.contains("a -> b -> a"));
}

#[cfg(unix)]
#[test]
fn a_script_declares_its_restart_policy() {
    let dir = TempDir::new();
    write_script(
        &dir,
        &script_name("oneshot"),
        "#!/bin/sh\nTYPE=oneshot\nstart() { :; }\n",
    );
    write_script(
        &dir,
        &script_name("always"),
        "#!/bin/sh\nRESTART=always\nstart() { :; }\n",
    );

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    assert_eq!(
        loaded.registry.get("oneshot").expect("oneshot").restart,
        Restart::Never
    );
    assert!(
        loaded
            .registry
            .get("oneshot")
            .expect("oneshot")
            .remain_after_exit,
        "a shell service that is not RESTART=always keeps running after a clean exit"
    );
    assert_eq!(
        loaded.registry.get("always").expect("always").restart,
        Restart::Always
    );
    assert!(
        !loaded
            .registry
            .get("always")
            .expect("always")
            .remain_after_exit
    );
}

#[test]
fn a_json_definition_defaults_to_on_failure_and_honours_restart() {
    let dir = TempDir::new();
    write_definition(&dir, "plain", definition("plain"));
    let mut never = definition("never");
    never["restart"] = serde_json::json!("never");
    write_definition(&dir, "never", never);
    let mut one_shot = definition("one-shot");
    one_shot["remain_after_exit"] = serde_json::json!(true);
    write_definition(&dir, "one-shot", one_shot);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    assert_eq!(
        loaded.registry.get("plain").expect("plain").restart,
        Restart::OnFailure
    );
    assert!(
        !loaded
            .registry
            .get("plain")
            .expect("plain")
            .remain_after_exit,
        "core JSON services must opt in explicitly"
    );
    assert_eq!(
        loaded.registry.get("never").expect("never").restart,
        Restart::Never
    );
    assert!(
        loaded
            .registry
            .get("one-shot")
            .expect("one-shot")
            .remain_after_exit
    );
}

#[test]
fn rejects_remain_after_exit_for_a_notify_ready_definition() {
    // #197: a one-shot keeps no process after it exits, but a notify-ready
    // service is a channel peer: callers write into the stdio channel that
    // dies with the process. Accepting the pair lets `svc start` report
    // success while the service has no way left to serve a call.
    for ready in [Some("notify"), None] {
        let dir = TempDir::new();
        let mut body = definition("odd");
        match ready {
            Some(ready) => body["exec"]["ready"] = json!(ready),
            None => {
                body["exec"]
                    .as_object_mut()
                    .expect("exec object")
                    .remove("ready");
            }
        }
        body["remain_after_exit"] = json!(true);
        write_definition(&dir, "odd", body);

        let loaded = load_services(
            &services_dir(&dir),
            &crate::platform::limits::test_default(),
        );

        assert!(
            loaded.registry.get("odd").is_none(),
            "ready={ready:?} must be rejected"
        );
        assert_eq!(
            loaded.issues.len(),
            1,
            "ready={ready:?}: {:?}",
            loaded.issues
        );
        let reason = &loaded.issues[0].reason;
        assert!(
            reason.contains("remain_after_exit"),
            "ready={ready:?}: {reason}"
        );
        assert!(reason.contains("notify"), "ready={ready:?}: {reason}");
    }
}

#[test]
fn rejects_a_started_action_on_a_one_shot_definition() {
    // A one-shot keeps no process after its exit, so there is no ready
    // instance for the post-ready action to gate; accepting the pair would
    // make the action's outcome racy with the exit.
    let dir = TempDir::new();
    let mut body = definition("odd");
    body["remain_after_exit"] = json!(true);
    let hook = if cfg!(windows) {
        r"C:\Windows\System32\true.cmd"
    } else {
        "/bin/true"
    };
    body["lifecycle"] = json!({"started": {"path": hook}});
    write_definition(&dir, "odd", body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("odd").is_none());
    assert_eq!(loaded.issues.len(), 1, "{:?}", loaded.issues);
    let reason = &loaded.issues[0].reason;
    assert!(reason.contains("started"), "{reason}");
}

#[test]
fn keeps_notify_services_and_exec_one_shots_loadable() {
    // The rejection above must not narrow the two legitimate shapes: a
    // notify-ready long-running service, and an exec one-shot whose work
    // outlives its process.
    let dir = TempDir::new();
    let mut notify = definition("notify");
    notify["exec"]["ready"] = json!("notify");
    write_definition(&dir, "notify", notify);
    let mut one_shot = definition("one-shot");
    one_shot["remain_after_exit"] = json!(true);
    write_definition(&dir, "one-shot", one_shot);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let notify = loaded.registry.get("notify").expect("notify");
    assert_eq!(notify.exec.as_ref().expect("exec").ready, Ready::Notify);
    assert!(!notify.remain_after_exit);
    assert!(
        loaded
            .registry
            .get("one-shot")
            .expect("one-shot")
            .remain_after_exit
    );
}

#[test]
fn loads_a_drop_in_script_as_a_core_service() {
    let dir = TempDir::new();
    write_script(&dir, &script_name("demo"), all_actions_body());

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let demo = loaded.registry.get("demo").expect("demo");
    assert_eq!(demo.kind, Kind::Core);
    assert!(demo.contract.surface().is_empty());
    assert_eq!(demo.version, "1.0.0");
    assert!(demo.enabled);
    let exec = demo.exec.as_ref().expect("exec");
    assert_eq!(exec.ready, Ready::Exec);
    assert_eq!(exec.args, ["start"]);
    let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
    // The declared start action is the process itself, so it lives in
    // `exec.args`; the lifecycle only carries the actions that run around it.
    assert!(lifecycle.start.is_none());
    assert_eq!(lifecycle.stop.as_ref().expect("stop").args, ["stop"]);
    assert_eq!(lifecycle.status.as_ref().expect("status").args, ["status"]);
    assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
    assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
}

#[test]
fn a_script_without_actions_runs_from_the_top() {
    let dir = TempDir::new();
    write_script(&dir, &script_name("demo"), "#!/bin/sh\r\n");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let demo = loaded.registry.get("demo").expect("demo");
    let exec = demo.exec.as_ref().expect("exec");
    assert!(exec.args.is_empty(), "exec args: {:?}", exec.args);
    assert!(demo.lifecycle.is_none());
}

#[test]
fn only_declared_actions_become_lifecycle_hooks() {
    let dir = TempDir::new();
    write_script(&dir, &script_name("demo"), start_only_body());

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    let demo = loaded.registry.get("demo").expect("demo");
    assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
    // A start-only script has no actions around the process: it *is* the
    // process, so there is no lifecycle left.
    assert!(demo.lifecycle.is_none());
}

#[test]
fn actions_in_comments_are_not_declared() {
    let dir = TempDir::new();
    #[cfg(windows)]
    let body = "@echo off\r\nrem :start\r\n:: :stop\r\n";
    #[cfg(not(windows))]
    let body = "#!/bin/sh\n# start() { :; }\n# stop() { :; }\n# status() { :; }\n";
    write_script(&dir, &script_name("demo"), body);

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    let demo = loaded.registry.get("demo").expect("demo");
    assert!(demo.exec.as_ref().expect("exec").args.is_empty());
    assert!(demo.lifecycle.is_none(), "{:?}", demo.lifecycle);
}

#[cfg(unix)]
#[test]
fn a_case_branch_declares_a_shell_action() {
    let dir = TempDir::new();
    write_script(
        &dir,
        &script_name("demo"),
        "#!/bin/sh\ncase \"$1\" in\n  start)\n    :;;\n  status)\n    :;;\nesac\n",
    );

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    let demo = loaded.registry.get("demo").expect("demo");
    assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
    let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
    // The declared start action is the process (#323), so only status remains.
    assert!(lifecycle.start.is_none());
    assert!(lifecycle.status.is_some());
    assert!(lifecycle.stop.is_none());
}

#[test]
fn a_disabled_marker_turns_off_a_script() {
    let dir = TempDir::new();
    write_script(&dir, &script_name("demo"), "");
    disable_definition(&dir, "demo");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(!loaded.registry.get("demo").expect("demo").enabled);
}

#[test]
fn a_json_definition_wins_over_a_script_with_the_same_id() {
    let dir = TempDir::new();
    write_definition(&dir, "demo", definition("demo"));
    write_script(&dir, &script_name("demo"), "");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    let demo = loaded.registry.get("demo").expect("demo");
    assert!(demo.lifecycle.is_none());
    assert!(
        loaded
            .issues
            .iter()
            .any(|issue| issue.reason.contains("already defined")),
        "issues: {:?}",
        loaded.issues
    );
}

#[test]
fn rejects_invalid_and_reserved_script_ids() {
    let dir = TempDir::new();
    write_script(&dir, &script_name("bad.name"), "");
    write_script(&dir, &script_name("svc"), "");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert_eq!(loaded.issues.len(), 2, "issues: {:?}", loaded.issues);
    assert!(loaded
        .issues
        .iter()
        .any(|issue| issue.reason.contains("invalid")));
    assert!(loaded
        .issues
        .iter()
        .any(|issue| issue.reason.contains("reserved")));
}

#[cfg(windows)]
#[test]
fn loads_a_powershell_script() {
    let dir = TempDir::new();
    write_script(
        &dir,
        "demo.ps1",
        "function Start { }\r\nfunction Stop { }\r\n",
    );

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.issues.is_empty(), "issues: {:?}", loaded.issues);
    let demo = loaded.registry.get("demo").expect("demo");
    assert_eq!(demo.exec.as_ref().expect("exec").args, ["start"]);
    let lifecycle = demo.lifecycle.as_ref().expect("lifecycle");
    assert!(lifecycle.start.is_none());
    assert!(lifecycle.stop.is_some());
    assert!(lifecycle.status.is_none());
}

#[cfg(unix)]
#[test]
fn rejects_a_non_executable_script() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new();
    let name = script_name("demo");
    write_script(&dir, &name, "");
    let path = services_dir(&dir).join(&name);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_none());
    assert!(loaded.issues[0].reason.contains("executable"));
}

#[cfg(unix)]
#[test]
fn rejects_a_symlinked_script() {
    let dir = TempDir::new();
    let target = dir.0.join("target.sh");
    fs::write(&target, "").expect("target");
    fs::create_dir_all(services_dir(&dir)).expect("services.d");
    std::os::unix::fs::symlink(&target, services_dir(&dir).join("demo.sh")).expect("symlink");

    let loaded = load_services(
        &services_dir(&dir),
        &crate::platform::limits::test_default(),
    );

    assert!(loaded.registry.get("demo").is_none());
    assert!(loaded.issues[0].reason.contains("symlink"));
}
