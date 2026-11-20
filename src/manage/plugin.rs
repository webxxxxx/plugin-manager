//! Plugin installation layout and the `plugin` scan used by the daemon.
//!
//! The package is verified by [`crate::manage::package`] before this module touches the
//! plugin tree. Versions live under `<data>/plugins/<id>/<version>/`; `current`
//! is a version text file and `enabled` is a marker.

#[cfg(test)]
use std::path::{Path, PathBuf};

#[cfg(test)]
use crate::kernel::contract::Effect;
#[cfg(test)]
use crate::kernel::registry::Interpreter;
#[cfg(test)]
use crate::kernel::registry::Ready;
#[cfg(test)]
use crate::manage::model::manifest::Package;
#[cfg(test)]
use crate::manage::package;
#[cfg(test)]
use crate::platform::paths::Paths;

#[cfg(test)]
use crate::kernel::failure::Problem;
pub use crate::manage::check::plugin::*;
pub use crate::manage::model::layout::*;
pub use crate::manage::ops::config::*;
pub use crate::manage::ops::install_plugin::*;
pub use crate::manage::ops::scan::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();

    fn root() -> &'static Path {
        let path = ROOT.get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            let path = crate::test_support::temp_dir().join(format!(
                "svcd-config-unit-{}-{}",
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(path.join("data/plugins")).expect("data");
            path
        });
        path
    }

    /// The directory set for the shared plugin tree: no process environment.
    fn paths() -> &'static Paths {
        static PATHS: OnceLock<Paths> = OnceLock::new();
        PATHS.get_or_init(|| Paths::under(root()))
    }

    fn make_plugin(id: &str, config: &str, check: bool) {
        let directory = root().join("data/plugins").join(id);
        let version_dir = directory.join("1.0.0");
        std::fs::create_dir_all(version_dir.join("config")).expect("version");
        std::fs::write(directory.join("current"), "1.0.0").expect("current");
        std::fs::write(version_dir.join("config/config"), config).expect("template");
        write_config(&directory.join("config"), config.as_bytes()).expect("active");
        let mut manifest = serde_json::json!({
            "schema": 1,
            "name": id,
            "version": "1.0.0",
            "desc": "config test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "config/config"},
            "files": []
        });
        if check {
            let path = if cfg!(windows) {
                "check/check.cmd"
            } else {
                "check/check.sh"
            };
            let script = if cfg!(windows) {
                "@echo off\r\nfindstr /C:\"good\" \"%SVCD_CONFIG%\" >nul\r\nexit /b %errorlevel%\r\n"
            } else {
                "#!/bin/sh\ngrep -q good \"$SVCD_CONFIG\"\n"
            };
            std::fs::create_dir_all(version_dir.join("check")).expect("check dir");
            std::fs::write(version_dir.join(path), script).expect("check script");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    version_dir.join(path),
                    std::fs::Permissions::from_mode(0o755),
                )
                .expect("chmod check");
            }
            manifest["configCheck"] = serde_json::json!({"path": path});
        }
        std::fs::write(
            version_dir.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");
    }

    #[test]
    fn scan_builds_the_contract_from_manifest_capabilities() {
        let id = "contract-caps";
        make_plugin(id, "", false);
        let manifest_path = root()
            .join("data/plugins")
            .join(id)
            .join("1.0.0")
            .join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("manifest"))
                .expect("manifest json");
        manifest["capabilities"] = serde_json::json!([{
            "name": "run",
            "desc": "run it",
            "inputSchema": {"type": "object"}
        }]);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("manifest"),
        )
        .expect("write manifest");

        let (services, _) = scan(paths(), &crate::platform::limits::test_default());
        let service = services
            .iter()
            .find(|service| service.id == id)
            .expect("plugin is scanned");
        let surface = service.contract.surface();
        assert_eq!(surface.len(), 1);
        assert_eq!(surface[0].name.as_str(), "run");
        assert_eq!(surface[0].desc, "run it");
        assert_eq!(surface[0].effect, Effect::Write);
        assert!(!surface[0].idempotent);
        assert!(!service
            .contract
            .failures
            .contains(&crate::kernel::failure::Class::Cancelled));
    }

    #[test]
    fn hooks_are_allowed_by_default_only_with_the_http_feature() {
        assert!(hooks_allowed(true));
        assert_eq!(hooks_allowed(false), cfg!(feature = "http"));
    }

    #[test]
    fn replace_plan_decides_from_free_old_and_new_sizes() {
        // Fresh installs need the whole package to fit.
        assert_eq!(replace_plan(100, None, 100), ReplacePlan::Fresh);
        assert_eq!(replace_plan(99, None, 100), ReplacePlan::NoSpace);
        // A backup is only safe while the new copy also fits next to the old one.
        assert_eq!(replace_plan(100, Some(80), 100), ReplacePlan::Backup);
        assert_eq!(replace_plan(150, Some(1), 100), ReplacePlan::Backup);
        // Reclaiming the old version is the fallback when both copies cannot coexist.
        assert_eq!(replace_plan(99, Some(80), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(20, Some(80), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(19, Some(80), 100), ReplacePlan::NoSpace);
        assert_eq!(replace_plan(0, Some(100), 100), ReplacePlan::Reclaim);
        assert_eq!(replace_plan(0, Some(98), 100), ReplacePlan::NoSpace);
    }

    #[test]
    fn replace_config_writes_last_good_and_checks_candidates() {
        make_plugin("plain", "old", false);
        let active = active_config(paths(), "plain").expect("active");
        assert_eq!(std::fs::read(&active).expect("read"), b"old");
        replace_config(paths(), "plain", b"new", false).expect("replace");
        assert_eq!(std::fs::read(&active).expect("read"), b"new");
        assert_eq!(
            std::fs::read(plugin_dir(paths(), "plain").join("config.last-good"))
                .expect("last-good"),
            b"old"
        );
    }

    #[test]
    fn config_check_rejects_a_bad_candidate_and_keeps_the_active_one() {
        make_plugin("checked", "old", true);
        replace_config(paths(), "checked", b"good config", true).expect("good");
        let error = replace_config(paths(), "checked", b"bad config", true).expect_err("bad");
        assert!(error.detail().contains("configCheck"), "{error}");
        assert_eq!(error.problem, Problem::Conflict);
        assert_eq!(
            std::fs::read(active_config(paths(), "checked").expect("active")).expect("read"),
            b"good config"
        );
    }

    /// The daemon's keep-old fallback reads this signal, never the detail text.
    #[test]
    fn an_install_failure_keeps_its_retry_signal_and_its_category() {
        let refused: InstallFailure = Problem::Conflict.error("reserved").into();
        assert!(matches!(refused, InstallFailure::Refused(_)));
        assert_eq!(refused.into_problem().problem, Problem::Conflict);

        let full = InstallFailure::OutOfSpace(Problem::Internal.error("not enough space"));
        assert!(matches!(full, InstallFailure::OutOfSpace(_)));
        assert_eq!(full.into_problem().problem, Problem::Internal);
    }

    #[test]
    fn a_config_refusal_carries_its_status_family() {
        make_plugin("families", "old", false);

        let error =
            replace_config(paths(), "no-such-plugin", b"x", false).expect_err("unknown plugin");
        assert_eq!(error.problem, Problem::UnknownPlugin);

        let oversized = vec![b'x'; 1024 * 1024 + 1];
        let error = replace_config(paths(), "families", &oversized, false).expect_err("too large");
        assert_eq!(error.problem, Problem::TooLarge);
    }

    fn script_manifest(
        id: &str,
        exec: &str,
        data: &[u8],
        interpreter: Option<serde_json::Value>,
        lifecycle: Option<serde_json::Value>,
    ) -> (std::path::PathBuf, package::Manifest) {
        let version_dir = root().join("data/plugins").join(id).join("1.0.0");
        let exec_path = version_dir.join(exec);
        std::fs::create_dir_all(exec_path.parent().expect("parent")).expect("script dir");
        std::fs::write(&exec_path, data).expect("script");
        let mut value = serde_json::json!({
            "schema": 2,
            "name": id,
            "version": "1.0.0",
            "desc": "script test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": exec, "ready": "exec"},
            "files": []
        });
        if let Some(interpreter) = interpreter {
            value["interpreter"] = interpreter;
        }
        if let Some(lifecycle) = lifecycle {
            value["lifecycle"] = lifecycle;
        }
        let bytes = serde_json::to_vec(&value).expect("manifest");
        let manifest = package::parse_manifest(&bytes).expect("parse");
        (version_dir, manifest)
    }

    #[test]
    fn maps_a_schema_two_script_lifecycle_and_interpreter() {
        let host = Interpreter {
            path: "/bin/sh".into(),
            args: Vec::new(),
        };
        let (version_dir, manifest) = script_manifest(
            "scripted",
            "payload/actions.sh",
            b"#!/bin/sh\n",
            Some(serde_json::json!({"name": "sh"})),
            Some(serde_json::json!({
                "start": {"path": "payload/actions.sh", "args": ["start"]},
                "stop": {"path": "payload/actions.sh", "args": ["stop"]},
                "status": {"path": "payload/actions.sh", "args": ["status"]}
            })),
        );

        let service = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "scripted",
            &version_dir,
            &manifest,
            true,
            Some(&host),
        )
        .expect("service");

        let exec = service.exec.as_ref().expect("exec");
        assert_eq!(exec.ready, Ready::Exec);
        assert_eq!(
            exec.interpreter.as_ref().map(|i| i.path.as_str()),
            Some("/bin/sh")
        );
        let lifecycle = service.lifecycle.as_ref().expect("lifecycle");
        let start = lifecycle.start.as_ref().expect("start");
        assert_eq!(start.args, ["start".to_string()]);
        assert_eq!(start.timeout_ms, 30_000);
        assert_eq!(
            start.interpreter.as_ref().map(|i| i.path.as_str()),
            Some("/bin/sh")
        );
        assert!(std::path::Path::new(&start.path).is_absolute());
        assert_eq!(
            std::path::Path::new(&start.path)
                .file_name()
                .and_then(|name| name.to_str()),
            Some("actions.sh")
        );
        assert_eq!(lifecycle.stop.as_ref().expect("stop").timeout_ms, 10_000);
        assert_eq!(lifecycle.status.as_ref().expect("status").timeout_ms, 5_000);
    }

    #[test]
    fn a_schema_two_script_requires_an_interpreter() {
        let (version_dir, manifest) = script_manifest(
            "no-interpreter",
            "payload/actions.sh",
            b"#!/bin/sh\n",
            None,
            None,
        );

        let error = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "no-interpreter",
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect_err("missing interpreter");

        assert!(
            error.detail().contains("declares no interpreter"),
            "{error}"
        );
    }

    #[test]
    fn a_native_schema_two_payload_runs_directly() {
        let (version_dir, manifest) =
            script_manifest("native", "payload/bin/tool", b"\x7fELFdemo", None, None);

        let service = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "native",
            &version_dir,
            &manifest,
            true,
            None,
        )
        .expect("service");

        assert!(service.exec.as_ref().expect("exec").interpreter.is_none());
        assert!(service.lifecycle.is_none());
    }

    /// An in-memory package with one payload file, enough for the layout
    /// strategies to run without a daemon.
    fn layout_package(name: &str, version: &str) -> Package {
        let manifest_json = serde_json::json!({
            "schema": 1,
            "name": name,
            "version": version,
            "desc": "layout test",
            "platform": crate::platform::target(),
            "protocol": 1,
            "exec": {"path": "payload/bin/demo"},
            "files": [{
                "path": "payload/bin/demo",
                "size": 5,
                "sha256": package::hex_sha256(b"hello"),
                "mode": 493
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest_json).expect("manifest json");
        let manifest = package::parse_manifest(&manifest_bytes).expect("manifest");
        Package {
            manifest,
            manifest_bytes,
            signature_bytes: Vec::new(),
            files: vec![package::FileData {
                path: "payload/bin/demo".into(),
                data: b"hello".to_vec(),
                mode: 0o755,
            }],
        }
    }

    #[test]
    fn keep_old_keeps_both_versions_and_replace_drops_the_old_one() {
        let root = crate::test_support::temp_dir().join(format!(
            "svcd-plugin-layout-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let paths = Paths::under(&root);

        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.0.0"),
            Strategy::KeepOld,
            false,
            None,
            &mut || {},
        )
        .expect("first install");
        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.1.0"),
            Strategy::KeepOld,
            false,
            None,
            &mut || {},
        )
        .expect("keep-old install");

        let directory = plugin_dir(&paths, "layout-demo");
        assert!(directory.join("1.0.0").is_dir());
        assert!(directory.join("1.1.0").is_dir());
        assert_eq!(
            current_version(&paths, "layout-demo").as_deref(),
            Some("1.1.0")
        );
        assert!(directory.join("current").is_file());

        install_package(
            &paths,
            &crate::platform::limits::test_default(),
            &layout_package("layout-demo", "1.1.0"),
            Strategy::Replace,
            false,
            None,
            &mut || {},
        )
        .expect("replace install");
        assert!(
            !directory.join("1.0.0").exists(),
            "replace keeps only the new version"
        );
        assert!(directory.join("1.1.0").is_dir());
        assert_eq!(
            current_version(&paths, "layout-demo").as_deref(),
            Some("1.1.0")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dangling_interpreter_on_a_native_payload_is_rejected() {
        let host = Interpreter {
            path: "/bin/sh".into(),
            args: Vec::new(),
        };
        let (version_dir, manifest) = script_manifest(
            "dangling",
            "payload/bin/tool",
            b"\x7fELFdemo",
            Some(serde_json::json!({"name": "sh"})),
            None,
        );

        let error = service_from_manifest(
            paths(),
            &crate::platform::limits::test_default(),
            "dangling",
            &version_dir,
            &manifest,
            true,
            Some(&host),
        )
        .expect_err("no script payload");

        assert!(error.detail().contains("no script payload"), "{error}");
    }
}
