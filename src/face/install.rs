//! /api/v1/plugins and /api/v1/import as sans-IO streams.
//!
//! Both endpoints used to take the connection out of the server loop and spawn
//! a per-request thread. Now the handler receives body chunks, feeds them into
//! a staging installer (or a bounded config buffer) through the blocking pool,
//! and answers once. A slow or disconnected client pauses input while one chunk
//! is in flight, so staging never grows an unbounded queue.
//!
//! The install semantics live where they already did: package::StagingInstaller
//! validates the manifest, the signature, files[] and the staged payload; this
//! face only decides the route, the framing and the response shape.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::http;
use crate::kernel::failure::{Problem, ProblemError};
use crate::lan::{AuthKind, Lan};
use crate::limits;
use crate::package;
use crate::paths;
use crate::plugin;
use crate::server::http::{
    BlockingOutcome, CloseReason, Exchange, OutBuffer, StreamAction, StreamClass, StreamEvent,
    StreamHandler, StreamStep,
};
use crate::settings;
use crate::transfer;

use super::support::{self, CallBudget};

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
            strategy: support::query_value(query, "strategy")
                .unwrap_or_else(|| "replace".into()),
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
    config: PathBuf,
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
    let limits = limits::current();
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
                    return Err(http::json_error(
                        413,
                        "payload_too_large",
                        "package is too large",
                    ))
                }
                _ => {}
            }
        }
        PackageTarget::Import { .. } => match framing {
            http::Framing::None | http::Framing::Length(0) => {
                return Err(http::json_error(400, "bad_request", "empty import body"))
            }
            http::Framing::Length(length) if length > limits.max_package_bytes => {
                return Err(http::json_error(
                    413,
                    "payload_too_large",
                    "import is too large",
                ))
            }
            _ => {}
        },
    }
    let handler = PackageUploadStream::new(
        target,
        framing,
        expects_continue(head),
        config,
        limits.max_package_bytes,
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
    config: PathBuf,
}

enum Outcome {
    /// The request was refused before the body ended: answer, then drain.
    Error(Vec<u8>),
    /// The body completed (successfully or not) and this is the final answer.
    Response(Vec<u8>),
}

enum Flow {
    Install(InstallFlow),
    Import(ImportFlow),
    Done,
}

struct InstallFlow {
    strategy: String,
    enable: bool,
    allow_hooks: bool,
    installer: Option<package::StagingInstaller>,
}

struct ImportFlow {
    params: ImportParams,
    stage: ImportStage,
}

struct ImportParams {
    config_id: Option<String>,
    no_config: bool,
    force: bool,
}

enum ImportStage {
    /// Fewer than 512 bytes have arrived; the first tar header decides between
    /// a package, a backup and a bare config.
    Classify(Vec<u8>),
    Package {
        installer: package::StagingInstaller,
    },
    Backup(BackupImport),
    Raw(Vec<u8>),
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
}

