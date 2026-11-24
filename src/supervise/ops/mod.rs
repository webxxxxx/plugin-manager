//! 监督真正干活的部分：起进程、写文件、重启与探针。

pub mod builtin_adapter;
pub mod factory;
pub mod hook;
pub mod host;
pub mod invoke;
pub mod lifecycle;
pub mod probe;
pub mod rescan;
pub mod restart;
pub mod slot;
