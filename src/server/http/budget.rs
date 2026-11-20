//! The shared `max_streams` admission counter for stream-handler transfers.
//!
//! Long-lived stream handlers (downloads, uploads, SSE and WebSocket) draw
//! from one budget; one-shot static assets are exempt. The composition root
//! constructs exactly one budget and hands it to the server drivers, so a test
//! can build a second one without sharing process state. The Unix server loop
//! uses it via `driver::StreamDriver`, the Windows thread fallback via
//! `fallback::drive`. Downloads, uploads, package install/import and export
//! all count here; one-shot static assets do not.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::stream::StreamClass;

/// Why a stream was refused before its headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// A `max_streams` slot held by one long-lived stream. One-shot responses do
/// not acquire one, so a page's static assets never trip the long-lived cap.
pub struct StreamPermit {
    active: Arc<AtomicUsize>,
}

impl Drop for StreamPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The shared admission counter behind `max_streams`.
#[derive(Clone)]
pub struct StreamBudget {
    active: Arc<AtomicUsize>,
}

impl StreamBudget {
    /// One independent budget. The composition root calls this once per
    /// process; tests call it per case so two budgets never interfere.
    pub fn new() -> StreamBudget {
        StreamBudget {
            active: Arc::new(AtomicUsize::new(0)),
        }
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
                        active: self.active.clone(),
                    }))
                }
                Err(actual) => current = actual,
            }
        }
    }
}

impl Default for StreamBudget {
    fn default() -> Self {
        StreamBudget::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_one_shot_response_never_consumes_the_long_lived_cap() {
        let budget = StreamBudget::new();
        let held = budget
            .acquire(StreamClass::LongLived, 1)
            .expect("the first long-lived stream fits")
            .expect("a long-lived stream holds a slot");
        assert!(budget.acquire(StreamClass::LongLived, 1).is_err());
        assert!(matches!(budget.acquire(StreamClass::OneShot, 1), Ok(None)));
        drop(held);
        assert!(budget.acquire(StreamClass::LongLived, 1).is_ok());
    }

    #[test]
    fn a_long_lived_permit_is_released_exactly_once() {
        let budget = StreamBudget::new();
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
            .expect("fits")
            .expect("holds a slot");
        drop(second);
        drop(third);
        assert!(budget.acquire(StreamClass::LongLived, 1).is_ok());
    }

    #[test]
    fn two_budgets_do_not_share_a_counter() {
        let first = StreamBudget::new();
        let second = StreamBudget::new();
        let held = first
            .acquire(StreamClass::LongLived, 1)
            .expect("first fits")
            .expect("holds");
        assert!(first.acquire(StreamClass::LongLived, 1).is_err());
        assert!(
            second.acquire(StreamClass::LongLived, 1).is_ok(),
            "a second budget must not see the first budget's permit"
        );
        drop(held);
    }
}
