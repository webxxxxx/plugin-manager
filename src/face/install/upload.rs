//! The package upload stream: route recognition, endpoint policy, body framing
//! and the sans-IO handler shared by install and import.

use std::sync::{Arc, Mutex};

use crate::face::support::{self, CallBudget};
use crate::kernel::failure::{Problem, ProblemError};
use crate::platform::config::Config;
use crate::server::auth::lan::{AuthKind, Lan};
use crate::server::http::model as http;
use crate::server::http::{
    BlockingOutcome, CloseReason, Exchange, OutBuffer, StreamAction, StreamClass, StreamEvent,
    StreamHandler, StreamStep,
};

use super::import_flow::{ImportFlow, ImportParams};
use super::install_flow::InstallFlow;

/// One recognized package upload. The query is already parsed because both
/// routes carry their endpoint policy there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageTarget {
    Install {
        strategy: String,
        enable: bool,
        allow_hooks: bool,
    },
    Import {
        config_id: Option<String>,
        no_config: bool,
        force: bool,
    },
}

/// Recognize the two streaming package routes. Only POST is claimed; every
/// other method stays on the ordinary router so GET /api/v1/import keeps its
/// 405 and an unknown plugin path keeps its 404.
pub fn target(head: &http::Head) -> Option<PackageTarget> {
    if head.method != "POST" {
        return None;
    }
    let path = head
        .path
        .split_once('?')
        .map(|(path, _)| path)
        .unwrap_or(head.path.as_str());
    let query = head
        .path
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    match path {
        "/api/v1/plugins" => Some(PackageTarget::Install {
            strategy: support::query_value(query, "strategy").unwrap_or_else(|| "replace".into()),
            enable: support::query_bool(query, "enable").unwrap_or(true),
            allow_hooks: support::query_bool(query, "allow_hooks").unwrap_or(false),
        }),
        "/api/v1/import" => Some(PackageTarget::Import {
            config_id: support::query_value(query, "config_id"),
            no_config: support::query_bool(query, "no_config").unwrap_or(false),
            force: support::query_bool(query, "force").unwrap_or(false),
        }),
        _ => None,
    }
}

/// Resolve auth and endpoint policy, validate the framing, and return the
/// stream handler. Errors are complete response bytes: the server writes them
/// and closes without reading a body.
pub fn exchange(
    head: &http::Head,
    lan: &Arc<Mutex<Lan>>,
    config: Arc<Config>,
) -> Result<Exchange, Vec<u8>> {
    let Some(target) = target(head) else {
        return Err(http::json_error(
            404,
            "not_found",
            "not a package upload route",
        ));
    };
    let auth = match lan.lock() {
        Ok(mut state) => match state.authenticate(&head.headers) {
            Ok(auth) => auth,
            Err(()) => {
                return Err(http::json_error(
                    401,
                    "unauthorized",
                    "authentication required",
                ))
            }
        },
        Err(_) => return Err(http::json_error(500, "internal", "LAN state unavailable")),
    };
    if auth == AuthKind::Cookie && !http::csrf_ok(&head.headers) {
        return Err(http::json_error(
            403,
            "forbidden",
            "cookie writes require Origin and X-Svcd-Csrf",
        ));
    }
    let limits = &config.limits;
    let framing = http::framing(&head.headers)
        .map_err(|message| http::json_error(400, "bad_request", &message))?;
    match &target {
        PackageTarget::Install { strategy, .. } => {
            let content_type = http::header(&head.headers, "content-type").unwrap_or("");
            if !content_type.starts_with("application/octet-stream") {
                return Ok(reject_after_drain(
                    http::json_error(
                        400,
                        "bad_request",
                        "Content-Type must be application/octet-stream",
                    ),
                    framing,
                    limits.max_package_bytes,
                ));
            }
            if strategy != "keep-old" && strategy != "replace" {
                return Ok(reject_after_drain(
                    http::json_error(400, "bad_request", "strategy must be keep-old or replace"),
                    framing,
                    limits.max_package_bytes,
                ));
            }
            match framing {
                http::Framing::None | http::Framing::Length(0) => {
                    return Err(http::json_error(400, "bad_request", "empty package body"))
                }
                http::Framing::Length(length) if length > limits.max_package_bytes => {
                    // #286: answer, then drain. Closing while the client is
                    // still sending a 15 MiB body makes the kernel answer with
                    // RST, and the browser shows `TypeError: Failed to fetch`
                    // instead of this 413 — the same drain the content-type
                    // and strategy refusals below already do.
                    return Ok(reject_after_drain(
                        http::json_error(413, "payload_too_large", "package is too large"),
                        framing,
                        limits.max_package_bytes,
                    ));
                }
                _ => {}
            }
        }
        PackageTarget::Import { .. } => match framing {
            http::Framing::None | http::Framing::Length(0) => {
                return Err(http::json_error(400, "bad_request", "empty import body"))
            }
            http::Framing::Length(length) if length > limits.max_package_bytes => {
                // Same drain as the install route (#286).
                return Ok(reject_after_drain(
                    http::json_error(413, "payload_too_large", "import is too large"),
                    framing,
                    limits.max_package_bytes,
                ));
            }
            _ => {}
        },
    }
    let max_package_bytes = config.limits.max_package_bytes;
    let handler = PackageUploadStream::new(
        target,
        framing,
        expects_continue(head),
        config,
        max_package_bytes,
    );
    Ok(Exchange::Stream(Box::new(handler)))
}

