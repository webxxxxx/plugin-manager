//! 内核：四个动词共用的领域定义与判定。不含进程、socket、文件。
//!
//! 这里**没有**整块的 `#![allow(dead_code)]`。留下的是逐项声明，理由写在每一项
//! 旁边，`grep -n "allow(dead_code)" src/kernel` 能一次列全。两类：
//!
//! - **只有测试驱动**：契约的 `lifecycle` 入口在生产里还没有驱动者（ADR-0015 把
//!   exec 服务的生命周期交给监督槽），`Control` / `Wire::lifecycle` / `CallError`
//!   就这样只由 `conformance` 与内核单测驱动。这类用
//!   `#[cfg_attr(not(test), allow(dead_code))]`，测试构建里仍然会被检查。
//! - **定义了但没有构造点**：`Obligations` 的 `Ping` / `Status` / `Hooks` 档、
//!   `Reply::Stream`、`Invocation` 的 `idempotency` / `trace`、`Service::desc`。
//!   这类在没有构造点之前用普通的 `#[allow(dead_code)]`。
//!
//! 没有标注又没有调用方的条目，就是真该删的。

pub mod contract;
pub mod failure;
pub mod hook;
pub mod layout;
pub mod projection;
pub mod registry;
pub mod schema;
pub mod script;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod fixtures;
#[cfg(test)]
mod memory;
