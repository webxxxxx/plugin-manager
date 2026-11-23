//! 特例：宿主领域（注册服务、PATH、卸载器）。
//!
//! `plan` 决定做什么（纯函数），`execute` 做（输入构造、副作用、平台分派），
//! 这个文件只做显式再导出——不用 `pub use *`，否则 dep-lint 的 `files`
//! 身份与真实依赖会脱节（#207）。

mod execute;
mod plan;
pub(crate) mod upgrade;

#[cfg(windows)]
pub(crate) use execute::wait_for_process;
pub use execute::{
    ensure_conf, install_service, self_install, self_uninstall, service_start, service_status,
    service_stop, uninstall_service,
};
#[cfg(windows)]
pub(crate) use plan::SCM_NAME;
#[cfg(windows)]
pub mod service_win;