fn expects_continue(head: &http::Head) -> bool {
    http::header(&head.headers, "expect")
        .map(|value| value.eq_ignore_ascii_case("100-continue"))
        .unwrap_or(false)
        && head.version.eq_ignore_ascii_case("HTTP/1.1")
}

fn reject_after_drain(response: Vec<u8>, framing: http::Framing, limit: u64) -> Exchange {
    Exchange::Stream(Box::new(RejectAfterDrain {
        framing: BodyFraming::new(framing, limit),
        response,
        response_sent: 0,
        body_done: false,
    }))
}

/// An endpoint-policy refusal for a request that still has a body. The final
/// answer goes out first, then the body is consumed and discarded so the close
/// is a clean FIN instead of a RST while the client is still uploading. This
/// is the behavior the old install takeover had for a wrong content type or
/// strategy.
struct RejectAfterDrain {
    framing: BodyFraming,
    response: Vec<u8>,
    response_sent: usize,
    body_done: bool,
}

impl RejectAfterDrain {
    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        if self.response_sent < self.response.len() {
            let take = (self.response.len() - self.response_sent).min(out.remaining());
            out.write(&self.response[self.response_sent..self.response_sent + take]);
            self.response_sent += take;
            if self.response_sent < self.response.len() {
                return StreamStep::wait(StreamAction::NeedWrite);
            }
        }
        if self.body_done {
            return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
        }
        StreamStep::wait(StreamAction::NeedRead)
    }

    fn on_data(&mut self, data: &[u8]) -> StreamStep {
        match self.framing.feed(data) {
            Ok((_, done)) => {
                if done {
                    self.body_done = true;
                }
            }
            // The framing itself is broken: there is no boundary to drain to.
            Err(_) => self.body_done = true,
        }
        if self.body_done {
            if self.response_sent >= self.response.len() {
                return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
            }
            // The response is still in the output buffer; flush it first.
            return StreamStep::wait(StreamAction::NeedWrite);
        }
        StreamStep::wait(StreamAction::NeedRead)
    }
}

impl StreamHandler for RejectAfterDrain {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Data(data) => self.on_data(data),
            StreamEvent::Closed(reason) => StreamStep::wait(StreamAction::Close(reason)),
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::NeedRead),
            StreamEvent::BlockingDone(_) => StreamStep::wait(StreamAction::Continue),
        }
    }

    /// The endpoint-policy refusal is a bounded response that was already
    /// rejected before any transfer work; it must not be turned into a 503 by
    /// a full max_streams budget, and the old takeover did not count it either.
    fn stream_class(&self) -> StreamClass {
        StreamClass::OneShot
    }
}

/// One package upload as a sans-IO state machine. Response bytes are small
/// JSON; body bytes are owned by blocking jobs and never accumulate in the
/// output buffer.
pub struct PackageUploadStream {
    state: Arc<Mutex<UploadState>>,
    phase: Phase,
    expect_continue: bool,
    continue_sent: bool,
    response: Vec<u8>,
    response_sent: usize,
    /// An error response is ready, but the request body still has to be
    /// consumed so the server can close with a clean FIN instead of a RST.
    drain_after_response: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Body,
    Blocking,
    Responding,
    Draining,
    DrainingBlocking,
    Done,
}

struct UploadState {
    framing: BodyFraming,
    body_done: bool,
    /// A refusal is queued or already answered; later chunks are drained.
    refused: bool,
    flow: Flow,
    outcome: Option<Outcome>,
    config: Arc<Config>,
}

enum Outcome {
    /// The request was refused before the body ended: answer, then drain.
    Error(Vec<u8>),
    /// The body completed (successfully or not) and this is the final answer.
    Response(Vec<u8>),
}

#[allow(clippy::large_enum_variant)] // Per-upload state; boxing would add an allocation to every request.
enum Flow {
    Install(InstallFlow),
    Import(ImportFlow),
    Done,
}

enum BodyFraming {
    Length { remaining: u64 },
    Chunked(http::ChunkedDecoder),
}

