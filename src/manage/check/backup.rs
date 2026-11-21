//! `.spkb` 备份导入的纯规则：schema、身份、config 哈希与版本策略。
//!
//! 缓冲路径（`ops::transfer::import_backup`）与流式路径
//! （`face::install::backup::BackupSink`）都只调用这里；两条路径不再各写一份
//! 判定。规则不读 tar、不写文件、不安装，也不碰 `require_root`——权限前提
//! 属于调用方（CLI 与 HTTP 不同）。

use crate::kernel::failure::{Problem, ProblemError};
use crate::manage::model::backup::Backup;
use crate::manage::model::hex::hex_sha256;

/// 一次 `.spkb` 导入在开始安装前已经具备的事实。
pub struct BackupRules<'a> {
    pub backup: &'a Backup,
    pub package_id: &'a str,
    pub package_version: &'a str,
    pub config: Option<&'a [u8]>,
    pub current_version: Option<&'a str>,
    pub force: bool,
}

/// 规则允许导入时带出的结论：warnings 按产生顺序排列。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackupPlan {
    pub warnings: Vec<String>,
}

impl BackupRules<'_> {
    /// 判定一次 `.spkb` 导入。错误类别、文案与 warnings 顺序沿用两条路径
    /// 各自原来的输出，统一后它们对同一输入给出同一个结果。
    pub fn plan(&self) -> Result<BackupPlan, ProblemError> {
        if self.backup.id != self.package_id {
            return Err(Problem::Invalid.error(format!(
                "backup id {} does not match package id {}",
                self.backup.id, self.package_id
            )));
        }
        if self.backup.version != self.package_version {
            return Err(Problem::Invalid.error(format!(
                "backup version {} does not match package version {}",
                self.backup.version, self.package_version
            )));
        }
        if let Some(expected) = &self.backup.config_sha256 {
            let Some(config) = self.config else {
                return Err(
                    Problem::Invalid.error("backup declares a config hash but config is missing")
                );
            };
            if &hex_sha256(config) != expected {
                return Err(Problem::Invalid.error("backup config hash does not match"));
            }
        }

        let mut warnings = Vec::new();
        if let Some(current) = self.current_version {
            if current != self.backup.version {
                warnings.push(format!(
                    "installed {} is {}, importing {}",
                    self.backup.id, current, self.backup.version
                ));
            } else if !self.force {
                return Err(Problem::Conflict.error(format!(
                    "{} {} is already installed; use --force",
                    self.backup.id, self.backup.version
                )));
            }
        }
        Ok(BackupPlan { warnings })
    }
}

/// 解析 backup.json 并拒绝未知 schema。
pub fn parse_backup(bytes: &[u8]) -> Result<Backup, ProblemError> {
    let backup: Backup = serde_json::from_slice(bytes)
        .map_err(|error| Problem::Invalid.error(format!("invalid backup.json: {}", error)))?;
    if backup.schema != 1 {
        return Err(Problem::Invalid.error(format!("unsupported backup schema {}", backup.schema)));
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> Backup {
        Backup {
            schema: 1,
            id: "demo".to_string(),
            version: "1.0.0".to_string(),
            config_sha256: None,
        }
    }

    fn rules<'a>(
        backup: &'a Backup,
        config: Option<&'a [u8]>,
        current_version: Option<&'a str>,
        force: bool,
    ) -> BackupRules<'a> {
        BackupRules {
            backup,
            package_id: &backup.id,
            package_version: &backup.version,
            config,
            current_version,
            force,
        }
    }

    #[test]
    fn invalid_backup_json_is_invalid() {
        let error = parse_backup(b"{").expect_err("invalid json");
        assert_eq!(error.problem, Problem::Invalid);
        assert!(
            error.detail().starts_with("invalid backup.json: "),
            "{}",
            error
        );
    }

    #[test]
    fn an_unknown_schema_is_invalid() {
        let error = parse_backup(br#"{"schema":2,"id":"demo","version":"1.0.0"}"#)
            .expect_err("unknown schema");
        assert_eq!(error.problem, Problem::Invalid);
        assert_eq!(error.detail(), "unsupported backup schema 2");
    }

    #[test]
    fn a_mismatched_id_is_invalid() {
        let backup = metadata();
        let mut rules = rules(&backup, None, None, false);
        rules.package_id = "other";
        let error = rules.plan().expect_err("id mismatch");
        assert_eq!(error.problem, Problem::Invalid);
        assert_eq!(
            error.detail(),
            "backup id demo does not match package id other"
        );
    }

    #[test]
    fn a_mismatched_version_is_invalid() {
        let backup = metadata();
        let mut rules = rules(&backup, None, None, false);
        rules.package_version = "2.0.0";
        let error = rules.plan().expect_err("version mismatch");
        assert_eq!(error.problem, Problem::Invalid);
        assert_eq!(
            error.detail(),
            "backup version 1.0.0 does not match package version 2.0.0"
        );
    }

    #[test]
    fn a_declared_hash_without_config_is_invalid() {
        let mut backup = metadata();
        backup.config_sha256 = Some(hex_sha256(b"one"));
        let error = rules(&backup, None, None, false)
            .plan()
            .expect_err("missing config");
        assert_eq!(error.problem, Problem::Invalid);
        assert_eq!(
            error.detail(),
            "backup declares a config hash but config is missing"
        );
    }

    #[test]
    fn a_mismatched_config_hash_is_invalid() {
        let mut backup = metadata();
        backup.config_sha256 = Some(hex_sha256(b"one"));
        let error = rules(&backup, Some(b"two"), None, false)
            .plan()
            .expect_err("hash mismatch");
        assert_eq!(error.problem, Problem::Invalid);
        assert_eq!(error.detail(), "backup config hash does not match");
    }

    #[test]
    fn a_matching_config_hash_is_accepted() {
        let mut backup = metadata();
        backup.config_sha256 = Some(hex_sha256(b"one"));
        let plan = rules(&backup, Some(b"one"), None, false)
            .plan()
            .expect("matching config");
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn the_same_version_without_force_is_a_conflict() {
        let backup = metadata();
        let error = rules(&backup, None, Some("1.0.0"), false)
            .plan()
            .expect_err("conflict");
        assert_eq!(error.problem, Problem::Conflict);
        assert_eq!(
            error.detail(),
            "demo 1.0.0 is already installed; use --force"
        );
    }

    #[test]
    fn the_same_version_with_force_is_accepted_without_a_warning() {
        let backup = metadata();
        let plan = rules(&backup, None, Some("1.0.0"), true)
            .plan()
            .expect("forced");
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn another_installed_version_is_a_warning() {
        let backup = metadata();
        let plan = rules(&backup, None, Some("0.9.0"), false)
            .plan()
            .expect("other version");
        assert_eq!(
            plan.warnings,
            vec!["installed demo is 0.9.0, importing 1.0.0".to_string()]
        );
    }

    #[test]
    fn no_installed_version_means_no_warning() {
        let backup = metadata();
        let plan = rules(&backup, None, None, false)
            .plan()
            .expect("first import");
        assert!(plan.warnings.is_empty());
    }
}
