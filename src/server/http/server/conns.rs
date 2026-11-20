use super::*;

impl ExtraOps for Server {
    fn wake_fd(&self) -> Option<RawFd> {
        Some(self.notify.as_raw_fd())
    }

    fn prepare_extra(&mut self, out: &mut Vec<(RawFd, i16)>) {
        self.extra_keys.clear();
        let mut control_keys = Vec::new();
        self.control.poll_sources(out, &mut control_keys);
        for key in control_keys {
            self.extra_keys.push(ExtraSource::Control(key));
        }
        let mut client_keys = Vec::new();
        self.client.poll_sources(out, &mut client_keys);
        for id in client_keys {
            self.extra_keys.push(ExtraSource::Client(id));
        }
    }

    fn on_extra_ready(&mut self, index: usize, revents: i16) {
        match self.extra_keys.get(index).copied() {
            Some(ExtraSource::Control(source)) => self.control.ready(source, revents),
            Some(ExtraSource::Client(id)) => self.client.ready(id, revents),
            None => {}
        }
    }
}

impl LifecycleOps for Server {
    fn should_stop(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn on_stop(&mut self) {
        self.control.drain_for_shutdown();
        self.client.close();
    }
}

impl Server {
    /// Run one server-owned buffered route on the blocking pool. The closure
    /// is the composition root's leftover router; the loop only writes the
    /// rendered bytes back.
    pub(super) fn submit_rendered(
        &mut self,
        connection: &mut Connection,
        request: HttpRequest,
        keep_alive: bool,
    ) {
        let id = connection.id;
        let key = self.next_job_key();
        let submitted = submit_rendered_job(
            &self.blocking,
            id,
            key,
            self.fallback.clone(),
            self.lan.clone(),
            self.results_tx.clone(),
            self.notify.try_clone().ok(),
            request,
            keep_alive,
        );
        match submitted {
            Ok(()) => {
                debug_assert!(
                    !self.stream_jobs.contains_key(&id),
                    "one in-flight blocking job per connection"
                );
                self.stream_jobs.insert(id, key);
                self.stream_job_keys.insert(key);
                connection.state = ConnState::Processing;
            }
            Err(busy) => write_response(connection, blocking_busy(busy), false),
        }
    }

    /// Allocate one detached blocking-pool key. Face jobs and server-owned
    /// routes share the range so a pool completion can never collide with a
    /// connection id.
    pub(super) fn next_job_key(&mut self) -> u64 {
        let key = self.next_stream_job;
        self.next_stream_job = self.next_stream_job.wrapping_add(1).max(1 << 32);
        key
    }

    /// Hand the stream handler's one pending job to the blocking pool. The
    /// job's value comes back as `StreamJobDone`; the pool's own completion
    /// only frees the key. A full queue is reported to the handler, which can
    /// answer with a protocol error instead of dropping the connection.
    pub(super) fn service_stream_job(&mut self, connection: &mut Connection) {
        let id = connection.id;
        let job = match &mut connection.state {
            ConnState::Stream(driver) => driver.take_job(),
            _ => return,
        };
        let Some(job) = job else {
            return;
        };
        let key = self.next_job_key();
        let results = self.results_tx.clone();
        let notify = self.notify.try_clone().ok();
        // Stamp the deadline at submission, the same instant the driver starts
        // its no-progress clock. A job that waits in the queue must not get a
        // fresh budget after the connection has already been given up on.
        let deadline = Instant::now() + self.config.limits.transfer_timeout;
        let submitted = self.blocking.try_submit(key, move |cancel| {
            if cancel.is_cancelled() {
                return;
            }
            let outcome = job(JobBudget::at(cancel, deadline));
            let _ = results.send(WorkResult::StreamJobDone {
                connection: id,
                outcome,
            });
            if let Some(notify) = &notify {
                let _ = (&*notify).write(&[1]);
            }
        });
        match submitted {
            Ok(()) => {
                debug_assert!(
                    !self.stream_jobs.contains_key(&id),
                    "one in-flight blocking job per connection"
                );
                self.stream_jobs.insert(id, key);
                self.stream_job_keys.insert(key);
            }
            Err(_) => {
                if let ConnState::Stream(driver) = &mut connection.state {
                    driver.on_blocking_done(
                        BlockingOutcome::Busy,
                        &mut connection.out,
                        &mut connection.out_at,
                    );
                }
            }
        }
    }

    /// Run the face-owned part of one request from the plan resolved before the
    /// body was read. Inline routes answer now; blocking routes are submitted to
    /// the pool and the connection waits in `Processing`.
    pub(super) fn serve_face(
        &mut self,
        connection: &mut Connection,
        prepared: PreparedFace,
        body: Vec<u8>,
        keep_alive: bool,
    ) {
        let prepared = match attach_face_body(prepared, &body) {
            Ok(prepared) => prepared,
            Err(response) => {
                // The wire limit should have caught this before the body was
                // read. A late mismatch must not keep the connection alive,
                // because unread body bytes would precede the next request.
                write_response(connection, response, false);
                return;
            }
        };

        match prepared.route.dispatch {
            Dispatch::Inline => {
                match dispatch_inline(self.face.as_ref(), &prepared.route, prepared.request) {
                    Ok(Exchange::Response(response)) => {
                        let response = if keep_alive {
                            keep_alive_response(response.into_bytes())
                        } else {
                            response.into_bytes()
                        };
                        write_response(connection, response, keep_alive);
                    }
                    Ok(Exchange::Stream(handler)) => {
                        // A streamed response is not a keep-alive response: its
                        // handler may own a file range and the wire head says
                        // `Connection: close`.
                        attach_stream(
                            connection,
                            handler,
                            &[],
                            None,
                            &self.budget,
                            &self.config.limits,
                        );
                        self.service_stream_job(connection);
                    }
                    Err(cause) => {
                        let response = http::json_error(500, "internal", &cause.message());
                        write_response(
                            connection,
                            if keep_alive {
                                keep_alive_response(response)
                            } else {
                                response
                            },
                            keep_alive,
                        );
                    }
                }
            }
            Dispatch::Blocking => {
                let endpoint = format!("{} {}", prepared.request.method, prepared.request.path);
                let (slot, done) = CompletionSlot::new(endpoint);
                let face = self.face.clone();
                let face_request = prepared.request;
                let control_timeout = self.config.limits.control_timeout;
                let job = move |cancel: CancelToken| {
                    if cancel.is_cancelled() {
                        return;
                    }
                    // Stamp the budget when the job actually starts, not when
                    // it was queued: time spent behind another job is not part
                    // of this route's control round trip.
                    let request = face_request.with_control_budget(
                        Instant::now(),
                        control_timeout,
                        Some(cancel),
                    );
                    face.handle(request, done);
                };
                match self.blocking.try_submit(connection.id, job) {
                    Ok(()) => {
                        self.blocking_pending.insert(
                            connection.id,
                            PendingBlocking {
                                slot,
                                body: prepared.route.body,
                                keep_alive,
                            },
                        );
                        connection.state = ConnState::Processing;
                    }
                    Err(busy) => {
                        // Pipelining is unsupported, so the loop normally
                        // cannot submit a second job for one connection. The
                        // pool still enforces one-per-connection for future
                        // engines and stream handlers.
                        write_response(connection, blocking_busy(busy), false);
                    }
                }
            }
        }
    }
}