impl PackageUploadStream {
    fn new(
        target: PackageTarget,
        framing: http::Framing,
        expect_continue: bool,
        config: PathBuf,
        max_package: u64,
    ) -> PackageUploadStream {
        let flow = match target {
            PackageTarget::Install {
                strategy,
                enable,
                allow_hooks,
            } => Flow::Install(InstallFlow {
                strategy,
                enable,
                allow_hooks,
                installer: None,
            }),
            PackageTarget::Import {
                config_id,
                no_config,
                force,
            } => Flow::Import(ImportFlow {
                params: ImportParams {
                    config_id,
                    no_config,
                    force,
                },
                stage: ImportStage::Classify(Vec::new()),
            }),
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
        if !self.continue_sent && self.expect_continue {
            if out.write(b"HTTP/1.1 100 Continue\r\n\r\n") {
                self.continue_sent = true;
                return StreamStep::wait(StreamAction::NeedRead);
            }
            return StreamStep::wait(StreamAction::NeedWrite);
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
                self.respond(http::json_error(status, crate::http::code_for(status), &message));
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

impl BodyFraming {
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

impl InstallFlow {
    fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        self.installer()?.feed(data)
    }

    fn installer(&mut self) -> Result<&mut package::StagingInstaller, ProblemError> {
        if self.installer.is_none() {
            self.installer = Some(package::StagingInstaller::new(
                staging_path(),
                settings::allow_insecure_install(),
                limits::current().max_package_bytes,
            ));
        }
        Ok(self
            .installer
            .as_mut()
            .expect("staging installer initialized"))
    }

    fn finish(&mut self, config: &Path, budget: &CallBudget) -> Vec<u8> {
        let Some(mut installer) = self.installer.take() else {
            return support::problem_response(&Problem::Invalid.error("manifest.json is missing"));
        };
        let staged = match installer.finish() {
            Ok(staged) => staged,
            Err(error) => return support::problem_response(&error),
        };
        let root = staged.root.clone();
        let id = staged.manifest.name.clone();
        let version = staged.manifest.version.clone();
        match install_staged(
            config,
            &root,
            &id,
            &version,
            &self.strategy,
            self.allow_hooks,
            budget,
        ) {
            Ok(status) => {
                if !self.enable {
                    let _ = support::rpc(
                        config,
                        "svc.disable",
                        json!({ "id": id }),
                        &budget.follow_up(),
                    );
                }
                let summary = status.get("summary").cloned().unwrap_or(Value::Null);
                let body = serde_json::to_vec(&json!({
                    "id": summary.get("id").cloned().unwrap_or(Value::Null),
                    "version": summary.get("version").cloned().unwrap_or(Value::Null),
                    "state": summary.get("state").cloned().unwrap_or(Value::Null),
                }))
                .unwrap_or_default();
                http::response(200, "application/json", &body)
            }
            Err(response) => response,
        }
    }
}

impl ImportFlow {
    fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        match &mut self.stage {
            ImportStage::Classify(prefix) => {
                prefix.extend_from_slice(data);
                if prefix.len() >= 512 {
                    let data = std::mem::take(prefix);
                    self.classify(data)?;
                }
                Ok(())
            }
            ImportStage::Package { installer } => installer.feed(data),
            ImportStage::Backup(import) => import.feed(data),
            ImportStage::Raw(config) => {
                config.extend_from_slice(data);
                if config.len() as u64 > limits::current().max_body {
                    return Err(Problem::TooLarge.error("import is too large"));
                }
                Ok(())
            }
        }
    }

    fn classify(&mut self, data: Vec<u8>) -> Result<(), ProblemError> {
        let first = package::tar_header_path(&data).ok().flatten();
        match first.as_deref() {
            Some("manifest.json") => {
                let mut installer = package::StagingInstaller::new(
                    staging_path(),
                    settings::allow_insecure_install(),
                    limits::current().max_package_bytes,
                );
                installer.feed(&data)?;
                self.stage = ImportStage::Package { installer };
            }
            Some("backup.json") => {
                let mut import = BackupImport::new(&self.params, staging_path());
                import.feed(&data)?;
                self.stage = ImportStage::Backup(import);
            }
            _ => {
                if data.len() as u64 > limits::current().max_body {
                    return Err(Problem::TooLarge.error("import is too large"));
                }
                self.stage = ImportStage::Raw(data);
            }
        }
        Ok(())
    }

    fn finish(&mut self, config: &Path, budget: &CallBudget) -> Vec<u8> {
        loop {
            let stage = std::mem::replace(&mut self.stage, ImportStage::Raw(Vec::new()));
            match stage {
                ImportStage::Classify(prefix) => {
                    if let Err(error) = self.classify(prefix) {
                        return support::problem_response(&error);
                    }
                    continue;
                }
                ImportStage::Package { mut installer } => {
                    let staged = match installer.finish() {
                        Ok(staged) => staged,
                        Err(error) => return support::problem_response(&error),
                    };
                    let root = staged.root.clone();
                    return match install_staged(
                        config,
                        &root,
                        &staged.manifest.name,
                        &staged.manifest.version,
                        "replace",
                        false,
                        budget,
                    ) {
                        Ok(status) => import_response(status, None, false, Vec::new()),
                        Err(response) => response,
                    };
                }
                ImportStage::Backup(mut import) => {
                    return match import.finish(config, budget) {
                        Ok(response) => response,
                        Err(error) => support::problem_response(&error),
                    }
                }
                ImportStage::Raw(data) => return raw_config_response(&data, &self.params),
            }
        }
    }
}

fn raw_config_response(data: &[u8], params: &ImportParams) -> Vec<u8> {
    match transfer::import(
        data,
        params.config_id.as_deref(),
        params.no_config,
        params.force,
        false,
    ) {
        Ok(outcome) => import_outcome_json(outcome),
        Err(transfer::Error::Usage(message)) => {
            support::problem_response(&Problem::Invalid.error(message))
        }
        Err(transfer::Error::Failed(error)) => support::problem_response(&error),
    }
}

/// A .spkb backup: backup.json, the nested .spk, and an optional config entry.
/// The outer tar and the inner package are both parsed as they arrive; the
/// inner package reuses package::StagingInstaller.
///
/// transfer::import_backup is the buffered twin used by the CLI and the legacy
/// whole-body import. The rules (schema, id/version match, config hash,
/// force/warning policy, config application) must stay in step; a change here
/// belongs there too.
struct BackupImport {
    tar: package::TarFeed,
    sink: BackupSink,
}

impl BackupImport {
    fn new(params: &ImportParams, staging: PathBuf) -> BackupImport {
        let max = limits::current().max_package_bytes;
        BackupImport {
            tar: package::TarFeed::new(max),
            sink: BackupSink::new(params, staging),
        }
    }

    fn feed(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        self.tar.feed(data, &mut self.sink)
    }

    fn finish(&mut self, config: &Path, budget: &CallBudget) -> Result<Vec<u8>, ProblemError> {
        self.tar.finish()?;
        self.sink.finish(config, budget)
    }
}

struct BackupSink {
    staging: PathBuf,
    allow_insecure: bool,
    no_config: bool,
    force: bool,
    backup: Option<plugin::Backup>,
    installer: Option<package::StagingInstaller>,
    staged: Option<package::StagedPackage>,
    config_data: Option<Vec<u8>>,
    warnings: Vec<String>,
    stage: BackupStage,
    current: Option<BackupCurrent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackupStage {
    ExpectBackup,
    ExpectSpk,
    ExpectConfig,
    AfterConfig,
}

enum BackupCurrent {
    Backup(Vec<u8>),
    Spk,
    Config(Vec<u8>),
}

impl BackupSink {
    fn new(params: &ImportParams, staging: PathBuf) -> BackupSink {
        BackupSink {
            staging,
            allow_insecure: settings::allow_insecure_install(),
            no_config: params.no_config,
            force: params.force,
            backup: None,
            installer: None,
            staged: None,
            config_data: None,
            warnings: Vec::new(),
            stage: BackupStage::ExpectBackup,
            current: None,
        }
    }
}

impl package::TarSink for BackupSink {
    fn entry_start(&mut self, entry: &package::StreamEntry) -> Result<(), ProblemError> {
        match self.stage {
            BackupStage::ExpectBackup => {
                if entry.path != "backup.json" {
                    return Err(Problem::Invalid.error("backup.json must be the first entry"));
                }
                if entry.size > 64 * 1024 {
                    return Err(Problem::TooLarge.error("backup.json is too large"));
                }
                self.current = Some(BackupCurrent::Backup(Vec::with_capacity(
                    entry.size as usize,
                )));
            }
            BackupStage::ExpectSpk => {
                if entry.path != "plugin.spk" {
                    return Err(Problem::Invalid.error("plugin.spk must be the second entry"));
                }
                if entry.kind != b'0' {
                    return Err(Problem::Invalid.error(format!(
                        "{} is not a regular file",
                        entry.path
                    )));
                }
                self.installer = Some(package::StagingInstaller::new(
                    self.staging.clone(),
                    self.allow_insecure,
                    limits::current().max_package_bytes,
                ));
                self.current = Some(BackupCurrent::Spk);
            }
            BackupStage::ExpectConfig => {
                if entry.path != "config" {
                    return Err(Problem::Invalid.error(format!(
                        "unexpected entry: {}",
                        entry.path
                    )));
                }
                if entry.size > 64 * 1024 {
                    return Err(Problem::TooLarge.error("config is too large"));
                }
                self.current = Some(BackupCurrent::Config(Vec::with_capacity(
                    entry.size as usize,
                )));
                self.stage = BackupStage::AfterConfig;
            }
            BackupStage::AfterConfig => {
                return Err(Problem::Invalid.error("unexpected entries after config"));
            }
        }
        Ok(())
    }

    fn entry_data(&mut self, data: &[u8]) -> Result<(), ProblemError> {
        match self.current.as_mut() {
            Some(BackupCurrent::Backup(buffer)) | Some(BackupCurrent::Config(buffer)) => {
                buffer.extend_from_slice(data);
                Ok(())
            }
            Some(BackupCurrent::Spk) => self
                .installer
                .as_mut()
                .expect("package installer initialized")
                .feed(data),
            None => Err(Problem::Invalid.error("tar entry data without a header")),
        }
    }

    fn entry_end(&mut self) -> Result<(), ProblemError> {
        let Some(current) = self.current.take() else {
            return Err(Problem::Invalid.error("tar entry ended without a header"));
        };
        match current {
            BackupCurrent::Backup(bytes) => {
                let backup: plugin::Backup = serde_json::from_slice(&bytes).map_err(|error| {
                    Problem::Invalid.error(format!("invalid backup.json: {}", error))
                })?;
                if backup.schema != 1 {
                    return Err(Problem::Invalid.error(format!(
                        "unsupported backup schema {}",
                        backup.schema
                    )));
                }
                self.backup = Some(backup);
                self.stage = BackupStage::ExpectSpk;
                Ok(())
            }
            BackupCurrent::Spk => {
                let mut installer = self
                    .installer
                    .take()
                    .ok_or_else(|| Problem::Invalid.error("plugin.spk is missing"))?;
                let staged = installer.finish()?;
                // A later config check may fail; keep the staged tree in
                // self.staged so the sink's Drop removes it.
                self.staged = Some(staged);
                let staged = self.staged.as_ref().expect("staged package");
                let Some(backup) = self.backup.as_ref() else {
                    return Err(Problem::Invalid.error("backup.json is missing"));
                };
                if staged.manifest.name != backup.id {
                    return Err(Problem::Invalid.error(format!(
                        "backup id {} does not match package id {}",
                        backup.id, staged.manifest.name
                    )));
                }
                if staged.manifest.version != backup.version {
                    return Err(Problem::Invalid.error(format!(
                        "backup version {} does not match package version {}",
                        backup.version, staged.manifest.version
                    )));
                }
                self.warnings.clear();
                if let Some(current) = plugin::current_version(&backup.id) {
                    if current != backup.version {
                        self.warnings.push(format!(
                            "installed {} is {}, importing {}",
                            backup.id, current, backup.version
                        ));
                    } else if !self.force {
                        return Err(Problem::Conflict.error(format!(
                            "{} {} is already installed; use --force",
                            backup.id, backup.version
                        )));
                    }
                }
                self.stage = BackupStage::ExpectConfig;
                Ok(())
            }
            BackupCurrent::Config(bytes) => {
                self.config_data = Some(bytes);
                self.stage = BackupStage::AfterConfig;
                Ok(())
            }
        }
    }
}

impl BackupSink {
    fn finish(&mut self, config: &Path, budget: &CallBudget) -> Result<Vec<u8>, ProblemError> {
        let backup = self
            .backup
            .take()
            .ok_or_else(|| Problem::Invalid.error("backup.json is missing"))?;
        // Check the config hash before taking the staged tree: an error here
        // must leave it in self.staged so Drop removes the directory.
        if let Some(expected) = &backup.config_sha256 {
            let Some(config_data) = self.config_data.as_ref() else {
                return Err(Problem::Invalid.error(
                    "backup declares a config hash but config is missing",
                ));
            };
            if &package::hex_sha256(config_data) != expected {
                return Err(Problem::Invalid.error("backup config hash does not match"));
            }
        }
        let staged = self
            .staged
            .take()
            .ok_or_else(|| Problem::Invalid.error("plugin.spk is missing"))?;
        let applied = !self.no_config && self.config_data.is_some();
        let status = match install_staged(
            config,
            &staged.root,
            &staged.manifest.name,
            &staged.manifest.version,
            "replace",
            false,
            budget,
        ) {
            Ok(status) => status,
            Err(response) => return Ok(response),
        };
        if applied {
            transfer::apply_config(
                &backup.id,
                self.config_data.as_ref().expect("config data"),
                false,
            )
            .map_err(|error| match error {
                transfer::Error::Usage(message) => Problem::Invalid.error(message),
                transfer::Error::Failed(error) => error,
            })?;
        }
        Ok(import_response(
            status,
            Some(backup.id),
            applied,
            std::mem::take(&mut self.warnings),
        ))
    }
}

impl Drop for BackupSink {
    fn drop(&mut self) {
        if let Some(staged) = self.staged.take() {
            let _ = std::fs::remove_dir_all(&staged.root);
        }
    }
}

/// Send one staged package tree to the daemon. A refusal removes the tree,
/// so a failed install never leaves staging behind no matter which route
/// (install, .spk import, .spkb import) produced it.
fn install_staged(
    config: &Path,
    root: &Path,
    id: &str,
    version: &str,
    strategy: &str,
    allow_hooks: bool,
    budget: &CallBudget,
) -> Result<Value, Vec<u8>> {
    support::rpc(
        config,
        "plugin.install_staged",
        json!({
            "path": root.to_string_lossy(),
            "id": id,
            "version": version,
            "strategy": strategy,
            "allowHooks": allow_hooks,
        }),
        budget,
    )
    .map_err(|response| {
        let _ = std::fs::remove_dir_all(root);
        response
    })
}

fn staging_path() -> PathBuf {
    let plugins = paths::plugins_dir()
        .canonicalize()
        .unwrap_or_else(|_| paths::plugins_dir());
    let _ = std::fs::create_dir_all(&plugins);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    plugins.join(format!(".staging-{}-{}", std::process::id(), nanos))
}

fn import_response(
    status: Value,
    config_id: Option<String>,
    config_applied: bool,
    warnings: Vec<String>,
) -> Vec<u8> {
    let summary = status.get("summary").cloned().unwrap_or(Value::Null);
    let mut body = json!({
        "ok": true,
        "configApplied": config_applied,
    });
    if !summary.is_null() {
        body["id"] = summary.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = summary.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = summary.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = config_id {
        body["id"] = json!(id);
    }
    if !warnings.is_empty() {
        body["warnings"] = json!(warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}

fn import_outcome_json(outcome: transfer::Outcome) -> Vec<u8> {
    let mut body = json!({
        "ok": true,
        "configApplied": outcome.config.is_some(),
    });
    if let Some(status) = outcome.status {
        let summary = &status["summary"];
        body["id"] = summary.get("id").cloned().unwrap_or(Value::Null);
        body["version"] = summary.get("version").cloned().unwrap_or(Value::Null);
        body["state"] = summary.get("state").cloned().unwrap_or(Value::Null);
    } else if let Some(id) = outcome.config {
        body["id"] = json!(id);
    }
    if !outcome.warnings.is_empty() {
        body["warnings"] = json!(outcome.warnings);
    }
    http::response(
        200,
        "application/json",
        &serde_json::to_vec(&body).unwrap_or_default(),
    )
}


#[cfg(test)]
mod tests {
    use super::*;

    fn parse_head(request: &[u8]) -> http::Head {
        http::parse_head(request).expect("request head").0
    }

    fn target_of(method: &str, path: &str) -> Option<PackageTarget> {
        target(&parse_head(
            format!("{method} {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes(),
        ))
    }

    #[test]
    fn only_post_package_routes_are_claimed() {
        assert!(matches!(
            target_of("POST", "/api/v1/plugins"),
            Some(PackageTarget::Install { .. })
        ));
        assert!(matches!(
            target_of("POST", "/api/v1/import?config_id=demo"),
            Some(PackageTarget::Import { .. })
        ));
        for (method, path) in [
            ("GET", "/api/v1/plugins"),
            ("GET", "/api/v1/import"),
            ("PUT", "/api/v1/plugins"),
            ("POST", "/api/v1/plugins/demo"),
            ("POST", "/api/v1/plugins/demo/ui-token"),
        ] {
            assert!(target_of(method, path).is_none(), "{method} {path}");
        }
    }

    #[test]
    fn the_install_query_keeps_its_defaults() {
        let Some(PackageTarget::Install {
            strategy,
            enable,
            allow_hooks,
        }) = target_of("POST", "/api/v1/plugins")
        else {
            panic!("install route");
        };
        assert_eq!(strategy, "replace");
        assert!(enable);
        assert!(!allow_hooks);

        let Some(PackageTarget::Install {
            strategy,
            enable,
            allow_hooks,
        }) = target_of(
            "POST",
            "/api/v1/plugins?strategy=keep-old&enable=false&allow_hooks=true",
        )
        else {
            panic!("install route");
        };
        assert_eq!(strategy, "keep-old");
        assert!(!enable);
        assert!(allow_hooks);
    }

    #[test]
    fn the_import_query_is_parsed_once() {
        let Some(PackageTarget::Import {
            config_id,
            no_config,
            force,
        }) = target_of(
            "POST",
            "/api/v1/import?config_id=demo&no_config=true&force=true",
        )
        else {
            panic!("import route");
        };
        assert_eq!(config_id.as_deref(), Some("demo"));
        assert!(no_config);
        assert!(force);
    }

    fn lan() -> Arc<Mutex<Lan>> {
        use std::net::SocketAddr;
        let bind: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        Arc::new(Mutex::new(Lan::new(bind, bind, false, None)))
    }

    fn stream_handler(request: &[u8]) -> Box<dyn StreamHandler> {
        let head = parse_head(request);
        match exchange(&head, &lan(), PathBuf::from("unused")).expect("streamed exchange") {
            Exchange::Stream(handler) => handler,
            Exchange::Response(_) => panic!("a package route must stream"),
        }
    }

    /// Drive the handler the way the server does: write, read body chunks, run
    /// submitted jobs inline, then write the final response. Returns the bytes
    /// written, the close reason and how much of the body the handler consumed.
    fn drive(
        mut handler: Box<dyn StreamHandler>,
        body: &[u8],
        chunk: usize,
    ) -> (Vec<u8>, CloseReason, usize) {
        let limit = 4096;
        let mut out = Vec::new();
        let mut all = Vec::new();
        let mut offset = 0;
        let mut step =
            handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
        loop {
            all.extend_from_slice(&out);
            out.clear();
            match step.action {
                StreamAction::Close(reason) => return (all, reason, offset),
                StreamAction::SubmitBlocking(job) => {
                    let outcome = job(crate::server::http::JobBudget::for_test());
                    step = handler.on_event(
                        StreamEvent::BlockingDone(outcome),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                }
                StreamAction::NeedWrite => {
                    step = handler
                        .on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, limit));
                }
                StreamAction::NeedRead | StreamAction::Continue => {
                    if offset >= body.len() {
                        panic!("handler still wants body after {} bytes", offset);
                    }
                    let end = (offset + chunk).min(body.len());
                    step = handler.on_event(
                        StreamEvent::Data(&body[offset..end]),
                        &mut OutBuffer::new(&mut out, limit),
                    );
                    offset = end;
                }
                StreamAction::SendFile(_) => panic!("a package upload sends no file"),
            }
        }
    }

    #[test]
    fn a_wrong_content_type_answers_then_consumes_the_body() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: text/plain\r\nContent-Length: 7\r\n\r\n",
        );
        let (response, reason, consumed) = drive(handler, b"ignored", 3);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, 7, "the refused body must still be drained");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
        assert!(
            text.contains("Content-Type must be application/octet-stream"),
            "{text}"
        );
    }

    #[test]
    fn a_full_blocking_queue_turns_into_503_busy() {
        let mut handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: 4\r\n\r\n",
        );
        let mut out = Vec::new();
        let step = handler.on_event(StreamEvent::Writable, &mut OutBuffer::new(&mut out, 4096));
        assert!(matches!(step.action, StreamAction::NeedRead));
        let step = handler.on_event(
            StreamEvent::Data(b"body"),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(step.action, StreamAction::SubmitBlocking(_)));
        let step = handler.on_event(
            StreamEvent::BlockingDone(BlockingOutcome::Busy),
            &mut OutBuffer::new(&mut out, 4096),
        );
        assert!(matches!(step.action, StreamAction::Close(_)));
        let text = String::from_utf8_lossy(&out);
        assert!(
            text.starts_with("HTTP/1.1 503 Service Unavailable"),
            "{text}"
        );
        assert!(text.contains("Retry-After: 1\r\n"), "{text}");
        assert!(text.contains("\"code\":\"busy\""), "{text}");
    }

    #[test]
    fn an_over_limit_chunked_body_is_refused_with_413() {
        let handler = PackageUploadStream::new(
            PackageTarget::Install {
                strategy: "replace".into(),
                enable: true,
                allow_hooks: false,
            },
            http::Framing::Chunked,
            false,
            PathBuf::from("unused"),
            8,
        );
        // Two 5-byte chunks exceed the 8-byte decoded limit.
        let body = b"5\r\nabcde\r\n5\r\nfghij\r\n0\r\n\r\n";
        let (response, reason, _) = drive(Box::new(handler), body, 3);
        assert_eq!(reason, CloseReason::Completed);
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 413 Payload Too Large"), "{text}");
        assert!(text.contains("\"code\":\"payload_too_large\""), "{text}");
    }

    #[test]
    fn an_install_error_drains_the_rest_of_the_body() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: 612\r\n\r\n",
        );
        // 0xAA is not a valid tar header: the feed fails at the first 512-byte
        // boundary, after the handler has already promised a response.
        let body = vec![0xAAu8; 612];
        let (response, reason, consumed) = drive(handler, &body, 32);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, body.len(), "the refused body must be drained");
        let text = String::from_utf8_lossy(&response);
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
    }

    #[test]
    fn a_broken_chunk_after_a_refusal_does_not_write_a_second_response() {
        let handler = stream_handler(
            b"POST /api/v1/plugins HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        );
        let mut body = Vec::new();
        body.extend_from_slice(b"200\r\n");
        body.extend_from_slice(&[0xAAu8; 512]);
        body.extend_from_slice(b"\r\n");
        // A size line that is not hex: the decoder cannot find a body boundary
        // after the first chunk already produced a 400.
        body.extend_from_slice(b"zz\r\n");
        let (response, reason, consumed) = drive(handler, &body, 17);
        assert_eq!(reason, CloseReason::Completed);
        assert_eq!(consumed, body.len());
        let text = String::from_utf8_lossy(&response);
        assert_eq!(
            text.matches("HTTP/1.1 ").count(),
            1,
            "only one response head may be written: {text}"
        );
        assert!(text.starts_with("HTTP/1.1 400 Bad Request"), "{text}");
    }
}
