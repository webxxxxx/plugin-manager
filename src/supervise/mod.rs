//! L3 领域·监督：注册表在内核，这里按 `model` / `ops` 落位（`check` 随内容拆分再建）。
//!
//! - `model/`：定义解析、运行态记录、6 态纯核。
//! - `ops/`：监督槽、hook、脚本与内置载体。

pub mod model;
pub mod ops;
