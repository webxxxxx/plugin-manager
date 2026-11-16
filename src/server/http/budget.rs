//! The shared `max_streams` admission counter.
//!
//! Long-lived transfer handlers (downloads, uploads, SSE, WebSocket) draw from
//! one global budget; one-shot static assets are exempt. Both engines use this
//! counter: the Unix server loop via `driver::StreamDriver`, the Windows
//! thread fallback via `fallback::drive`, and the still-legacy blob uploads via
//! `blob_http`.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::stream::StreamClass;

static ACTIVE_STREAMS: AtomicUsize = AtomicUsize::new(0);

/// Why a stream was refused before its headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// A `max_streams` slot held by one long-lived stream. One-shot responses do
/// not acquire one, so a page's static assets never trip the long-lived cap.
pub struct StreamPermit {
    active: &'static AtomicUsize,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The shared admission counter behind `max_streams`.
pub struct StreamBudget {
    active: &'static AtomicUsize,
}

impl StreamBudget {
    pub fn global() -> StreamBudget {
        StreamBudget {
            active: &ACTIVE_STREAMS,
        }
    }

    #[cfg(test)]
    fn for_counter(active: &'static AtomicUsize) -> StreamBudget {
        StreamBudget { active }
    }

    /// `Ok(None)` for an exempt one-shot response; `Err(Busy)` when the cap
    /// is already reached.
    pub fn acquire(&self, class: StreamClass, limit: usize) -> Result<Option<StreamPermit>, Busy> {
        if class == StreamClass::OneShot {
            return Ok(None);
        }
        let mut current = self.active.load(Ordering::Acquire);
        loop {
            if current >= limit {
                return Err(Busy);
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(Some(StreamPermit {
                        active: self.active,
                    }))
                }
                Err(actual) => current = actual,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // One counter per test: the tests run in parallel and a shared store(0)
    // would race a permit drop in the other test.
    static ONE_SHOT_COUNTER: AtomicUsize = AtomicUsize::new(0);
    static RELEASE_COUNTER: AtomicUsize = AtomicUsize::new(0);

    #[test]
    fn a_one_shot_response_never_consumes_the_long_lived_cap() {
        let budget = StreamBudget::for_counter(&ONE_SHOT_COUNTER);
        let held = budget
            .acquire(StreamClass::LongLived, 1)
            .expect("the first long-lived stream fits")
            .expect("a long-lived stream holds a slot");
        assert!(budget.acquire(StreamClass::LongLived, 1).is_err());
        assert!(matches!(budget.acquire(StreamClass::OneShot, 1), Ok(None)));
        drop(held);
        assert!(budget.acquire(StreamClass::LongLived, 1).is_ok());
        assert_eq!(ONE_SHOT_COUNTER.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_long_lived_permit_is_released_exactly_once() {
        let budget = StreamBudget::for_counter(&RELEASE_COUNTER);
        let first = budget
            .acquire(StreamClass::LongLived, 2)
            .expect("fits")
            .expect("holds a slot");
        let second = budget
            .acquire(StreamClass::LongLived, 2)
            .expect("fits")
            .expect("holds a slot");
        assert!(budget.acquire(StreamClass::LongLived, 2).is_err());
        drop(first);
        let third = budget
            .acquire(StreamClass::LongLived, 2)
            .expect("a released slot is reusable")
            .expect("holds a slot");
        drop(second);
        assert_eq!(RELEASE_COUNTER.load(Ordering::SeqCst), 1);
        drop(third);
        assert_eq!(RELEASE_COUNTER.load(Ordering::SeqCst), 0);
    }
}
