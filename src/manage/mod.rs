//! L3 领域·管理：`.spk` 的布局、校验与安装 / 导入 / 导出。
//!
//! `model` / `check` / `ops` 三段拆分是每个文件的后续票；step 6 只把文件搬进
//! 领域目录，不改内容。

pub mod bundle;
pub mod interpreter;
pub mod package;
pub mod plugin;
pub mod transfer;
pub mod trust;
