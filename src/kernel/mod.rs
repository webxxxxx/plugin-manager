//! 内核：四个动词共用的领域定义与判定。不含进程、socket、文件。

#![allow(dead_code)]
// 契约类型在 #103 接入监督槽之前没有调用方，先整体放行 dead_code；
// 并入调用点后应逐项摘掉。#212 摘掉试过一次：仍有 30 项（`Control`、
// `Health::{is_alive,is_usable}`、`Invocation` 的 budget/idempotency/trace 等），
// 留待单独一张票，不在这轮投影搬家范围里。

pub mod contract;
pub mod failure;
pub mod projection;
pub mod registry;
pub mod schema;

#[cfg(test)]
pub(crate) mod conformance;
#[cfg(test)]
pub(crate) mod fixtures;
#[cfg(test)]
mod memory;
