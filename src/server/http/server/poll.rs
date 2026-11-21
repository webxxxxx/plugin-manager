use super::*;

/// After the final response, keep draining the peer for this long before
/// closing. Closing with unread request bytes sends RST, which can discard a
/// status line the client has not read yet; the grace bounds how long a silent
/// peer can pin the connection.
const CLOSE_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// Cap one drain turn so a peer that keeps sending cannot hold the reactor
/// inside a read; the loop re-enters and the grace deadline still closes it.
const DRAIN_BUDGET: usize = 64 * 1024;

/// Read and discard whatever the peer has already sent. Returns true when the
/// connection should stay in the loop (it is still open with more to drain),
/// false when it reached EOF or failed and can be closed now.
fn drain_available(connection: &mut Connection) -> bool {
    let mut junk = [0u8; 4096];
    let mut budget = DRAIN_BUDGET;
    loop {
        if budget == 0 {
            return true;
        }
        match connection.stream.read(&mut junk) {
            Ok(0) => return false,
            Ok(read) => budget = budget.saturating_sub(read),
            Err(error) if error.kind() == ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

/// Half-close the response side, drain what is already queued, and hand the
/// socket back to the loop for the rest of the peer's bytes.
fn begin_close(connection: &mut Connection) -> bool {
    let _ = connection.stream.shutdown(Shutdown::Write);
    if !drain_available(connection) {
        return false;
    }
    connection.state = ConnState::Draining(Instant::now() + CLOSE_DRAIN_GRACE);
    true
}

impl ConnectionOps for Server {
    type State = ConnState;

    fn new_state(&mut self, _id: u64, _peer: IpAddr) -> ConnState {
        ConnState::Reading
    }

    fn on_accept(&mut self, _connection: &mut Connection) {}

    fn route_request(
        &mut self,
        id: u64,
        peer: IpAddr,
        head: &http::Head,
    ) -> Result<BodyPolicy, Vec<u8>> {
        match route_face_request(self.face.as_ref(), head, peer, &self.lan) {
            Ok(Some(prepared)) => {
                let body = prepared.route.body;
                self.face_plans.insert(id, prepared);
                Ok(body)
            }
            Ok(None) => Ok(self.routes.body_policy(head)),
            Err(response) => Err(response),
        }
    }

    fn reads_request(&self, connection: &Connection) -> bool {
        matches!(connection.state, ConnState::Reading)
    }

    fn pre_request(&mut self, connection: &mut Connection) -> PreRequest {
        if connection.pending.is_some() {
            return PreRequest::Continue;
        }
        let Some(header_end) = http::header_end(&connection.buffer) else {
            return PreRequest::Continue;
        };
        if header_end > self.config.limits.max_header {
            return PreRequest::Continue;
        }
        let Some((head, body_start)) = http::parse_head(&connection.buffer[..header_end + 4])
        else {
            return PreRequest::Continue;
        };
        // A pre-body route can answer before advance_request sees the head, so
        // record the method here as well. advance_request resets it for the
        // requests that continue to the buffered path.
        connection.head_only = http::is_head(&head.method);
        let Some(claim) =
            self.routes
                .claim(&head, connection.peer, &self.lan, self.raw_hub.clone())
        else {
            return PreRequest::Continue;
        };
        let exchange = match claim.exchange {
            Ok(exchange) => exchange,
            Err(response) => {
                write_response(connection, response, false);
                return PreRequest::Handled;
            }
        };
        let Exchange::Stream(handler) = exchange else {
            write_response(
                connection,
                http::json_error(
                    500,
                    "internal",
                    "a streamed route returned a memory response",
                ),
                false,
            );
            return PreRequest::Handled;
        };
        let initial = connection.buffer.split_off(body_start);
        connection.buffer.clear();
        attach_stream(
            connection,
            handler,
            &initial,
            None,
            &self.budget,
            &self.config.limits,
        );
        self.service_stream_job(connection);
        PreRequest::Handled
    }

    fn on_request(&mut self, connection: &mut Connection, request: HttpRequest) {
        let keep_alive = wants_keep_alive(&request);
        if let Some(prepared) = self.face_plans.remove(&connection.id) {
            self.serve_face(connection, prepared, request.body, keep_alive);
            return;
        }
        if let Some(prepared) = self.routes.mcp(&request, &self.lan) {
            match prepared {
                Ok(call) => {
                    // Reserve the long-lived slot before starting the plugin
                    // call: a full budget must refuse the request without a
                    // side effect on the service.
                    let Ok(Some(permit)) = self
                        .budget
                        .acquire(StreamClass::LongLived, self.config.limits.max_streams)
                    else {
                        write_response(
                            connection,
                            crate::server::http::stream_busy_response(),
                            false,
                        );
                        return;
                    };
                    let (sender, receiver) = mpsc::sync_channel(self.config.limits.progress_queue);
                    let request = json!({
                        "name": call.name,
                        "arguments": call.arguments,
                        "progressToken": call.progress_token,
                    });
                    // A token/connect failure becomes the stream's first
                    // progress event, so the client still sees the -32603
                    // channel error instead of an HTTP 500 before the SSE
                    // headers.
                    let stream = self.client.open(
                        self.config.paths.control_dir(),
                        "registry.call",
                        request,
                        sender,
                    );
                    let cancel = self.client.canceller();
                    let handler = McpSseStream::new(
                        receiver,
                        call.id,
                        stream,
                        Box::new(move |id| cancel.cancel(id)),
                    );
                    // The slot reserved above becomes the driver's
                    // `max_streams` permit.
                    attach_stream(
                        connection,
                        Box::new(handler),
                        &[],
                        Some(permit),
                        &self.budget,
                        &self.config.limits,
                    );
                    return;
                }
                Err(response) => {
                    write_response(connection, response, false);
                    return;
                }
            }
        }
        self.submit_rendered(connection, request, keep_alive);
    }

    fn on_error(&mut self, connection: &mut Connection, response: Vec<u8>) {
        if response.is_empty() {
            // An interim response could not be written atomically. Close
            // without adding a status line behind the truncated bytes.
            connection.out.clear();
            connection.out_at = 0;
            connection.keep_alive = false;
            connection.state = ConnState::Writing;
            return;
        }
        write_response(connection, response, false);
    }

    fn on_readable(&mut self, connection: &mut Connection) -> bool {
        if matches!(connection.state, ConnState::Draining(_)) {
            return drain_available(connection);
        }
        if matches!(&connection.state, ConnState::Stream(_)) {
            let keep = stream_readable(connection);
            if keep {
                self.service_stream_job(connection);
            }
            return keep;
        }
        true
    }

    fn on_writable(&mut self, connection: &mut Connection) -> bool {
        if matches!(&connection.state, ConnState::Stream(_)) {
            let keep = stream_writable(connection);
            if keep {
                self.service_stream_job(connection);
            }
            return keep;
        }
        {
            {
                if !connection.write_out() {
                    return false;
                }
                if connection.out_at >= connection.out.len() {
                    if connection.keep_alive {
                        connection.state = ConnState::Reading;
                        true
                    } else {
                        begin_close(connection)
                    }
                } else {
                    true
                }
            }
        }
    }

    fn interest(&self, connection: &Connection) -> i16 {
        let mut events = match &connection.state {
            ConnState::Reading => libc::POLLIN,
            ConnState::Writing => libc::POLLOUT,
            // A closing connection only waits for the peer's remaining bytes
            // (or its FIN); the response side is already shut down.
            ConnState::Draining(_) => libc::POLLIN,
            ConnState::Stream(driver) => {
                if driver.wants_read() {
                    libc::POLLIN
                } else if driver.has_job() {
                    // A stream job in flight still has to notice a client FIN
                    // so it can be cancelled; the engine owns the platform's
                    // half-close flag. A stream merely blocked on write keeps
                    // the old behaviour and may still deliver its response.
                    engine::paused_stream_interest()
                } else {
                    0
                }
            }
            ConnState::Processing => 0,
        };
        if connection.out_at < connection.out.len() || stream_has_file(connection) {
            events |= libc::POLLOUT;
        }
        events
    }

    fn is_idle(&self, connection: &Connection, now: Instant, idle: Duration) -> bool {
        match &connection.state {
            ConnState::Draining(deadline) => now >= *deadline,
            ConnState::Reading | ConnState::Writing => {
                now.saturating_duration_since(connection.last_active) > idle
            }
            ConnState::Processing | ConnState::Stream(_) => false,
        }
    }

    fn on_close(&mut self, connection: &mut Connection) {
        // A plan is resolved per request and consumed by `serve_face`; a
        // connection that dies while waiting for its body leaves it behind.
        self.face_plans.remove(&connection.id);
        // Replacing or dropping ConnState::Stream drops the driver, which
        // delivers the handler's `Closed` event.
        if let Some(pending) = self.blocking_pending.remove(&connection.id) {
            // Seal the slot first: a job that finishes after this point writes
            // into a sealed slot and is ignored. Then free a queued slot or
            // flip a running job's cancellation token.
            pending.slot.take();
            self.blocking.cancel(connection.id);
        } else if let Some(key) = self.stream_jobs.remove(&connection.id) {
            // Same contract for a stream job: the connection is gone, so a
            // queued job is dropped and a running one learns it was cancelled.
            self.stream_job_keys.remove(&key);
            self.blocking.cancel(key);
        }
    }
}

impl TimerOps for Server {
    type State = ConnState;
    fn on_tick(&mut self, connections: &mut Table<ConnState>) {
        let mut drain = [0u8; 64];
        while self.notify.read(&mut drain).is_ok() {}
        // A UI event wake is "some handler may have output waiting", not a
        // deadline: polling every stream here is what turns a published event
        // into one loop iteration.
        let bus_wake = self.bus_wake.swap(false, Ordering::SeqCst);
        // The same wake pipe carries blocking-pool completions, control
        // replies, and outbound progress retries.
        self.control.drain_replies();
        self.control.flush();
        self.client.drain_cancels();
        self.client.tick();

        while let Ok(result) = self.results.try_recv() {
            match result {
                WorkResult::Rendered {
                    connection,
                    response,
                    keep_alive,
                } => {
                    // The pool's own BlockingDone clears stream_job_keys; this
                    // map only tracks the connection's one in-flight job.
                    self.stream_jobs.remove(&connection);
                    if let Some(connection) = connections.get_mut(&connection) {
                        write_response(connection, response, keep_alive);
                    }
                }
                WorkResult::BlockingDone { key, outcome } => {
                    let Some(pending) = self.blocking_pending.remove(&key) else {
                        // A stream handler's job reports its value through
                        // `StreamJobDone` first; this outcome only cleans the
                        // pool slot. Cancel/drop outcomes are expected after a
                        // disconnect too. Anything else lost its bookkeeping.
                        if !self.stream_job_keys.remove(&key)
                            && outcome == JobOutcome::Finished
                            && connections.contains_key(&key)
                        {
                            log::svcd(&format!(
                                "svcd: blocking completion for connection {} lost its slot",
                                key
                            ));
                        }
                        continue;
                    };
                    let exchange = pending.slot.take();
                    let Some(connection) = connections.get_mut(&key) else {
                        // The client disconnected while the job was running.
                        // The sealed slot drops a late completion.
                        continue;
                    };
                    let (response, keep_alive) = match outcome {
                        JobOutcome::Dropped => (blocking_busy(Busy::Closed), false),
                        JobOutcome::Finished | JobOutcome::Cancelled => {
                            let mut response =
                                render_exchange(complete_blocking(pending.body, exchange));
                            if pending.keep_alive {
                                response = keep_alive_response(response);
                            }
                            (response, pending.keep_alive)
                        }
                    };
                    write_response(connection, response, keep_alive);
                }
                WorkResult::StreamJobDone {
                    connection: id,
                    outcome,
                } => {
                    // The job is no longer in flight. A follow-up job submitted
                    // from service_stream_job below gets a fresh key.
                    self.stream_jobs.remove(&id);
                    let keep = match connections.get_mut(&id) {
                        Some(connection) => deliver_stream_outcome(connection, outcome),
                        None => false,
                    };
                    if keep {
                        if let Some(connection) = connections.get_mut(&id) {
                            self.service_stream_job(connection);
                        }
                    } else if let Some(mut connection) = connections.remove(&id) {
                        self.on_close(&mut connection);
                    }
                }
            }
        }

        // A blocking job leaves the connection in `Processing`, where the loop
        // does not request POLLIN. Check for a graceful peer FIN here so a
        // disconnect cancels the job instead of waiting for POLLHUP.
        let closed: Vec<u64> = connections
            .iter()
            .filter(|(_, connection)| {
                matches!(connection.state, ConnState::Processing) && peer_closed(connection)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in closed {
            if let Some(mut connection) = connections.remove(&id) {
                self.on_close(&mut connection);
            }
        }

        let now = Instant::now();
        let stream_ids: Vec<u64> = connections
            .iter()
            .filter(|(_, connection)| matches!(&connection.state, ConnState::Stream(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in stream_ids {
            let keep = match connections.get_mut(&id) {
                Some(connection) => {
                    let keep = if bus_wake {
                        stream_writable(connection)
                    } else {
                        stream_tick(connection, now)
                    };
                    if keep {
                        self.service_stream_job(connection);
                    }
                    keep
                }
                None => true,
            };
            if !keep {
                if let Some(mut connection) = connections.remove(&id) {
                    self.on_close(&mut connection);
                }
            }
        }
    }
}
