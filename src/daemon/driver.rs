use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use serde_json::{json, Value};

use crate::face::control;
use crate::kernel::call::CallId;
use crate::kernel::failure::{Problem, ProblemError};
use crate::kernel::projection::Status;
use crate::kernel::schema::{MAX_PROTOCOL, MAX_SCHEMA};
use crate::platform::clock::Clock;
use crate::platform::compat;
use crate::platform::log;
#[cfg(feature = "http")]
use crate::platform::settings;
use crate::server::auth::lan::LanStatus;
use crate::wire::control::channel;
use crate::wire::message::rpc;

use super::Daemon;

impl Daemon {
    pub(super) fn event_loop(&mut self, clock: &Clock) {
        loop {
            let disconnected = self.drain(clock);
            let now = clock.now();
            self.host.tick(now, clock);
            self.sync_catalog();
            self.shutdown |= crate::platform::host::signal::requested();
            if self.shutdown || disconnected {
                break;
            }
            // Management requests and plugin stdout wake this immediately; the
            // bounded wait is only for shutdown and timer deadlines. A timer
            // that comes due sooner shortens the wait.
            self.host.wait_for_work(self.host.next_wait(now));
        }
        self.host.stop_all(clock);
    }

    /// Handle every queued management request, then report whether the control
    /// channel closed. Blocking requests (start / stop / reload) wait on the
    /// clock, so the drain takes the clock rather than a stale snapshot.
    pub(super) fn drain(&mut self, clock: &Clock) -> bool {
        loop {
            match self.rx.try_recv() {
                Ok(incoming) => self.handle(clock, incoming),
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    pub(super) fn handle(&mut self, clock: &Clock, incoming: channel::PendingCall) {
        let started = Instant::now();
        // One request, one number, allocated before anything can fail: a
        // malformed frame and an unauthorized method are still this call, and
        // the caller needs a name for whatever went wrong (#343).
        let call = self.allocate_call();
        let request = match rpc::parse(&incoming.line) {
            Ok(request) => request,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&stamp_call(response, call)));
                return;
            }
        };
        let id = request.id.clone();
        let method = request.method.clone();
        if incoming.endpoint == channel::Endpoint::User
            && channel::Endpoint::for_method(&method) == channel::Endpoint::Admin
        {
            let response =
                rpc::err_data(id, -32000, "unauthorized", Problem::Internal.error_data());
            incoming.reply.send(rpc::to_line(&stamp_call(response, call)));
            return;
        }
        let parsed = match control::parse(request) {
            Ok(parsed) => parsed,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&stamp_call(response, call)));
                return;
            }
        };
        match parsed.command {
            control::Command::ToolCall(tool) => {
                self.handle_tool_call(clock, parsed.id, tool, incoming.reply)
            }
            command => {
                let response = self.dispatch(clock, call, parsed.id, command);
                incoming.reply.send(rpc::to_line(&stamp_call(response, call)));
            }
        }
        self.sync_catalog();
        log::note_slow(&format!("request {} call={}", method, call), started);
    }

    pub(super) fn dispatch(
        &mut self,
        clock: &Clock,
        call: CallId,
        id: Option<Value>,
        command: control::Command,
    ) -> rpc::Response {
        let now = clock.now();
        match command {
            control::Command::Ping => rpc::ok(id, json!({"ok": true})),
            control::Command::Compat => rpc::ok(
                id,
                serde_json::to_value(compat::detect(&self.config.paths, MAX_SCHEMA, MAX_PROTOCOL))
                    .unwrap_or(Value::Null),
            ),
            control::Command::Shutdown => {
                self.shutdown = true;
                rpc::ok(id, json!({"ok": true}))
            }
            control::Command::ToolsPage { cursor, limit } => {
                match self.tools_page(cursor.as_deref(), limit, now) {
                    Ok(value) => rpc::ok(id, value),
                    Err(error) => problem_response(id, -32602, error),
                }
            }
            control::Command::Version => rpc::ok(id, json!({ "version": self.catalog.version() })),
            control::Command::List => rpc::ok(id, json!({"services": self.host.statuses(now)})),
            control::Command::Status { service, probe } => {
                match self.host.status_for_id(&service, now) {
                    Some(mut status) => {
                        if probe {
                            self.host.probe_status(&service, &mut status);
                        }
                        rpc::ok(id, status_json(status))
                    }
                    None => rpc::err_data(
                        id,
                        -32001,
                        &format!("unknown service: {}", service),
                        Problem::UnknownService.error_data(),
                    ),
                }
            }
            control::Command::Start { service } => {
                status_response(id, -32003, self.host.start_service(&service, clock))
            }
            control::Command::Stop { service } => {
                status_response(id, -32004, self.host.stop_service(&service, clock))
            }
            control::Command::Restart { service } => {
                status_response(id, -32010, self.host.restart_service(&service, clock))
            }
            control::Command::Enable { service } => {
                status_response(id, -32005, self.host.set_enabled(&service, true, clock))
            }
            control::Command::Disable { service } => {
                status_response(id, -32005, self.host.set_enabled(&service, false, clock))
            }
            control::Command::Reload => match self.host.reload(clock) {
                Ok(summary) => rpc::ok(id, summary),
                Err(error) => problem_response(id, -32009, error),
            },
            control::Command::Install(install) => match self.install_plugin(install, call, clock) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => {
                    warn!("call={} install failed: {}", call, error.detail());
                    problem_response(id, -32006, error)
                }
            },
            control::Command::InstallStaged(install) => {
                match self.install_staged(install, call, clock) {
                    Ok(status) => rpc::ok(id, status_json(status)),
                    Err(error) => {
                        warn!("call={} install failed: {}", call, error.detail());
                        problem_response(id, -32006, error)
                    }
                }
            }
            control::Command::Uninstall {
                service,
                purge,
                force,
            } => match self.uninstall_plugin(&service, purge, force, clock) {
                Ok(()) => rpc::ok(id, json!({ "ok": true })),
                Err(error) => problem_response(id, -32007, error),
            },
            control::Command::AuthStatus => match self.auth_status() {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
            },
            control::Command::RestartDaemon => match self.restart_daemon() {
                Ok(value) => rpc::ok(id, value),
                Err(error) => problem_response(id, -32013, error),
            },
            control::Command::LogsClear { service } => match self.clear_logs(service) {
                Ok(value) => rpc::ok(id, value),
                Err(error) => problem_response(id, -32013, error),
            },
            control::Command::AuthPasswd { password } => match self.auth_passwd(&password) {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
            },
            // handle() routes streaming tool calls to the reply sink; a direct
            // caller has no sink, so refuse instead of dropping the call.
            control::Command::ToolCall(_) => rpc::err_data(
                id,
                -32603,
                "tools/call needs a control reply sink",
                Problem::Internal.error_data(),
            ),
        }
    }

    /// Restart the daemon after a settings save.
    ///
    /// The route is the one `svcd restart` and an upgrade take (#317): a user
    /// daemon is replaced by a detached helper, a service daemon is handed to
    /// the init system that will respawn it, and a host with neither gets the
    /// command to run by hand. The save already happened, so a helper that
    /// cannot start is a hint, not a failed request.
    pub(super) fn restart_daemon(&mut self) -> Result<serde_json::Value, ProblemError> {
        let restarted = crate::selfinstall::restart_now(None);
        if let Some(helper) = restarted.helper {
            // The helper stops this daemon itself, after a short grace period
            // that lets the HTTP response carrying `restarting: true` flush.
            // Shutting down here could kill the process before the reply
            // reaches the browser.
            return Ok(json!({"restarting": true, "helper": helper}));
        }
        let hint = restarted
            .hint
            .unwrap_or_else(crate::selfinstall::manual_hint);
        Ok(json!({"restarting": false, "hint": format!("saved; {}", hint)}))
    }

    /// Clear the daemon log or one plugin's logs. The daemon log goes through
    /// its writer because that writer holds the open handle; a plugin's service
    /// log goes through the slot for the same reason, and its hooks log is
    /// removed directly (no hook writer outlives the hook).
    pub(super) fn clear_logs(
        &mut self,
        service: Option<String>,
    ) -> Result<serde_json::Value, ProblemError> {
        let Some(id) = service else {
            let files = log::reset_svcd()
                .map_err(|error| Problem::Internal.error(format!("clear svcd.log: {}", error)))?;
            log::record(
                log::Level::Info,
                &format!("logs cleared: svcd ({} files)", files),
            );
            return Ok(json!({"cleared": "svcd", "files": files}));
        };
        if self.host.service(&id).is_none() {
            return Err(Problem::UnknownPlugin.error(format!("unknown plugin: {}", id)));
        }
        let files = self
            .host
            .clear_logs(&id)
            .map_err(|error| Problem::Internal.error(format!("clear {} logs: {}", id, error)))?;
        Ok(json!({"cleared": id, "files": files}))
    }

    #[cfg(feature = "http")]
    pub(super) fn auth_status(&self) -> Result<LanStatus, ProblemError> {
        self.api
            .as_ref()
            .ok_or_else(|| ProblemError::new(Problem::Internal, "HTTP API is not running"))?
            .status()
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))
    }

    #[cfg(not(feature = "http"))]
    pub(super) fn auth_status(&self) -> Result<LanStatus, ProblemError> {
        Err(ProblemError::new(
            Problem::Internal,
            "HTTP API is disabled in this build",
        ))
    }

    #[cfg(feature = "http")]
    pub(super) fn auth_passwd(&self, password: &str) -> Result<LanStatus, ProblemError> {
        if password.is_empty() {
            return Err(ProblemError::new(
                Problem::Internal,
                "password must not be empty",
            ));
        }
        let path =
            self.config.settings_path.as_deref().ok_or_else(|| {
                ProblemError::new(Problem::Internal, "svcd.conf path is unavailable")
            })?;
        let hash = crate::platform::auth::hash_password(password)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        settings::update_value(path, "auth_passwd", &hash)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))?;
        self.api
            .as_ref()
            .ok_or_else(|| ProblemError::new(Problem::Internal, "HTTP API is not running"))?
            .set_password(hash)
            .map_err(|detail| ProblemError::new(Problem::Internal, detail))
    }

    #[cfg(not(feature = "http"))]
    pub(super) fn auth_passwd(&self, _password: &str) -> Result<LanStatus, ProblemError> {
        Err(ProblemError::new(
            Problem::Internal,
            "HTTP API is disabled in this build",
        ))
    }
}

