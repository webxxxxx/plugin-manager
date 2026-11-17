//! L3 领域·监督：注册表、6 态核、监督槽、运行态记录与生命周期动作。
//!
//! `model` / `check` / `ops` 三段拆分是每个文件的后续票；step 6 只搬文件。

pub mod builtin_adapter;
pub mod definition;
pub mod hook;
pub mod runtime;
pub mod script;
pub mod slot;
pub mod supervisor;
