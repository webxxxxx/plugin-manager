//! 重启退避：失败次数 → 下一次重试的延迟。

/// Full §1.3 schedule. `MAX_FAILURES = 5` means one crash loop enters `Failed`
/// on the fifth failure, so only 1/2/4/8s are armed in practice; 16s/30s are
/// the policy ceiling kept for a raised cap.
pub const BACKOFF_MS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000];
pub const MAX_FAILURES: u32 = 5;

pub fn backoff_delay(failures: u32) -> u64 {
    let index = failures.saturating_sub(1) as usize;
    BACKOFF_MS[index.min(BACKOFF_MS.len() - 1)]
}
