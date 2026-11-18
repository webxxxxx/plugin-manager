//! 稳定窗口：一次崩溃要先把停止动作跑完再重启，还是直接进入退避。

pub const STABLE_MS: u64 = 10_000;

/// A run is stable once it has been ready for the whole stability window.
pub fn is_stable(ready_since_ms: Option<u64>, now_ms: u64) -> bool {
    ready_since_ms.is_some_and(|ready| now_ms.saturating_sub(ready) >= STABLE_MS)
}
