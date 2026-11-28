#![cfg(test)]
//! 子进程的 spawn 面：解释器包装、子进程环境、插件文件路径。

use super::*;

#[test]
fn an_interpreter_wraps_the_script_and_its_arguments() {
    let exec = Exec {
        path: "clock.sh".into(),
        args: vec!["start".into()],
        env: BTreeMap::new(),
        ready: Ready::Exec,
        interpreter: Some(Interpreter {
            path: "/usr/bin/sh".into(),
            args: vec!["-e".into()],
        }),
    };

    let command = command_for(&exec);
    let args: Vec<String> = command
        .get_args()
        .map(|argument| argument.to_string_lossy().to_string())
        .collect();

    assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/sh");
    assert_eq!(args, vec!["-e", "clock.sh", "start"]);
}

#[test]
fn a_hook_with_an_interpreter_keeps_the_action_environment() {
    let hook = Hook {
        path: "actions.sh".into(),
        args: vec!["stop".into()],
        env: BTreeMap::new(),
        timeout_ms: 1_000,
        interpreter: Some(Interpreter {
            path: "/usr/bin/bash".into(),
            args: Vec::new(),
        }),
    };

    let command = hook_command("demo", "stop", &hook, &test_paths(), None);
    let args: Vec<String> = command
        .get_args()
        .map(|argument| argument.to_string_lossy().to_string())
        .collect();

    assert_eq!(command.get_program().to_string_lossy(), "/usr/bin/bash");
    assert_eq!(args, vec!["actions.sh", "stop"]);
    assert!(command.get_envs().any(|(key, value)| {
        key.to_string_lossy() == "SVCD_ACTION"
            && value
                .map(|value| value.to_string_lossy() == "stop")
                .unwrap_or(false)
    }));
    // A core service is the operator's own code: the plugin-only directory is
    // not invented for it.
    assert!(!command
        .get_envs()
        .any(|(key, _)| key.to_string_lossy() == "SVCD_PLUGIN_DIR"));
}

/// #312: a plugin is third-party code. It learns where to put its own files and
/// nothing about the daemon's config (password hash, `trust.d`) or data tree.
#[test]
fn a_plugin_child_learns_only_where_to_put_its_files() {
    let hook = Hook {
        path: "actions.sh".into(),
        args: vec!["stop".into()],
        env: BTreeMap::new(),
        timeout_ms: 1_000,
        interpreter: None,
    };
    let paths = test_paths();
    let plugin_dir = std::path::Path::new("/mnt/sd/plugins/demo");

    let command = hook_command("demo", "stop", &hook, &paths, Some(plugin_dir));
    let env: BTreeMap<String, String> = command
        .get_envs()
        .filter_map(|(key, value)| {
            value.map(|value| {
                (
                    key.to_string_lossy().to_string(),
                    value.to_string_lossy().to_string(),
                )
            })
        })
        .collect();
    // The daemon's own environment carries these; a plugin child must not
    // inherit them, so they are removed rather than merely left unset.
    let mut removed: Vec<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(key, _)| key.to_string_lossy().to_string())
        .collect();
    removed.sort();
    assert_eq!(
        removed,
        vec![
            "SVCD_ACTION",
            "SVCD_CONFIG_DIR",
            "SVCD_DATA_DIR",
            "SVCD_SERVICE_ID"
        ]
    );

    let keys: Vec<&str> = env.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec!["SVCD_LOG_DIR", "SVCD_PLUGIN_DIR", "SVCD_TEMP_DIR"],
        "a plugin gets exactly the directories it places files in"
    );
    assert_eq!(
        env["SVCD_PLUGIN_DIR"],
        plugin_dir.to_string_lossy(),
        "the directory is the one the package was installed into"
    );
    assert_eq!(env["SVCD_LOG_DIR"], paths.logs_dir.to_string_lossy());
    assert_eq!(env["SVCD_TEMP_DIR"], paths.temp_dir.to_string_lossy());
}
