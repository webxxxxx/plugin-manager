//! L3 领域·管理的稳定部分：清单类型。
//!
//! tar 编解码、校验（`check/`）与安装/导入/导出（`ops/`）随 `package.rs` 的
//! 后续切片落地。

pub mod hex;
pub mod layout;
pub mod manifest;
pub mod path;
pub mod ustar;
