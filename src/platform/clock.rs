//! The clock seam (S8): one production source of "now".
//!
//! The supervisor runs on monotonic milliseconds; the runtime records, the
//! status projection and the adoption conversion all need epoch seconds. The
//! two are captured **together** once per tick into [`Now`], so every
//! conversion afterwards is a pure function of the same pair — no conversion
//! reads the wall clock a second time.
//!
//! [`Clock`] is constructed once at the composition root and passed by
//! reference to the paths that wait on time (the supervisor loop and its
//! blocking start/stop waits). It is not a field on `Daemon` and not a trait:
//! time is a parameter. See `docs/adr/0020-time-is-a-parameter-not-a-trait.md`.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// One instant in both domains, captured together by [`Clock::now`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Now {
    /// Milliseconds since the daemon started (the supervisor's timeline).
    pub mono_ms: u64,
    /// Whole epoch seconds at the same instant.
    pub epoch_secs: u64,
}

impl Now {
    /// A fixed pair, for tests and for callers that never had a clock.
    #[cfg(test)]
    pub(crate) fn at(mono_ms: u64, epoch_secs: u64) -> Self {
        Now {
            mono_ms,
            epoch_secs,
        }
    }

    /// Convert a monotonic supervisor stamp to epoch seconds.
    pub(crate) fn epoch_of(&self, mono_ms: u64) -> u64 {
        self.epoch_secs
            .saturating_sub(self.mono_ms.saturating_sub(mono_ms) / 1000)
    }

    /// The inverse of [`Now::epoch_of`]: epoch seconds back to a monotonic
    /// stamp.
    pub(crate) fn mono_of(&self, epoch_secs: u64) -> u64 {
        self.mono_ms.saturating_sub(
            self.epoch_secs
                .saturating_sub(epoch_secs)
                .saturating_mul(1000),
        )
    }
}

/// The daemon's single clock: one monotonic origin, read together with the wall
/// clock on every `now()`. Reading the wall clock per call keeps a system time
/// adjustment visible to the projection; capturing both fields once keeps the
/// two domains consistent for all conversions that follow.
///
/// `Clock` is `Copy`: the logger holds its own copy so a log line never needs
/// the supervisor's `Now` (ADR-0022). A fixed clock exists for tests that
/// assert the exact bytes of a log line.
#[derive(Clone, Copy)]
pub(crate) struct Clock {
    origin: Instant,
    fixed_epoch_ms: Option<u64>,
}

impl Clock {
    pub(crate) fn boot() -> Self {
        Clock {
            origin: Instant::now(),
            fixed_epoch_ms: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixed(epoch_ms: u64) -> Self {
        Clock {
            origin: Instant::now(),
            fixed_epoch_ms: Some(epoch_ms),
        }
    }

    pub(crate) fn now(&self) -> Now {
        Now {
            mono_ms: self.origin.elapsed().as_millis() as u64,
            epoch_secs: self.epoch_ms() / 1000,
        }
    }

    /// Wall-clock milliseconds since the Unix epoch.
    ///
    /// This is the log line's time column (ADR-0022). It is I/O metadata, not a
    /// state input, which is why it is a projection on the injected clock
    /// instead of another `Now` field every caller has to thread through.
    pub(crate) fn epoch_ms(&self) -> u64 {
        if let Some(fixed) = self.fixed_epoch_ms {
            return fixed;
        }
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}
#[cfg(test)]
mod tests {
    use super::{Clock, Now};

    #[test]
    fn epoch_and_monotonic_round_trip_through_one_pair() {
        let now = Now::at(120_000, 1_700_000_000);

        // A stamp 90.5s in the past is 90 whole seconds earlier.
        assert_eq!(now.epoch_of(29_500), 1_699_999_910);
        assert_eq!(now.mono_of(1_699_999_910), 30_000);

        // The current instant is itself in both domains.
        assert_eq!(now.epoch_of(now.mono_ms), now.epoch_secs);
        assert_eq!(now.mono_of(now.epoch_secs), now.mono_ms);
    }

    #[test]
    fn conversions_never_underflow_on_a_clock_that_moved_backwards() {
        let now = Now::at(1_000, 10);

        // A stamp "after now" clamps instead of wrapping.
        assert_eq!(now.epoch_of(5_000), 10);
        // An epoch "after now" clamps instead of wrapping.
        assert_eq!(now.mono_of(20), 1_000);
    }

    #[test]
    fn a_booted_clock_moves_forward_and_keeps_epoch_seconds_sane() {
        let clock = Clock::boot();
        let first = clock.now();
        let second = clock.now();

        assert!(second.mono_ms >= first.mono_ms);
        assert!(second.epoch_secs >= first.epoch_secs);
        assert!(first.epoch_secs > 1_600_000_000, "wall clock is set");
    }
}
