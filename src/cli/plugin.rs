//! The `plugin` entry: plugin-only views over the same daemon channel.
//!
//! `svc` stays the generic service CLI; `plugin` refuses core ids for
//! status/start/stop and adds install/uninstall.

mod commands;
mod parse;
mod render;

pub use commands::run;
#[cfg(test)]
use parse::{parse, Command};

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli::client::{self, EXIT_OK, EXIT_SERVICE_ERROR, EXIT_USAGE};
use crate::cli::status;
use crate::kernel::projection::Status;
use crate::kernel::registry::Kind;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::manage as package;
use crate::manage as plugin;
use crate::manage::ops::transfer;
use crate::platform::compat;
use crate::platform::config::Config;
use crate::wire::control::channel;

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_bundle_compat_and_platform() {
        assert_eq!(
            parse(&args(&[
                "bundle",
                "src",
                "--key",
                "key.pem",
                "--compat",
                "sr6110,sr1010",
                "--platform=aarch64-unknown-linux-musl"
            ]))
            .expect("bundle"),
            Command::Bundle(crate::manage::ops::bundle::Options {
                dir: PathBuf::from("src"),
                key: Some(PathBuf::from("key.pem")),
                compat: vec!["sr6110".into(), "sr1010".into()],
                platform: Some("aarch64-unknown-linux-musl".into()),
                ..Default::default()
            })
        );
        assert!(parse(&args(&["bundle", "src", "--compat"])).is_err());
        assert!(parse(&args(&["bundle", "src", "--platform"])).is_err());
        assert_eq!(
            parse(&args(&["bundle", "src", "--unsigned"])).expect("unsigned bundle"),
            Command::Bundle(crate::manage::ops::bundle::Options {
                dir: PathBuf::from("src"),
                unsigned: true,
                ..Default::default()
            })
        );
    }

    #[test]
    fn parses_plugin_commands() {
        assert_eq!(parse(&args(&["list"])).expect("list"), Command::List);
        assert_eq!(
            parse(&args(&["status", "demo", "--json"])).expect("status"),
            Command::Status {
                id: "demo".into(),
                json: true
            }
        );
        assert_eq!(
            parse(&args(&["install", "demo.spk", "--strategy", "replace"])).expect("install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: false,
                root: None
            }
        );
        assert_eq!(
            parse(&args(&["enable", "demo"])).expect("enable"),
            Command::Enable { id: "demo".into() }
        );
        assert_eq!(
            parse(&args(&["disable", "demo"])).expect("disable"),
            Command::Disable { id: "demo".into() }
        );
        assert_eq!(
            parse(&args(&["restart", "demo"])).expect("restart"),
            Command::Restart { id: "demo".into() }
        );
        assert!(parse(&args(&["enable"])).is_err());
        assert!(parse(&args(&["restart"])).is_err());
        assert!(parse(&args(&["disable", "one", "two"])).is_err());
        assert_eq!(
            parse(&args(&["install", "demo.spk"])).expect("default install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: false,
                root: None
            }
        );
        assert_eq!(
            parse(&args(&["install", "demo.spk", "--allow-hooks"])).expect("hooks install"),
            Command::Install {
                path: PathBuf::from("demo.spk"),
                strategy: "replace".into(),
                allow_hooks: true,
                root: None
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk"])).expect("verify defaults"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: false,
                pubkey: None
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk", "--json"])).expect("verify json"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: true,
                pubkey: None
            }
        );
        assert_eq!(
            parse(&args(&["verify", "demo.spk", "--pubkey", "release.pub"]))
                .expect("verify explicit key"),
            Command::Verify {
                path: PathBuf::from("demo.spk"),
                json: false,
                pubkey: Some(PathBuf::from("release.pub"))
            }
        );
        assert!(parse(&args(&["verify", "demo.spk", "--allow-hooks"])).is_err());
        assert!(parse(&args(&["verify"])).is_err());
        assert!(parse(&args(&["verify", "one.spk", "two.spk"])).is_err());
        assert_eq!(
            parse(&args(&["uninstall", "demo", "--purge"])).expect("uninstall"),
            Command::Uninstall {
                id: "demo".into(),
                purge: true
            }
        );
        assert!(parse(&args(&["install", "demo.spk", "--strategy", "nope"])).is_err());
        assert!(parse(&args(&["status"])).is_err());
        assert!(parse(&args(&["list", "extra"])).is_err());
        assert_eq!(
            parse(&args(&["editconfig", "demo", "--print"])).expect("editconfig"),
            Command::EditConfig {
                id: "demo".into(),
                print: true,
                file: None,
                reset: false
            }
        );
        assert_eq!(
            parse(&args(&["export", "demo", "--config-only", "--out", "x"])).expect("export"),
            Command::Export {
                id: "demo".into(),
                config_only: true,
                out: Some(PathBuf::from("x"))
            }
        );
        assert_eq!(
            parse(&args(&[
                "import",
                "x.spkb",
                "--config",
                "demo",
                "--no-config",
                "--force"
            ]))
            .expect("import"),
            Command::Import {
                path: PathBuf::from("x.spkb"),
                config_id: Some("demo".into()),
                no_config: true,
                force: true
            }
        );
        assert!(parse(&args(&["editconfig", "demo", "--print", "--reset"])).is_err());
        assert_eq!(
            parse(&args(&[
                "bundle",
                "samples/demo",
                "--key",
                "key.pem",
                "-o",
                "out.spk",
                "--id",
                "demo",
                "--name",
                "示例插件",
                "--version",
                "1.2.3",
                "--exec",
                "payload/bin/demo",
                "--desc",
                "demo plugin",
                "--key-id",
                "test"
            ]))
            .expect("bundle"),
            Command::Bundle(crate::manage::ops::bundle::Options {
                dir: PathBuf::from("samples/demo"),
                out: Some(PathBuf::from("out.spk")),
                key: Some(PathBuf::from("key.pem")),
                key_id: Some("test".into()),
                id: Some("demo".into()),
                name: Some("示例插件".into()),
                version: Some("1.2.3".into()),
                exec: Some("payload/bin/demo".into()),
                desc: Some("demo plugin".into()),
                from_core: false,
                interpreter: None,
                interpreter_version: None,
                interpreter_args: Vec::new(),
                compat: Vec::new(),
                platform: None,
                unsigned: false
            })
        );
        assert_eq!(
            parse(&args(&[
                "bundle",
                "samples/demo",
                "--key",
                "key.pem",
                "--from-core",
                "--interpreter",
                "sh",
                "--interpreter-version",
                ">=5",
                "--interpreter-arg=-e"
            ]))
            .expect("script bundle"),
            Command::Bundle(crate::manage::ops::bundle::Options {
                dir: PathBuf::from("samples/demo"),
                out: None,
                key: Some(PathBuf::from("key.pem")),
                key_id: None,
                id: None,
                name: None,
                version: None,
                exec: None,
                desc: None,
                from_core: true,
                interpreter: Some("sh".into()),
                interpreter_version: Some(">=5".into()),
                interpreter_args: vec!["-e".into()],
                compat: Vec::new(),
                platform: None,
                unsigned: false
            })
        );
        assert!(parse(&args(&["bundle", "a", "b"])).is_err());
    }
}
