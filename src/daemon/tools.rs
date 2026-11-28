use serde_json::{json, Value};

use crate::face::catalog;
use crate::face::control;
use crate::face::mcp_tools;
use crate::kernel::call::CallId;
use crate::kernel::contract::{CapabilityName, Invocation, Reply, ReplySink};
use crate::kernel::failure::{Failure, Problem, ProblemError};
use crate::kernel::registry::Kind;
use crate::platform::clock::{Clock, Now};
use crate::supervise::ops::invoke::{self, Outcome};
use crate::wire::message::rpc;

use super::driver::status_json;
use super::Daemon;

impl Daemon {
    pub(super) fn tool_list(&self, now: Now) -> Vec<Value> {
        mcp_tools::tools(&self.host.registry, |id| self.host.is_live(id, now))
    }

    /// Invalidate the cached tool projection. The cache is keyed by the
    /// catalog version so a page is always cut from one stable snapshot.
    /// The supervisor reports a moved projection; the cache lives here.
    pub(super) fn sync_catalog(&mut self) {
        if self.host.take_catalog_dirty() {
            self.bump_catalog();
        }
    }

    pub(super) fn bump_catalog(&mut self) {
        self.catalog.bump();
    }

    pub(super) fn tool_list_cached(&mut self, now: Now) -> &[Value] {
        if !self.catalog.is_fresh() {
            let tools = self.tool_list(now);
            self.catalog.store(tools);
        }
        self.catalog.cached()
    }

    pub(super) fn tools_page(
        &mut self,
        cursor: Option<&str>,
        limit: Option<usize>,
        now: Now,
    ) -> Result<Value, ProblemError> {
        let version = self.catalog.version();
        let page =
            catalog::page(self.tool_list_cached(now), version, cursor, limit).map_err(|error| {
                let problem = match error {
                    crate::face::catalog::PageError::InvalidCursor => Problem::Invalid,
                    // Both today fall through to 500; keep that projection.
                    crate::face::catalog::PageError::VersionMismatch
                    | crate::face::catalog::PageError::PastEnd => Problem::Internal,
                };
                ProblemError::new(problem, error.to_string())
            })?;
        // The control channel carries a face-neutral page; the MCP face renders
        // the protocol field `nextCursor` (ADR-0025: protocol projection is the
        // face's job). Keep `next` neutral here so the composition root never
        // spells an MCP key.
        let mut result = json!({ "tools": page.tools });
        if let Some(cursor) = page.next_cursor {
            result["next"] = json!(cursor);
        }
        Ok(result)
    }

    /// Handle tools/call. Plugin calls are deferred: the plugin response arrives
    /// on the slot pump, so the control connection stays open without blocking
    /// the daemon's event loop. A start tool is synchronous because starting is
    /// defined as synchronous in the service contract.
    pub(super) fn handle_tool_call(
        &mut self,
        clock: &Clock,
        call: CallId,
        id: Option<Value>,
        tool: control::ToolCall,
        reply: crate::wire::control::channel::Reply,
    ) {
        let name = tool.name;
        let mut arguments = tool.arguments;

        // Only a known plugin's <id>.start is the lifecycle tool. A capability
        // named "foo.start" falls through to the capability lookup below.
        let start_id = name.strip_suffix(".start").filter(|service_id| {
            self.host
                .registry
                .get(service_id)
                .map(|service| {
                    service.kind == Kind::Plugin || !service.contract.surface().is_empty()
                })
                .unwrap_or(false)
        });
        if let Some(service_id) = start_id {
            let response = match self.host.start_service(service_id, clock) {
                Ok(status) => {
                    mcp_tools::tool_result(id, status_json(status).to_string(), false, call)
                }
                Err(error) => mcp_tools::tool_error(id, error.detail(), call),
            };
            send(&reply, response);
            return;
        }

        let Some((service_id, capability)) = name.split_once('.') else {
            send(&reply, unknown_tool(&id, &name, call));
            return;
        };
        let Some(service) = self.host.service(service_id) else {
            send(&reply, unknown_tool(&id, &name, call));
            return;
        };
        // A service is callable when its contract declares the capability and
        // the assembly point knows how to reach it. `svc` is builtin and has no
        // supervised process; everything else keeps the SlotWire path.
        let builtin = service.impl_ref.clone();
        let known = service
            .contract
            .capability(&CapabilityName::new(capability))
            .is_some();
        let callable = builtin.is_some()
            || service.kind == Kind::Plugin
            || self.host.slots.contains_key(service_id);
        if !known || !callable {
            send(&reply, unknown_tool(&id, &name, call));
            return;
        }
        if builtin.is_some() && !self.host.is_live(service_id, clock.now()) {
            send(
                &reply,
                mcp_tools::tool_result(
                    id,
                    format!("service {} is not running", service_id),
                    true,
                    call,
                ),
            );
            return;
        }
        let service = service.clone();

        if let Err(message) =
            crate::platform::storage::resolve_args(&self.config.paths, &mut arguments)
        {
            send(
                &reply,
                mcp_tools::tool_error(
                    id,
                    &format!("cannot resolve blob argument: {}", message),
                    call,
                ),
            );
            return;
        }
        let progress_token = tool.progress;
        let mut invocation =
            Invocation::new(capability, arguments, self.config.limits.default_timeout);
        invocation.progress = progress_token;

        let builtin_done = projected_done(
            reply.clone(),
            id.clone(),
            call,
            mcp_tools::project_builtin_outcome,
        );
        let slot_done = projected_done(
            reply.clone(),
            id.clone(),
            call,
            mcp_tools::project_slot_outcome,
        );
        match invoke::call(
            &mut self.host,
            &service,
            invocation,
            &reply,
            builtin_done,
            slot_done,
        ) {
            Outcome::Started => {}
            Outcome::NoRuntime => send(
                &reply,
                mcp_tools::tool_result(
                    id,
                    format!("service {} has no runtime", service_id),
                    true,
                    call,
                ),
            ),
            Outcome::BuiltinFailed(failure) => {
                reply.send(mcp_tools::project_builtin_outcome(id, Err(failure), call))
            }
            Outcome::SlotFailed(failure) => {
                reply.send(mcp_tools::project_slot_outcome(id, Err(failure), call))
            }
            Outcome::Rejected(rejection) => reply.send(rpc::to_line(
                &mcp_tools::project_rejection(id, rejection, call),
            )),
        }
    }
}

/// Build the completion sink for one tools/call; the final value/failure is
/// projected by the face.
pub(super) fn projected_done(
    reply: crate::wire::control::channel::Reply,
    external_id: Option<Value>,
    call: CallId,
    project: fn(Option<Value>, Result<Value, Failure>, CallId) -> String,
) -> ReplySink {
    Box::new(move |outcome| {
        let outcome = match outcome {
            Ok(Reply::Value(value)) => Ok(value),
            Err(failure) => Err(failure),
        };
        reply.send(project(external_id, outcome, call));
    })
}

fn send(reply: &crate::wire::control::channel::Reply, response: rpc::Response) {
    reply.send(rpc::to_line(&response));
}

fn unknown_tool(id: &Option<Value>, name: &str, call: CallId) -> rpc::Response {
    mcp_tools::named_error(
        rpc::err(id.clone(), -32602, &format!("unknown tool: {}", name)),
        call,
    )
}