impl BodyFraming {
    fn new(framing: http::Framing, limit: u64) -> BodyFraming {
        match framing {
            http::Framing::Length(remaining) => BodyFraming::Length { remaining },
            http::Framing::Chunked => BodyFraming::Chunked(http::ChunkedDecoder::new(limit)),
            http::Framing::None => BodyFraming::Length { remaining: 0 },
        }
    }

    /// Decode one raw body chunk. The second value says the body ended.
    fn feed(&mut self, raw: &[u8]) -> Result<(Vec<u8>, bool), ProblemError> {
        match self {
            BodyFraming::Length { remaining } => {
                let take = std::cmp::min(raw.len() as u64, *remaining) as usize;
                *remaining -= take as u64;
                Ok((raw[..take].to_vec(), *remaining == 0))
            }
            BodyFraming::Chunked(decoder) => {
                let mut decoded = Vec::new();
                decoder
                    .feed(raw, &mut |chunk| {
                        decoded.extend_from_slice(chunk);
                        Ok(())
                    })
                    .map_err(|error| match error {
                        http::BodyError::Malformed(detail) => Problem::Invalid.error(detail),
                        http::BodyError::TooLarge(detail) => Problem::TooLarge.error(detail),
                    })?;
                Ok((decoded, decoder.is_done()))
            }
        }
    }
}

impl PackageUploadStream {
    pub(super) fn new(
        target: PackageTarget,
        framing: http::Framing,
        expect_continue: bool,
        config: Arc<Config>,
        max_package: u64,
    ) -> PackageUploadStream {
        let flow = match target {
            PackageTarget::Install {
                strategy,
                enable,
                allow_hooks,
            } => Flow::Install(InstallFlow::new(&config, strategy, enable, allow_hooks)),
            PackageTarget::Import {
                config_id,
                no_config,
                force,
            } => Flow::Import(ImportFlow::new(
                &config,
                ImportParams {
                    config_id,
                    no_config,
                    force,
                },
            )),
        };
        PackageUploadStream {
            state: Arc::new(Mutex::new(UploadState {
                framing: BodyFraming::new(framing, max_package),
                body_done: false,
                refused: false,
                flow,
                outcome: None,
                config,
            })),
            phase: Phase::Body,
            expect_continue,
            continue_sent: false,
            response: Vec::new(),
            response_sent: 0,
            drain_after_response: false,
        }
    }

    fn respond(&mut self, response: Vec<u8>) {
        self.response = response;
        self.response_sent = 0;
    }

    fn finish_response(&mut self) -> StreamStep {
        if self.drain_after_response {
            self.drain_after_response = false;
            self.phase = Phase::Draining;
            return StreamStep::wait(StreamAction::NeedRead);
        }
        self.phase = Phase::Done;
        StreamStep::wait(StreamAction::Close(CloseReason::Completed))
    }

    fn on_writable(&mut self, out: &mut OutBuffer<'_>) -> StreamStep {
        if let Some(step) = crate::face::support::expect_continue(
            out,
            &mut self.continue_sent,
            self.expect_continue,
        ) {
            return step;
        }
        if self.response_sent < self.response.len() {
            let take = (self.response.len() - self.response_sent).min(out.remaining());
            out.write(&self.response[self.response_sent..self.response_sent + take]);
            self.response_sent += take;
            if self.response_sent == self.response.len() {
                return self.finish_response();
            }
            return StreamStep::wait(StreamAction::NeedWrite);
        }
        if self.phase == Phase::Draining || self.phase == Phase::DrainingBlocking {
            return StreamStep::wait(StreamAction::NeedRead);
        }
        if !self.response.is_empty() {
            return self.finish_response();
        }
        StreamStep::wait(StreamAction::NeedRead)
    }

    fn on_data(&mut self, data: &[u8]) -> StreamStep {
        if data.is_empty() {
            return StreamStep::wait(StreamAction::NeedRead);
        }
        match self.phase {
            Phase::Body => self.phase = Phase::Blocking,
            Phase::Draining => self.phase = Phase::DrainingBlocking,
            _ => return StreamStep::wait(StreamAction::NeedRead),
        }
        let chunk = data.to_vec();
        let state = self.state.clone();
        StreamStep::wait(StreamAction::SubmitBlocking(Box::new(move |budget| {
            run_chunk(&state, &chunk, &budget)
        })))
    }

