//! The CPU budget: how much of this machine the daemon may use.
//!
//! Memory is a reservation; CPU is a rate. A percentage can only be held by
//! refusing new work once the measured rate stays above it, so
//! cpu_budget_percent has two halves: the kernel cap from
//! platform::host::cpu (installed when the host allows it) and this admission
//! gate, which every host gets.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::platform::host::cpu;

/// How often the sampler reads this process's CPU time.
const SAMPLE: Duration = Duration::from_millis(250);
/// Weight of the newest sample in the moving average. Low enough that one
/// expensive request does not close the gate, high enough to react in ~1 s.
const ALPHA: f64 = 0.4;

pub(crate) struct CpuGovernor {
    target_percent: u8,
    cores: usize,
    /// Set while the moving average is above the target. Read on every accept.
    over: AtomicBool,
    stop: AtomicBool,
}

impl CpuGovernor {
    /// None when the budget is off (0), so the admission check costs nothing.
    pub(crate) fn new(target_percent: u8, cores: usize) -> Option<Arc<Self>> {
        if target_percent == 0 || cores == 0 {
            return None;
        }
        Some(Arc::new(CpuGovernor {
            target_percent,
            cores,
            over: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }))
    }

    pub(crate) fn target_percent(&self) -> u8 {
        self.target_percent
    }

    pub(crate) fn cores(&self) -> usize {
        self.cores
    }

    /// True when new connections should be refused right now.
    pub(crate) fn should_shed(&self) -> bool {
        self.over.load(Ordering::Relaxed)
    }

    /// Sample this process's own CPU time until the process exits. The thread
    /// holds only the governor, so nothing blocks shutdown.
    pub(crate) fn start(self: &Arc<Self>) {
        let governor = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("svcd-cpu".into())
            .stack_size(64 * 1024)
            .spawn(move || governor.sample_forever());
        if let Err(error) = spawned {
            // Without a sampler the gate stays closed, which would refuse every
            // connection: log and leave it open instead.
            crate::platform::log::svcd(&format!(
                "svcd: cpu governor thread failed: {}; admission stays open",
                error
            ));
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn sample_forever(&self) {
        let mut previous = cpu::own_cpu_ms();
        let mut previous_at = Instant::now();
        let mut average: Option<f64> = None;
        while !self.stop.load(Ordering::Relaxed) {
            thread::sleep(SAMPLE);
            let Some(now_cpu) = cpu::own_cpu_ms() else {
                continue;
            };
            let Some(before) = previous else {
                previous = Some(now_cpu);
                previous_at = Instant::now();
                continue;
            };
            let wall = previous_at.elapsed();
            let used = now_cpu.saturating_sub(before);
            average = Some(blended(average, utilization(used, wall, self.cores)));
            self.over.store(
                exceeds(average.unwrap_or(0.0), self.target_percent),
                Ordering::Relaxed,
            );
            previous = Some(now_cpu);
            previous_at = Instant::now();
        }
    }

    #[cfg(test)]
    pub(crate) fn set_over(&self, over: bool) {
        self.over.store(over, Ordering::Relaxed);
    }
}

/// Fraction of this machine's CPU that used_ms per wall represents.
pub(crate) fn utilization(used_ms: u64, wall: Duration, cores: usize) -> f64 {
    let capacity = wall.as_secs_f64() * 1000.0 * cores.max(1) as f64;
    if capacity <= 0.0 {
        0.0
    } else {
        (used_ms as f64 / capacity).clamp(0.0, 1.0)
    }
}

/// One step of the moving average, seeded by the first sample.
pub(crate) fn blended(previous: Option<f64>, sample: f64) -> f64 {
    match previous {
        None => sample,
        Some(value) => value * (1.0 - ALPHA) + sample * ALPHA,
    }
}

pub(crate) fn exceeds(utilization: f64, target_percent: u8) -> bool {
    utilization > target_percent as f64 / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utilization_is_share_of_every_core() {
        // Half a second of CPU over one second on four cores is 12.5%.
        let used = utilization(500, Duration::from_secs(1), 4);
        assert!((used - 0.125).abs() < 1e-9, "{used}");
        assert_eq!(utilization(1000, Duration::from_secs(1), 1), 1.0);
        assert_eq!(utilization(0, Duration::from_secs(1), 8), 0.0);
        // A read that claims more than the machine can do is clamped.
        assert_eq!(utilization(10_000, Duration::from_secs(1), 1), 1.0);
    }

    #[test]
    fn the_average_is_seeded_then_moves_by_alpha() {
        assert_eq!(blended(None, 0.5), 0.5);
        let next = blended(Some(0.5), 1.0);
        assert!((next - 0.7).abs() < 1e-9, "{next}");
    }

    #[test]
    fn the_gate_opens_above_the_target_and_closes_below_it() {
        assert!(exceeds(0.95, 90));
        assert!(!exceeds(0.90, 90));
        assert!(!exceeds(0.5, 90));
        assert!(exceeds(0.5, 40));
    }

    #[test]
    fn a_zero_budget_installs_no_governor() {
        assert!(CpuGovernor::new(0, 8).is_none());
        assert!(CpuGovernor::new(90, 0).is_none());
        let governor = CpuGovernor::new(90, 8).expect("governor");
        assert_eq!(governor.target_percent(), 90);
        assert_eq!(governor.cores(), 8);
        assert!(!governor.should_shed());
        governor.set_over(true);
        assert!(governor.should_shed());
    }
}
