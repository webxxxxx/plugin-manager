//! 启动收敛：这一次 Ready 是直接结算，还是先跑 `lifecycle.started`。
//!
//! 门控只对「同一次 spawn 的 Ready」成立：代次标记在 `Action::Spawn` 时置位、
//! 在钩子启动时消费掉，pid 必须还是当前实例的——迟到的 Ready 不能替另一个
//! 进程跑钩子。

use crate::kernel::registry::State;

/// 该 Ready 事件是否交给 `started` 钩子，而不是直接结算成 Running。
pub(crate) fn takes_ready(owed: bool, state: State, current: Option<u32>, pid: u32) -> bool {
    owed && state == State::Starting && current == Some(pid)
}

#[cfg(test)]
mod tests {
    use super::takes_ready;
    use crate::kernel::registry::State;

    #[test]
    fn a_ready_with_a_started_hook_owed_is_gated() {
        assert!(takes_ready(true, State::Starting, Some(7), 7));
    }

    #[test]
    fn a_definition_without_a_started_hook_settles_directly() {
        assert!(!takes_ready(false, State::Starting, Some(7), 7));
    }

    #[test]
    fn a_late_ready_from_an_older_instance_is_not_gated() {
        assert!(!takes_ready(true, State::Starting, Some(8), 7));
        assert!(!takes_ready(true, State::Running, Some(7), 7));
        // A pid that is already gone has nothing to gate.
        assert!(!takes_ready(true, State::Starting, None, 7));
    }
}
