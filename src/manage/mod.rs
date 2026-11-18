//! L3 领域·管理：`.spk` 的布局、校验与安装 / 导入 / 导出。
//!
//! `model/` / `check/` / `ops/` 按 §5 落位；`package.rs` 与 `plugin.rs` 的内容
//! 拆分按切片推进。

pub mod check;
pub mod model;
pub mod ops;
pub mod package;
pub mod plugin;
