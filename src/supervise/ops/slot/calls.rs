//! In-flight `tools/call` bookkeeping: the pending map and the JSON-RPC id
//! space it shares with the readiness ping.

use std::collections::BTreeMap;

use super::PendingCall;

/// The calls a slot is waiting on. Ids come from one JSON-RPC request space
/// shared with the readiness ping, so a late ping answer cannot be mistaken
/// for the answer to a call.
pub(super) struct PendingCalls {
    calls: BTreeMap<u64, PendingCall>,
    next_id: u64,
}

impl PendingCalls {
    pub(super) fn new() -> PendingCalls {
        PendingCalls {
            calls: BTreeMap::new(),
            next_id: 1,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.calls.len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &PendingCall> + '_ {
        self.calls.values()
    }

    pub(super) fn insert(&mut self, id: u64, call: PendingCall) {
        self.calls.insert(id, call);
    }

    pub(super) fn remove(&mut self, id: u64) -> Option<PendingCall> {
        self.calls.remove(&id)
    }

    /// Allocate the next request id.
    pub(super) fn allocate(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// A fresh process restarts the id space at 1.
    pub(super) fn reset(&mut self) {
        self.next_id = 1;
    }

    /// Ids of the calls that passed their deadline.
    pub(super) fn expired(&self, now: u64) -> Vec<u64> {
        self.calls
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Every pending call, for failing them all with one reason.
    pub(super) fn take_all(&mut self) -> BTreeMap<u64, PendingCall> {
        std::mem::take(&mut self.calls)
    }
}