pub(super) fn status_response(
    id: Option<Value>,
    error_code: i32,
    result: Result<Status, ProblemError>,
) -> rpc::Response {
    match result {
        Ok(status) => match serde_json::to_value(status) {
            Ok(value) => rpc::ok(id, value),
            Err(error) => rpc::err_data(
                id,
                -32603,
                &error.to_string(),
                Problem::Internal.error_data(),
            ),
        },
        Err(error) => problem_response(id, error_code, error),
    }
}

/// One management error -> one JSON-RPC frame. Funnelling every dispatch arm
/// through here keeps the `data.kind` and the code together.
pub(super) fn problem_response(id: Option<Value>, code: i32, error: ProblemError) -> rpc::Response {
    rpc::err_data(id, code, error.detail(), error.problem.error_data())
}

/// Put the request's call number into the structured error payload.
///
/// Only errors carry it: a successful response has no request metadata, and
/// the number exists to name a failure in the log (`call=7`). The category
/// (`kind`) stays exactly where it was, so a reader that only knows the old
/// shape keeps working.
pub(super) fn stamp_call(mut response: rpc::Response, call: CallId) -> rpc::Response {
    let Some(error) = response.error.as_mut() else {
        return response;
    };
    let data = error.data.get_or_insert_with(|| json!({}));
    if let Value::Object(map) = data {
        map.insert("call".to_string(), json!(call));
    }
    response
}

pub(super) fn status_json(status: Status) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

pub(super) fn auth_json(status: &LanStatus) -> Value {
    json!({
        "bind": status.bind.to_string(),
        "requestedBind": status.requested_bind.to_string(),
        "authRequired": status.require_auth,
        "passwordSet": status.password_set,
    })
}
