//! 特例：宿主领域（注册服务、PATH、卸载器）。
//!
//! plan → execute 的拆分是后续票；step 6 只搬文件。

pub mod install;
#[cfg(windows)]
pub mod service_win;
