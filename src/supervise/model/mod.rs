//! 监督的稳定部分：定义、运行态记录与状态机核。不碰进程、文件或 socket。

pub mod definition;
pub mod runtime;
pub mod supervisor;
