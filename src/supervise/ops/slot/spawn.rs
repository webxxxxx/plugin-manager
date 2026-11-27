//! Spawning the supervised process: the command, its stdio plan, parent-death
//! protection, the service tree and the log writer. Kept out of `slot.rs` so
//! that file stays inside the size budget (#239).

use super::*;

impl Slot {
    pub(super) fn spawn_process(&mut self) -> Result<u32, String> {
        self.exit_while_tree_alive = None;
        // `ready: port` must know whether the port is ours to take: a squatter
        // would satisfy the probe and make a broken start look ready.
        if let Ready::Port(addr) = self.exec.ready {
            match std::net::TcpListener::bind(addr) {
                // Was free a moment ago; the instance gets its chance now.
                Ok(listener) => drop(listener),
                Err(error) => return Err(format!("{} is not available: {}", addr, error)),
            }
        }
        if self.tree.is_none() {
            self.tree = crate::platform::host::process::Tree::new();
            if self.tree.is_none() {
                warn!(
                    "{}: process job unavailable; stop only terminates the leader",
                    self.id
                );
            }
        }
        let plugin_dir = self.plugin_dir.as_deref();
        let mut command = command_for(&self.exec);
        match plugin_dir {
            // A core service is written by the operator and keeps the old
            // environment surface (#312). The process is its start action.
            None => apply_core_child_env(&mut command, &self.id, "start", &self.paths),
            Some(_) => apply_plugin_child_env(&mut command, plugin_dir, &self.paths),
        }
        let blob_dir = self.paths.temp_dir.clone();
        let _ = std::fs::create_dir_all(crate::platform::storage::incoming_dir(&self.paths));
        command.env("SVCD_TEMP_DIR", &blob_dir);
        let plan = match self.exec.ready {
            Ready::Notify => Plan::notify(),
            Ready::Exec | Ready::Port(_) => Plan::exec(self.frames.protocol()),
        };
        let (mut child, streams) =
            stdio::spawn(command, plan).map_err(|error| error.to_string())?;
        if let Err(error) = crate::platform::host::process::adopt(&child) {
            warn!("{}: parent-death protection: {}", self.id, error);
        }
        if let Some(tree) = &self.tree {
            if !tree.assign(&child) {
                warn!(
                    "{}: cannot assign the process job; stop only terminates the leader",
                    self.id
                );
            }
        }
        let pid = child.id();
        info!(
            "{}: spawned pid {} (ready={:?})",
            self.id, pid, self.exec.ready
        );
        let log = match LogWriter::open(
            &self.paths.logs_dir,
            &self.id,
            &format!("plugin:{}", self.id),
            &self.limits,
            log::clock(),
            false,
        ) {
            Ok(sink) => Some(Arc::new(Mutex::new(sink))),
            Err(error) => {
                error!("{}: cannot open service log: {}", self.id, error);
                None
            }
        };
        self.log = log.clone();

        let Streams {
            stdin,
            stdout,
            stderr,
        } = streams;

        // Protocol 3 always runs over the raw transport: the hub owns stdio,
        // stream 0 carries JSON-RPC when the instance is a channel peer, and
        // streams >= 1 carry raw payloads. Below is the JSON-only path.
        if self.frames.protocol() == RAW_PROTOCOL {
            self.stdin = None;
            let (Some(stdin), Some(stdout), Some(stderr)) = (stdin, stdout, stderr) else {
                return abandon(&mut child, "protocol 3 needs piped stdio");
            };
            if let Err(error) = self.start_raw_transport(stdin, stdout, stderr, log.clone()) {
                return abandon(&mut child, error);
            }
            self.child = Some(child);
            self.adopted = false;
            return Ok(pid);
        }

        if self.exec.ready == Ready::Notify {
            let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE);
            let Some(stdout) = stdout else {
                return abandon(&mut child, "piped stdout is missing");
            };
            let Some(stderr) = stderr else {
                return abandon(&mut child, "piped stderr is missing");
            };
            if let Err(error) = self.stdio.frames(
                self.id.clone(),
                self.frames.protocol(),
                stdout,
                Some(stderr),
                sender,
                self.wake.clone(),
                log.clone(),
                &self.limits,
            ) {
                return abandon(&mut child, error.to_string());
            }

            self.stdin = stdin;
            self.frames.attach(receiver);
            self.pending.reset();
            self.ping_id = 1;
        } else {
            self.stdin = None;
            let Some(stdout) = stdout else {
                return abandon(&mut child, "piped stdout is missing");
            };
            let Some(stderr) = stderr else {
                return abandon(&mut child, "piped stderr is missing");
            };

            if let Err(error) = self.stdio.logs(
                self.id.clone(),
                stdout,
                Some(stderr),
                log.clone(),
                &self.limits,
            ) {
                return abandon(&mut child, error.to_string());
            }
            self.frames.detach();
        }

        self.child = Some(child);
        self.adopted = false;
        Ok(pid)
    }
}
