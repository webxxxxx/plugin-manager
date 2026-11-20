use std::sync::mpsc::TryRecvError;
use std::time::Instant;

use serde_json::{json, Value};

use crate::face::control;
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
        let request = match rpc::parse(&incoming.line) {
            Ok(request) => request,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&response));
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
            incoming.reply.send(rpc::to_line(&response));
            return;
        }
        let call = match control::parse(request) {
            Ok(call) => call,
            Err(response) => {
                incoming.reply.send(rpc::to_line(&response));
                return;
            }
        };
        match call.command {
            control::Command::ToolCall(tool) => {
                self.handle_tool_call(clock, call.id, tool, incoming.reply)
            }
            command => {
                let response = self.dispatch(clock, call.id, command);
                incoming.reply.send(rpc::to_line(&response));
            }
        }
        self.sync_catalog();
        log::note_slow(&format!("request {}", method), started);
    }

    pub(super) fn dispatch(
        &mut self,
        clock: &Clock,
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
            control::Command::Install(install) => match self.install_plugin(install, clock) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            control::Command::InstallStaged(install) => match self.install_staged(install, clock) {
                Ok(status) => rpc::ok(id, status_json(status)),
                Err(error) => problem_response(id, -32006, error),
            },
            control::Command::Uninstall { service, purge } => {
                match self.uninstall_plugin(&service, purge, clock) {
                    Ok(()) => rpc::ok(id, json!({ "ok": true })),
                    Err(error) => problem_response(id, -32007, error),
                }
            }
            control::Command::AuthStatus => match self.auth_status() {
                Ok(status) => rpc::ok(id, auth_json(&status)),
                Err(error) => problem_response(id, -32008, error),
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
