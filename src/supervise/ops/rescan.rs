use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::json;

use serde_json::Value;

use crate::kernel::failure::ProblemError;
use crate::kernel::registry::{Service, State};
use crate::manage as plugin;
use crate::platform::clock::Clock;
use crate::platform::log;
use crate::supervise::check::reconcile;
use crate::supervise::model::definition;
use crate::supervise::model::runtime::{self};
use crate::supervise::ops::host::Supervisor;

impl Supervisor {
    /// Re-scan `services.d` plus the plugin store and reconcile the registry.
    /// Running services keep running (a changed exec applies at the next start);
    /// definitions that disappeared are stopped and forgotten. This is the
    /// recovery path for a late overlay mount, and the operator's way to pick
    /// up a hand-edited definition without restarting the daemon.
    /// Reconcile when `services.d` changed since the last scan: a definition
    /// dropped in while the daemon runs starts (if enabled), a deleted one is
    /// stopped and forgotten. This mirrors the shell `svcd` poll loop.
    pub(crate) fn rescan_if_changed(&mut self, clock: &Clock) {
        let fingerprint = services_fingerprint(&self.config.paths.services_dir);
        if fingerprint == self.services_fingerprint {
            return;
        }
        self.services_fingerprint = fingerprint;
        match self.reconcile_definitions(clock) {
            Ok(summary) => {
                let changed = ["added", "removed", "updated"].iter().any(|key| {
                    summary
                        .get(key)
                        .and_then(Value::as_array)
                        .map(|items| !items.is_empty())
                        .unwrap_or(false)
                });
                if changed {
                    log::svcd(&format!("svcd: rescan: {}", summary));
                }
            }
            Err(error) => log::svcd(&format!("svcd: rescan failed: {}", error)),
        }
    }

    pub(crate) fn reload(&mut self, clock: &Clock) -> Result<Value, ProblemError> {
        let summary = self.reconcile_definitions(clock)?;
        self.services_fingerprint = services_fingerprint(&self.config.paths.services_dir);
        Ok(summary)
    }

    /// Re-scan `services.d` plus the plugin store and reconcile the registry.
    /// Running services keep running (a changed exec applies at the next start);
    /// definitions that disappeared are stopped and forgotten. Shared by
    /// `svc reload`, the periodic rescan, and boot recovery.
    pub(crate) fn reconcile_definitions(&mut self, clock: &Clock) -> Result<Value, ProblemError> {
        let loaded =
            definition::load_services(&self.config.paths.services_dir, &self.config.limits);
        for issue in &loaded.issues {
            log::svcd(&format!("svcd: reload: warning: {}", issue));
        }
        let (plugins, warnings) = plugin::scan(&self.config.paths, &self.config.limits);
        for warning in warnings {
            log::svcd(&format!("svcd: reload: plugin: {}", warning));
        }
        // Core definitions win; an installed plugin still lands in `desired` so
        // the stale sweep below does not unregister it on every reload.
        let desired = reconcile::merge_desired(loaded.registry.managed_values().cloned(), plugins);

        let mut removed = Vec::new();
        let stale = reconcile::stale_ids(
            self.registry
                .managed_values()
                .map(|service| service.id.as_str()),
            &desired,
        );
        for id in stale {
            let active = self
                .slots
                .get(&id)
                .map(|slot| {
                    matches!(
                        slot.machine.state,
                        State::Starting | State::Running | State::Stopping | State::Backoff
                    )
                })
                .unwrap_or(false);
            if active {
                let _ = self.stop_service(&id, clock);
            }
            self.slots.remove(&id);
            self.registry.remove(&id);
            runtime::remove(&id, &self.config.paths);
            self.persisted.remove(&id);
            log::svcd(&format!("svcd: reload: removed {}", id));
            removed.push(id);
        }

        let (added, updated) = self.apply_definitions(desired);

        // Only definitions that appeared *now* are started: a reload must not
        // resurrect services the operator stopped, nor retry Failed ones.
        for id in &added {
            let enabled = self
                .registry
                .get(id)
                .map(|service| service.enabled)
                .unwrap_or(false);
            if enabled {
                if let Err(error) = self.start_service(id, clock) {
                    log::svcd(&format!("svcd: reload: cannot start {}: {}", id, error));
                }
            }
        }
        self.sweep_runtime_records();
        self.catalog_dirty = true;
        Ok(json!({ "added": added, "removed": removed, "updated": updated }))
    }