    fn on_blocking_done(
        &mut self,
        outcome: BlockingOutcome,
        out: &mut OutBuffer<'_>,
    ) -> StreamStep {
        match outcome {
            BlockingOutcome::Busy => {
                if self.phase == Phase::DrainingBlocking {
                    self.phase = Phase::Done;
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                }
                // A full pool refuses the chunk before a final answer: use the
                // same 503 busy contract as a stream cap.
                self.respond(crate::server::http::stream_busy_response());
                self.phase = Phase::Responding;
                self.on_writable(out)
            }
            BlockingOutcome::Failed { status, message } => {
                if self.phase == Phase::DrainingBlocking {
                    self.phase = Phase::Done;
                    return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                }
                self.respond(http::json_error(
                    status,
                    crate::server::http::model::code_for(status),
                    &message,
                ));
                self.phase = Phase::Responding;
                self.on_writable(out)
            }
            BlockingOutcome::Ok(_) => {
                let (outcome, body_done) = {
                    let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                    (state.outcome.take(), state.body_done)
                };
                if let Some(outcome) = outcome {
                    match outcome {
                        Outcome::Error(response) => {
                            self.respond(response);
                            self.drain_after_response = !body_done;
                        }
                        Outcome::Response(response) => {
                            self.respond(response);
                            self.drain_after_response = false;
                        }
                    }
                    self.phase = Phase::Responding;
                    return self.on_writable(out);
                }
                if body_done {
                    if self.phase == Phase::DrainingBlocking {
                        self.phase = Phase::Done;
                        return StreamStep::wait(StreamAction::Close(CloseReason::Completed));
                    }
                    // The body finished without a final answer; this is a
                    // handler bug, not a client error. Answer instead of
                    // waiting for bytes that will never arrive.
                    self.respond(http::json_error(
                        500,
                        "internal",
                        "package upload ended without a response",
                    ));
                    self.phase = Phase::Responding;
                    return self.on_writable(out);
                }
                self.phase = if self.phase == Phase::DrainingBlocking {
                    Phase::Draining
                } else {
                    Phase::Body
                };
                StreamStep::wait(StreamAction::NeedRead)
            }
        }
    }
}

impl StreamHandler for PackageUploadStream {
    fn on_event(&mut self, event: StreamEvent<'_>, out: &mut OutBuffer<'_>) -> StreamStep {
        match event {
            StreamEvent::Writable => self.on_writable(out),
            StreamEvent::Data(data) => self.on_data(data),
            StreamEvent::BlockingDone(outcome) => self.on_blocking_done(outcome, out),
            // The server's own no-progress bound closes a stalled body; a
            // poll tick must not abort an upload that is still arriving.
            StreamEvent::Tick(_) => StreamStep::wait(StreamAction::NeedRead),
            StreamEvent::Closed(reason) => {
                self.phase = Phase::Done;
                self.response.clear();
                self.response_sent = 0;
                StreamStep::wait(StreamAction::Close(reason))
            }
        }
    }

    fn stream_class(&self) -> StreamClass {
        StreamClass::LongLived
    }
}

fn run_chunk(
    state: &Arc<Mutex<UploadState>>,
    chunk: &[u8],
    job_budget: &crate::server::http::JobBudget,
) -> BlockingOutcome {
    let budget = CallBudget::from_job(job_budget);
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    state.feed(chunk, &budget);
    BlockingOutcome::Ok(Vec::new())
}

impl UploadState {
    fn feed(&mut self, raw: &[u8], budget: &CallBudget) {
        let (decoded, done) = match self.framing.feed(raw) {
            Ok(value) => value,
            Err(error) => {
                // A broken chunk framing cannot be drained: there is no
                // boundary to wait for. If a refusal is already queued, keep
                // it: replacing it would write a second response head.
                if !self.refused {
                    self.fail(&error);
                }
                self.body_done = true;
                return;
            }
        };
        if self.refused || self.outcome.is_some() {
            // Draining the rest of a refused request: decode the framing and
            // discard, so the close is a clean FIN.
            if done {
                self.body_done = true;
            }
            return;
        }
        if !decoded.is_empty() {
            let result = match &mut self.flow {
                Flow::Install(flow) => flow.feed(&decoded),
                Flow::Import(flow) => flow.feed(&decoded),
                Flow::Done => Ok(()),
            };
            if let Err(error) = result {
                self.fail(&error);
            }
        }
        if done {
            self.body_done = true;
            if self.outcome.is_none() && !self.refused {
                self.finish(budget);
            }
        }
    }

    fn fail(&mut self, error: &ProblemError) {
        let response = support::problem_response(error);
        self.flow = Flow::Done;
        self.refused = true;
        self.outcome = Some(Outcome::Error(response));
    }

    fn finish(&mut self, budget: &CallBudget) {
        let config = self.config.clone();
        let flow = std::mem::replace(&mut self.flow, Flow::Done);
        let response = match flow {
            Flow::Install(mut flow) => flow.finish(&config, budget),
            Flow::Import(mut flow) => flow.finish(&config, budget),
            Flow::Done => return,
        };
        self.outcome = Some(Outcome::Response(response));
    }
}
