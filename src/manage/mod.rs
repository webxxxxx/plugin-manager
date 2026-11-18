//! L3 领域·管理：`.spk` 的布局、校验与安装 / 导入 / 导出。
//!
//! `check/` 与 `ops/` 已按 §5 落位；`package.rs` 与 `plugin.rs` 的内容拆分
//! （model / verify / install）留给后续票，暂时留在本目录根。

pub mod check;
pub mod ops;
pub mod package;
pub mod plugin;
