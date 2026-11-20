//! L3 领域·管理：`.spk` 的布局、校验与安装 / 导入 / 导出。
//!
//! `model/` / `check/` / `ops/` 按 §5 落位；`package.rs` 与 `plugin.rs` 的内容
//! 拆分按切片推进。

pub mod check;
pub mod model;
pub mod ops;

// The production facade for the plugin install/scan API. `plugin.rs` is now a
// test-only shell; callers use `crate::manage` directly.
pub use check::plugin::*;
pub use check::verify::*;
pub(crate) use model::hex::hex_sha256;
pub use model::layout::*;
pub use model::manifest::*;
pub(crate) use model::path::check_path;
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

#[cfg(test)]
pub mod package;
#[cfg(test)]
pub mod plugin;
