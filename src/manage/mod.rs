//! L3 领域·管理：`.spk` 的布局、校验与安装 / 导入 / 导出。
//!
//! `model/` / `check/` / `ops/` 按 §5 落位：类型与解析在 model，纯判定在
//! check，写目标树在 ops。测试跟着被测模块走，共用构造器只在
//! `test_support.rs` 里有一份。

pub mod check;
pub mod model;
pub mod ops;
#[cfg(test)]
pub(crate) mod test_support;

pub use check::backup::{parse_backup, BackupRules};
pub use check::plugin::*;
pub use check::verify::*;
#[cfg(feature = "http")]
pub use model::backup::*;
pub(crate) use model::hex::hex_sha256;
pub use model::layout::*;
pub use model::manifest::*;
pub use model::ustar::*;
#[cfg(feature = "http")]
pub(crate) use ops::config::config_lock;
pub use ops::config::*;
pub use ops::export::*;
pub use ops::export_plugin::*;
pub use ops::install::*;
pub use ops::install_plugin::*;
pub use ops::scan::*;
pub use ops::uninstall::*;
