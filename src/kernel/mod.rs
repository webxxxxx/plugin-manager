//! 内核：四个动词共用的领域定义与判定。不含进程、socket、文件。

#![allow(dead_code)]
// 契约类型在 #103 接入监督槽之前没有调用方，先整体放行 dead_code；
// 并入调用点后应逐项摘掉。

pub mod contract;
pub mod failure;

#[cfg(test)]
mod conformance;
#[cfg(test)]
mod memory;
