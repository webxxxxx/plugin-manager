//! 一次性服务：干净退出后效果仍在，所以保持 running、pid 为空。

/// 一次退出算不算「成功」。健康不是就绪时，干净退出也算失败。
pub(crate) fn exited_successfully(code: Option<i32>, ready: bool) -> bool {
    code == Some(0) && ready
}

/// 一次性服务在成功退出后保持 `running`；其余情况一律落到 `stopped`。
pub(crate) fn keeps_running_after_exit(remain_after_exit: bool, successful: bool) -> bool {
    remain_after_exit && successful
}

#[cfg(test)]
mod tests {
    use super::{exited_successfully, keeps_running_after_exit};

    #[test]
    fn only_a_clean_exit_from_a_ready_state_is_successful() {
        assert!(exited_successfully(Some(0), true));
        assert!(!exited_successfully(Some(1), true));
        assert!(!exited_successfully(None, true));
        assert!(!exited_successfully(Some(0), false));
    }

    #[test]
    fn only_a_successful_oneshot_stays_running() {
        assert!(keeps_running_after_exit(true, true));
        assert!(!keeps_running_after_exit(false, true));
        assert!(!keeps_running_after_exit(true, false));
    }
}