    /// One reconcile pass over the desired definitions: register new services and
    /// update existing ones. A service rejected by the registry (ADR-0015's
    /// carrier invariant) is logged and skipped; the live service and its slot
    /// stay exactly as they were.
    pub(crate) fn apply_definitions(
        &mut self,
        desired: BTreeMap<String, Service>,
    ) -> (Vec<String>, Vec<String>) {
        let mut added = Vec::new();
        let mut updated = Vec::new();
        for (id, service) in desired {
            let old = self.registry.get(&id).cloned();
            match reconcile::action_for(old.as_ref(), &service) {
                reconcile::Action::Skip => continue,
                reconcile::Action::Add => {
                    if let Some(slot) = self.factory.build(&service) {
                        self.slots.insert(id.clone(), slot);
                    }
                    // A rejected service must not leave a slot with no registry
                    // entry: undo the cheap insert.
                    if let Err(reason) = self.registry.insert(service) {
                        self.slots.remove(&id);
                        log::svcd(&format!("svcd: reload: rejecting {}: {}", id, reason));
                        continue;
                    }
                    log::svcd(&format!("svcd: reload: added {}", id));
                    added.push(id);
                }
                reconcile::Action::Update => {
                    // Plugins are managed through install/uninstall RPCs; the
                    // pure classifier already returned Skip for those.
                    let old = old.expect("update implies an existing definition");
                    let mut next = service;
                    next.state = old.state;
                    next.pid = old.pid;
                    next.since = old.since;
                    next.ready_since = old.ready_since;
                    next.next_at = old.next_at;
                    next.failures = old.failures;
                    next.last_exit = old.last_exit;
                    let exec = next.exec.clone();
                    let lifecycle = next.lifecycle.clone();
                    let restart = next.restart;
                    let remain_after_exit = next.remain_after_exit;
                    let protocol = next.protocol;
                    // Go through the registry door so the carrier invariant is
                    // enforced here too; a rejected definition keeps the live
                    // service and its slot untouched.
                    if let Err(reason) = self.registry.replace(next) {
                        log::svcd(&format!("svcd: reload: rejecting {}: {}", id, reason));
                        continue;
                    }
                    if let Some(slot) = self.slots.get_mut(&id) {
                        if let Some(exec) = exec {
                            slot.set_exec(exec);
                        }
                        slot.set_lifecycle(lifecycle);
                        slot.set_restart(restart);
                        slot.set_remain_after_exit(remain_after_exit);
                        slot.set_protocol(protocol);
                    }
                    log::svcd(&format!("svcd: reload: updated {}", id));
                    updated.push(id);
                }
            }
        }
        (added, updated)
    }
}

/// Name + mtime + size of every entry in `services.d`. Cheap enough to poll;
/// any create/delete/edit changes it and triggers a reconcile.
pub(crate) fn services_fingerprint(directory: &Path) -> Vec<(String, u64, u64)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut fingerprint: Vec<(String, u64, u64)> = entries
        .flatten()
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let (mtime, size) = entry
                .metadata()
                .ok()
                .map(|metadata| {
                    let mtime = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_nanos() as u64)
                        .unwrap_or(0);
                    (mtime, metadata.len())
                })
                .unwrap_or((0, 0));
            (name, mtime, size)
        })
        .collect();
    fingerprint.sort();
    fingerprint
}
