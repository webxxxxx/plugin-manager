//! The processes a stop has to take that are not in the process group.
//!
//! A child that calls `setsid` leaves the group — dropbear's per-connection
//! child does it for every connection it accepts — and once the leader exits it
//! is reparented away from the tree, so the snapshot has to be taken before any
//! signal goes out (#332, ADR-0031).

use crate::platform::host::process;

use super::Slot;

impl Slot {
    /// True while any process of the service is alive: the leader's group plus
    /// the descendants that left it with `setsid` (#332).
    pub(super) fn tree_alive(&self) -> bool {
        let Some(pid) = self.machine.pid else {
            return false;
        };
        process::tree_alive(self.tree.as_ref(), pid) || self.descendants.alive()
    }

    /// Snapshot the tree for this stop, once.
    ///
    /// A stop can be asked for twice (a CLI stop on a `Stopping` service,
    /// shutdown after a timeout) and a forced stop can arrive without a stop
    /// request at all (shutdown paths, stdout EOF). Capturing again would
    /// overwrite a good snapshot with an empty one — by then the leader may be
    /// gone and its children reparented — so the first capture wins (#332,
    /// #334).
    pub(super) fn capture_tree(&mut self) {
        if !self.descendants.is_empty() {
            return;
        }
        self.descendants = Descendants::capture(self.machine.pid);
        if !self.descendants.is_empty() {
            info!(
                "{}: stop takes {} descendant process(es) outside the group",
                self.id,
                self.descendants.len()
            );
        }
    }
}

/// The descendants captured at the start of one stop, each with the start time
/// that keeps a recycled pid from being signalled.
#[derive(Default)]
pub(super) struct Descendants {
    pids: Vec<process::Descendant>,
}

impl Descendants {
    /// Snapshot `pid`'s tree. Empty when there is no leader, or when the host
    /// guards its tree with a handle instead of walking parenthood.
    pub(super) fn capture(pid: Option<u32>) -> Self {
        let Some(pid) = pid else {
            return Self::default();
        };
        Self {
            pids: process::descendants(pid),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.pids.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }

    /// True while any captured process is still the one the snapshot saw.
    pub(super) fn alive(&self) -> bool {
        process::descendants_alive(&self.pids)
    }

    /// The polite half of the stop: ask them to exit with the group.
    pub(super) fn polite(&self) {
        process::stop_descendants(&self.pids);
    }

    /// The kill half: whatever ignored the polite signal dies here.
    pub(super) fn kill(&self) {
        process::kill_descendants(&self.pids);
    }

    pub(super) fn clear(&mut self) {
        self.pids.clear();
    }
}
