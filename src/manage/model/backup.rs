//! `.spkb` 的 backup.json 元数据。
//!
//! 两条导入路径（缓冲的 `ops::transfer::import_backup` 与流式的
//! `face::install::backup::BackupSink`）与导出共用这一个类型。

#[derive(Debug, serde::Deserialize)]
pub struct Backup {
    pub schema: u32,
    pub id: String,
    pub version: String,
    #[serde(rename = "configSha256")]
    pub config_sha256: Option<String>,
}
