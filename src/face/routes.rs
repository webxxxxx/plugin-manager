//! The face route table: one owner per (method, path).
//!
//! api, the fallback handler and the pre-body hooks used to keep three
//! separate method/path matches, each normalizing the historical /files alias
//! on its own. This module owns the shape, the source of truth for 405 Allow,
//! and the body policy a server-owned route needs before the face runs.
//!
//! A row names a path pattern, the methods it accepts, the face that handles
//! it, and the handler endpoint. More specific rows win; a path with no method
//! match returns MethodNotAllowed with Allow derived from the rows at the same
//! specificity, so a new method cannot silently fall through.

use std::sync::OnceLock;

use crate::platform::limits::Limits;
use crate::server::http::model as http;
use crate::server::http::{BodyPolicy, Route};

/// Who handles a route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Owner {
    Api,
    Fallback,
    PreBody,
}

/// What runs after the route is matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endpoint {
    Console,
    Login,
    Logout,
    /// A macro-declared API route, identified by its handler name.
    Api(&'static str),
    /// A macro-declared pre-body route, identified by its handler name.
    PreBody(&'static str),
    ExportSpk,
    ExportBundle,
    /// A /api/v1/import path that must answer 405, not 404.
    ImportClaim,
    /// An unclaimed /api/v1 path: the API face keeps its 404 projection.
    UnknownApi,
    BlobDownload,
    FileDownload,
    BlobCreate,
    BlobStatus,
    BlobComplete,
    BlobAbort,
    FileCreate,
    FileStatus,
    FileComplete,
    FileAbort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Body {
    Empty,
    Json,
    Complete,
    Streamed,
}

impl Body {
    fn policy(self, limits: &Limits) -> BodyPolicy {
        match self {
            Body::Empty => BodyPolicy::Empty,
            Body::Json => BodyPolicy::Buffered {
                limit: limits.max_json_body,
            },
            Body::Complete => BodyPolicy::Buffered {
                limit: limits.max_complete_body,
            },
            Body::Streamed => BodyPolicy::Streamed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Run {
    Inline,
    Blocking,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Pattern {
    /// Exact, query-less paths such as /, /login and /logout.
    Exact(&'static [&'static str]),
    /// Slash-separated segments after the query is removed.
    Segments(&'static [Segment]),
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Segment {
    Lit(&'static str),
    Param(&'static str),
    /// Matches the rest of the path, including nothing.
    Rest,
}

/// Whether a method miss on this row is a 405 claim or an absent route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Miss {
    Claim,
    Ignore,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RouteSpec {
    pub(crate) endpoint: Endpoint,
    pub(crate) owner: Owner,
    methods: &'static [&'static str],
    pattern: Pattern,
    body: Body,
    run: Run,
    public: bool,
    priority: u8,
    miss: Miss,
}

impl RouteSpec {
    fn matches(&self, path: &str, parts: &[&str]) -> bool {
        match self.pattern {
            Pattern::Exact(paths) => paths.contains(&path),
            Pattern::Segments(segments) => match segments.split_last() {
                Some((Segment::Rest, prefix)) => {
                    parts.len() >= prefix.len()
                        && prefix
                            .iter()
                            .zip(parts)
                            .all(|(segment, part)| segment_matches(segment, part))
                }
                Some((_, _)) => {
                    segments.len() == parts.len()
                        && segments
                            .iter()
                            .zip(parts)
                            .all(|(segment, part)| segment_matches(segment, part))
                }
                None => parts.is_empty(),
            },
        }
    }

    fn method_matches(&self, method: &str) -> bool {
        self.methods
            .iter()
            .any(|candidate| *candidate == "*" || *candidate == method)
    }

    fn score(&self) -> u32 {
        match self.pattern {
            Pattern::Exact(paths) => {
                9000 + paths.iter().map(|path| path.len()).max().unwrap_or(0) as u32
            }
            Pattern::Segments(segments) => {
                let literals = segments
                    .iter()
                    .filter(|segment| matches!(segment, Segment::Lit(_)))
                    .count() as u32;
                let rest = segments
                    .iter()
                    .any(|segment| matches!(segment, Segment::Rest));
                literals * 1000 + segments.len() as u32 * 10 - if rest { 100 } else { 0 }
            }
        }
    }

    pub(crate) fn route(&self, limits: &Limits) -> Route {
        let body = self.body.policy(limits);
        let route = match self.run {
            Run::Inline => Route::inline(body),
            Run::Blocking => Route::blocking(body),
        };
        if self.public {
            route.public()
        } else {
            route
        }
    }

    /// A 405 never reads a body: the method did not match, so no handler will
    /// consume it. Keep the representative row's dispatch and visibility.
    pub(crate) fn route_for_405(&self) -> Route {
        // Every management 405 runs on the blocking pool except the console,
        // which was inline before the table existed.
        let route = if self.endpoint == Endpoint::Console {
            Route::inline(BodyPolicy::Empty)
        } else {
            Route::blocking(BodyPolicy::Empty)
        };
        if self.public {
            route.public()
        } else {
            route
        }
    }
}

/// One ordinary `/api/v1` row declared by the API face's macro table.
///
/// The macro supplies only the method, the path and the handler name. Owner,
/// public, priority, body policy and run mode are derived here; the historical
/// 404-on-method-miss rows set `quiet`.
pub(crate) struct ApiRouteDef {
    kind: RouteKind,
    pub(crate) method: &'static str,
    pub(crate) path: &'static str,
    pub(crate) handler: &'static str,
    pub(crate) quiet: bool,
}

#[derive(Clone, Copy)]
enum RouteKind {
    /// An ordinary JSON API route handled by `ApiFace`.
    Api,
    /// A streamed upload claimed before the body is read by `PreBodyFaces`.
    PreBody,
}

impl ApiRouteDef {
    pub(crate) const fn api(
        method: &'static str,
        path: &'static str,
        handler: &'static str,
        quiet: bool,
    ) -> ApiRouteDef {
        ApiRouteDef {
            kind: RouteKind::Api,
            method,
            path,
            handler,
            quiet,
        }
    }

    pub(crate) const fn prebody(
        method: &'static str,
        path: &'static str,
        handler: &'static str,
    ) -> ApiRouteDef {
        ApiRouteDef {
            kind: RouteKind::PreBody,
            method,
            path,
            handler,
            quiet: false,
        }
    }

    fn spec(&self) -> RouteSpec {
        let (endpoint, owner, body, run, priority) = match self.kind {
            RouteKind::Api => (
                Endpoint::Api(self.handler),
                Owner::Api,
                body_for(self.method),
                Run::Blocking,
                0,
            ),
            RouteKind::PreBody => (
                Endpoint::PreBody(self.handler),
                Owner::PreBody,
                Body::Streamed,
                Run::Inline,
                2,
            ),
        };
        RouteSpec {
            endpoint,
            owner,
            methods: Box::leak(vec![self.method].into_boxed_slice()),
            pattern: compile_pattern(self.path),
            body,
            run,
            public: false,
            priority,
            miss: if self.quiet {
                Miss::Ignore
            } else {
                Miss::Claim
            },
        }
    }
}

/// A normal JSON API route buffers a JSON body; reads, deletes and the
/// body-less management writes keep the empty policy.
fn body_for(method: &str) -> Body {
    match method {
        "POST" | "PUT" | "PATCH" => Body::Json,
        _ => Body::Empty,
    }
}

/// Compile one macro path such as `/api/v1/services/{id}` into segments.
///
/// This runs once when the table is built, not per request. A malformed path
/// is a programming error: fail loudly instead of silently returning 404.
fn compile_pattern(path: &'static str) -> Pattern {
    assert!(
        path.starts_with('/'),
        "API route path must start with /: {path}"
    );
    let trimmed = path.trim_matches('/');
    let mut segments = Vec::new();
    let mut names = std::collections::HashSet::new();
    if !trimmed.is_empty() {
        for part in trimmed.split('/') {
            assert!(!part.is_empty(), "empty segment in API route: {path}");
            if part == "*" {
                segments.push(Segment::Rest);
                continue;
            }
            if let Some(name) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                assert!(!name.is_empty(), "empty parameter in API route: {path}");
                assert!(names.insert(name), "duplicate parameter '{name}' in {path}");
                segments.push(Segment::Param(name));
                continue;
            }
            assert!(
                !part.contains('{') && !part.contains('}'),
                "malformed parameter segment '{part}' in {path}"
            );
            segments.push(Segment::Lit(part));
        }
    }
    Pattern::Segments(Box::leak(segments.into_boxed_slice()))
}

/// The complete route table: hand-written protocol rows plus the API face's
/// macro rows. Built once; every request reads the leaked slice.
fn specs() -> &'static [RouteSpec] {
    static BUILT: OnceLock<&'static [RouteSpec]> = OnceLock::new();
    BUILT.get_or_init(|| {
        let mut rows: Vec<RouteSpec> = MANUAL_SPECS.to_vec();
        rows.extend(crate::face::api::route_defs().map(ApiRouteDef::spec));
        rows.extend(crate::face::prebody::route_defs().map(ApiRouteDef::spec));
        let leaked: &'static [RouteSpec] = Box::leak(rows.into_boxed_slice());
        leaked
    })
}

/// The matched row plus the decoded-name path parameters.
#[derive(Debug)]
pub(crate) struct Matched<'a> {
    pub(crate) spec: &'static RouteSpec,
    params: Vec<(&'static str, &'a str)>,
}

impl<'a> Matched<'a> {
    pub(crate) fn param(&self, name: &str) -> Option<&'a str> {
        self.params
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| *value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owners(u8);

impl Owners {
    const API: u8 = 1;
    const FALLBACK: u8 = 2;
    const PRE_BODY: u8 = 4;

    fn add(&mut self, owner: Owner) {
        *self = Owners(
            self.0
                | match owner {
                    Owner::Api => Owners::API,
                    Owner::Fallback => Owners::FALLBACK,
                    Owner::PreBody => Owners::PRE_BODY,
                },
        );
    }

    pub(crate) fn contains(self, owner: Owner) -> bool {
        self.0
            & match owner {
                Owner::Api => Owners::API,
                Owner::Fallback => Owners::FALLBACK,
                Owner::PreBody => Owners::PRE_BODY,
            }
            != 0
    }

    /// The API face claims a 405 only when the fallback is not the better
    /// owner: blob/file downloads keep the fallback's historical body shape.
    pub(crate) fn api_claims(self) -> bool {
        self.contains(Owner::Api) && !self.contains(Owner::Fallback)
    }
}

#[derive(Debug)]
pub(crate) enum Match<'a> {
    Found(Matched<'a>),
    MethodNotAllowed {
        allow: String,
        owners: Owners,
        spec: &'static RouteSpec,
    },
    NotFound,
}

impl<'a> Match<'a> {
    pub(crate) fn found(self) -> Option<Matched<'a>> {
        match self {
            Match::Found(matched) => Some(matched),
            _ => None,
        }
    }
}

/// Parse one request path into segments, applying the historical
/// /api/v1/files alias once. The query is not part of routing.
pub(crate) fn segments(path: &str) -> Vec<&str> {
    let path = path.split_once('?').map(|(path, _)| path).unwrap_or(path);
    let trimmed = path.trim_matches('/');
    let mut parts: Vec<&str> = if trimmed.is_empty() {
        Vec::new()
    } else {
        trimmed.split('/').collect()
    };
    if parts.get(2) == Some(&"files") {
        parts[2] = "file";
    }
    parts
}

pub(crate) fn split_query(path: &str) -> (&str, &str) {
    path.split_once('?').unwrap_or((path, ""))
}

#[allow(clippy::result_large_err)] // Match carries the wire allow string; it is not cloned.
pub(crate) fn resolve<'a>(method: &str, path: &'a str) -> Match<'a> {
    let (path, _) = split_query(path);
    let parts = segments(path);
    let mut candidates: Vec<(&'static RouteSpec, u32)> = specs()
        .iter()
        .filter(|spec| spec.matches(path, &parts))
        .filter(|spec| spec.method_matches(method) || spec.miss == Miss::Claim)
        .map(|spec| (spec, spec.score()))
        .collect();
    if candidates.is_empty() {
        return Match::NotFound;
    }
    let best = candidates
        .iter()
        .map(|(_, score)| *score)
        .max()
        .unwrap_or(0);
    candidates.retain(|(_, score)| *score == best);

    let matched: Vec<&'static RouteSpec> = candidates
        .iter()
        .filter(|(spec, _)| spec.method_matches(method))
        .map(|(spec, _)| *spec)
        .collect();
    if !matched.is_empty() {
        let priority = matched.iter().map(|spec| spec.priority).max().unwrap_or(0);
        let mut chosen = matched
            .iter()
            .filter(|spec| spec.priority == priority)
            .copied();
        let spec = chosen.next().expect("matched is not empty");
        if let Some(other) = chosen.next() {
            if other.endpoint != spec.endpoint {
                debug_assert!(
                    false,
                    "ambiguous route: {:?} and {:?} for {} {}",
                    spec.endpoint, other.endpoint, method, path
                );
            }
        }
        return Match::Found(Matched {
            spec,
            params: params(spec, &parts),
        });
    }

    let mut owners = Owners(0);
    let mut methods: Vec<&'static str> = Vec::new();
    for (spec, _) in &candidates {
        owners.add(spec.owner);
        for method in spec.methods {
            if *method != "*" && *method != "HEAD" && !methods.contains(method) {
                methods.push(method);
            }
        }
    }
    let spec = candidates
        .iter()
        .max_by_key(|(spec, _)| spec.priority)
        .map(|(spec, _)| *spec)
        .expect("candidates is not empty");
    Match::MethodNotAllowed {
        allow: allow(&methods),
        owners,
        spec,
    }
}

fn segment_matches(segment: &Segment, part: &str) -> bool {
    match segment {
        Segment::Lit(value) => *value == part,
        Segment::Param(_) | Segment::Rest => true,
    }
}

fn params<'a>(spec: &'static RouteSpec, parts: &[&'a str]) -> Vec<(&'static str, &'a str)> {
    let Pattern::Segments(segments) = spec.pattern else {
        return Vec::new();
    };
    segments
        .iter()
        .zip(parts)
        .filter_map(|(segment, part)| match segment {
            Segment::Param(name) => Some((*name, *part)),
            Segment::Lit(_) | Segment::Rest => None,
        })
        .collect()
}

fn allow(methods: &[&str]) -> String {
    const ORDER: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "TRACE"];
    let mut ordered: Vec<&str> = ORDER
        .iter()
        .copied()
        .filter(|candidate| methods.contains(candidate))
        .collect();
    for method in methods {
        if !ORDER.contains(method) && !ordered.contains(method) {
            ordered.push(method);
        }
    }
    ordered.join(", ")
}

/// The body policy for a server-owned request. Only the upload-complete
/// endpoints read a bounded body; everything else keeps the JSON limit the
/// server used before the table existed.
pub(crate) fn body_policy(head: &http::Head, limits: &Limits) -> BodyPolicy {
    match resolve(&head.method, &head.path) {
        Match::Found(matched) if matched.spec.body == Body::Complete => BodyPolicy::Buffered {
            limit: limits.max_complete_body,
        },
        _ => BodyPolicy::Buffered {
            limit: limits.max_json_body,
        },
    }
}

const GET: &[&str] = &["GET"];
const GET_HEAD: &[&str] = &["GET", "HEAD"];
const POST: &[&str] = &["POST"];
const DELETE: &[&str] = &["DELETE"];
const ANY: &[&str] = &["*"];

#[allow(clippy::too_many_arguments)] // A table row reads better as named columns.
const fn row(
    endpoint: Endpoint,
    owner: Owner,
    methods: &'static [&'static str],
    pattern: Pattern,
    body: Body,
    run: Run,
    public: bool,
    priority: u8,
) -> RouteSpec {
    RouteSpec {
        endpoint,
        owner,
        methods,
        pattern,
        body,
        run,
        public,
        priority,
        miss: Miss::Claim,
    }
}

const fn ignore(
    endpoint: Endpoint,
    owner: Owner,
    methods: &'static [&'static str],
    pattern: Pattern,
    body: Body,
    run: Run,
    public: bool,
) -> RouteSpec {
    RouteSpec {
        endpoint,
        owner,
        methods,
        pattern,
        body,
        run,
        public,
        priority: 0,
        miss: Miss::Ignore,
    }
}

const fn lit(value: &'static str) -> Segment {
    Segment::Lit(value)
}

const fn param(name: &'static str) -> Segment {
    Segment::Param(name)
}

/// Hand-written protocol rows: console/login, streamed exports and downloads,
/// pre-body uploads, the fallback's blob/file sessions, the import claim and the
/// `/api/v1` catch-all. Ordinary API rows live in `api::route_defs()`.
static MANUAL_SPECS: &[RouteSpec] = &[
    // Public console surface.
    row(
        Endpoint::Console,
        Owner::Api,
        GET,
        Pattern::Exact(&["/", "/index.html"]),
        Body::Empty,
        Run::Inline,
        true,
        0,
    ),
    ignore(
        Endpoint::Login,
        Owner::Api,
        POST,
        Pattern::Exact(&["/login"]),
        Body::Json,
        Run::Blocking,
        true,
    ),
    ignore(
        Endpoint::Logout,
        Owner::Api,
        POST,
        Pattern::Exact(&["/logout"]),
        Body::Empty,
        Run::Blocking,
        true,
    ),
    // Streamed exports stay on the face's own dispatch.
    row(
        Endpoint::ExportSpk,
        Owner::Api,
        GET,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("export"),
            param("id"),
            lit("spk"),
        ]),
        Body::Streamed,
        Run::Inline,
        false,
        0,
    ),
    row(
        Endpoint::ExportBundle,
        Owner::Api,
        GET,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("export"), param("id")]),
        Body::Streamed,
        Run::Inline,
        false,
        0,
    ),
    // Pre-body upload rows are declared in face/prebody.rs.

    // The API keeps its 404 for a bare /api/v1/plugins of any non-POST method;
    // the POST row above wins on priority.
    row(
        Endpoint::UnknownApi,
        Owner::Api,
        ANY,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("plugins")]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
    // GET /api/v1/import keeps its historical 405 rather than the catch-all
    // 404. The claim has no method of its own; Allow comes from the POST row.
    row(
        Endpoint::ImportClaim,
        Owner::Api,
        &[],
        Pattern::Segments(&[lit("api"), lit("v1"), lit("import")]),
        Body::Empty,
        Run::Blocking,
        false,
        3,
    ),
    // Fallback blob/file session routes. The streamed POST methods stay with
    // the pre-body rows above; these rows own the buffered session metadata.
    row(
        Endpoint::BlobCreate,
        Owner::Fallback,
        POST,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("blobs"), lit("uploads")]),
        Body::Json,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::BlobStatus,
        Owner::Fallback,
        GET,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("blobs"),
            lit("uploads"),
            param("id"),
        ]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::BlobComplete,
        Owner::Fallback,
        POST,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("blobs"),
            lit("uploads"),
            param("id"),
            lit("complete"),
        ]),
        Body::Complete,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::BlobAbort,
        Owner::Fallback,
        DELETE,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("blobs"),
            lit("uploads"),
            param("id"),
        ]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::FileCreate,
        Owner::Fallback,
        POST,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("file"), lit("uploads")]),
        Body::Json,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::FileStatus,
        Owner::Fallback,
        GET,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("file"),
            lit("uploads"),
            param("id"),
        ]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::FileComplete,
        Owner::Fallback,
        POST,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("file"),
            lit("uploads"),
            param("id"),
            lit("complete"),
        ]),
        Body::Complete,
        Run::Blocking,
        false,
        0,
    ),
    row(
        Endpoint::FileAbort,
        Owner::Fallback,
        DELETE,
        Pattern::Segments(&[
            lit("api"),
            lit("v1"),
            lit("file"),
            lit("uploads"),
            param("id"),
        ]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
    // Streamed downloads stay with the API face. The empty-method fallback
    // rows below make a POST keep the fallback's historical 405 and Allow.
    row(
        Endpoint::BlobDownload,
        Owner::Api,
        GET_HEAD,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("blobs"), param("id")]),
        Body::Streamed,
        Run::Inline,
        false,
        0,
    ),
    row(
        Endpoint::FileDownload,
        Owner::Api,
        GET_HEAD,
        Pattern::Segments(&[lit("api"), lit("v1"), lit("file"), param("id")]),
        Body::Streamed,
        Run::Inline,
        false,
        0,
    ),
    row(
        Endpoint::BlobDownload,
        Owner::Fallback,
        &[],
        Pattern::Segments(&[lit("api"), lit("v1"), lit("blobs"), param("id")]),
        Body::Empty,
        Run::Blocking,
        false,
        3,
    ),
    row(
        Endpoint::FileDownload,
        Owner::Fallback,
        &[],
        Pattern::Segments(&[lit("api"), lit("v1"), lit("file"), param("id")]),
        Body::Empty,
        Run::Blocking,
        false,
        3,
    ),
    // Fallback 405 claims for the shapes whose streaming method lives above.
    row(
        Endpoint::PreBody("blob_stream"),
        Owner::Fallback,
        &[],
        Pattern::Segments(&[lit("api"), lit("v1"), lit("blobs")]),
        Body::Empty,
        Run::Blocking,
        false,
        3,
    ),
    row(
        Endpoint::PreBody("file_stream"),
        Owner::Fallback,
        &[],
        Pattern::Segments(&[lit("api"), lit("v1"), lit("file")]),
        Body::Empty,
        Run::Blocking,
        false,
        3,
    ),
    // Any other /api/v1 path stays on the API face: its UnknownApi arm owns
    // the 404 shape, and the catch-all score keeps it below every named row.
    row(
        Endpoint::UnknownApi,
        Owner::Api,
        ANY,
        Pattern::Segments(&[lit("api"), lit("v1"), Segment::Rest]),
        Body::Empty,
        Run::Blocking,
        false,
        0,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn path_of(spec: &RouteSpec) -> String {
        match spec.pattern {
            Pattern::Exact(paths) => paths[0].to_string(),
            Pattern::Segments(segments) => {
                let mut path = String::from("/");
                for segment in segments {
                    match segment {
                        Segment::Lit(value) => path.push_str(value),
                        Segment::Param(_) => path.push('x'),
                        Segment::Rest => path.push_str("rest"),
                    }
                    path.push('/');
                }
                path.trim_end_matches('/').to_string()
            }
        }
    }

    #[test]
    fn every_row_is_reachable_by_its_own_methods() {
        for spec in specs() {
            if spec.methods.is_empty() {
                continue;
            }
            let path = path_of(spec);
            for method in spec.methods {
                if *method == "*" {
                    continue;
                }
                let matched = resolve(method, &path);
                let Match::Found(matched) = matched else {
                    panic!(
                        "{:?} {} should reach {:?}: {:?}",
                        spec.endpoint, path, spec.endpoint, matched
                    );
                };
                assert_eq!(
                    matched.spec.endpoint, spec.endpoint,
                    "{} {} reached the wrong endpoint",
                    method, path
                );
            }
        }
    }

    #[test]
    fn one_method_has_one_owner() {
        for spec in specs() {
            let path = path_of(spec);
            for method in spec.methods {
                if *method == "*" {
                    continue;
                }
                let Match::Found(matched) = resolve(method, &path) else {
                    panic!("{} {} is not a route", method, path);
                };
                assert_eq!(
                    matched.spec.owner, spec.owner,
                    "{} {} has two owners",
                    method, path
                );
            }
        }
    }

    #[test]
    fn method_not_allowed_derives_allow_from_the_table() {
        let cases = [
            ("PATCH", "/api/v1/config/demo", "GET, PUT"),
            ("PATCH", "/api/v1/trust", "GET, POST"),
            ("POST", "/api/v1/trust/demo", "DELETE"),
            ("POST", "/api/v1/export/demo", "GET"),
            ("GET", "/api/v1/import", "POST"),
            ("POST", "/api/v1/blobs/demo", "GET"),
            ("GET", "/api/v1/blobs/uploads", "POST"),
            ("PATCH", "/api/v1/blobs/uploads/demo", "GET, DELETE"),
            ("GET", "/api/v1/blobs/uploads/demo/complete", "POST"),
            ("GET", "/api/v1/blobs/uploads/demo/parts/1", "PUT"),
        ];
        for (method, path, expected) in cases {
            let Match::MethodNotAllowed { allow, .. } = resolve(method, path) else {
                panic!("{} {} should be a 405", method, path);
            };
            assert_eq!(allow, expected, "{} {}", method, path);
        }
    }

    #[test]
    fn transfer_paths_have_one_owner_across_faces() {
        let cases = [
            ("POST", "/api/v1/blobs", Owner::PreBody),
            ("GET", "/api/v1/blobs", Owner::PreBody),
            ("POST", "/api/v1/plugins", Owner::PreBody),
            ("GET", "/api/v1/plugins", Owner::Api),
            ("PUT", "/api/v1/files/uploads/u/parts/1", Owner::PreBody),
            ("GET", "/api/v1/file/uploads/u/parts/1", Owner::PreBody),
            ("POST", "/api/v1/file/uploads", Owner::Fallback),
            ("POST", "/api/v1/file/uploads/u/complete", Owner::Fallback),
        ];
        for (method, path, owner) in cases {
            match resolve(method, path) {
                Match::Found(matched) => {
                    assert_eq!(matched.spec.owner, owner, "{} {}", method, path)
                }
                Match::MethodNotAllowed {
                    owners: owners_here,
                    ..
                } => assert!(
                    owners_here.contains(owner),
                    "{} {} should include {:?}",
                    method,
                    path,
                    owner
                ),
                Match::NotFound => panic!("{} {} has no owner", method, path),
            }
        }
    }

    #[test]
    fn the_files_alias_normalizes_once() {
        assert_eq!(
            segments("/api/v1/files/uploads/u/parts/1"),
            vec!["api", "v1", "file", "uploads", "u", "parts", "1"]
        );
        let Match::Found(matched) = resolve("PUT", "/api/v1/files/uploads/u/parts/1") else {
            panic!("files alias should match a file part");
        };
        assert_eq!(matched.spec.endpoint, Endpoint::PreBody("file_part"));
        assert_eq!(matched.param("upload"), Some("u"));
        assert_eq!(matched.param("part"), Some("1"));
    }

    #[test]
    fn quiet_api_rows_keep_their_404_on_a_method_miss() {
        let Match::Found(matched) = resolve("POST", "/api/v1/version") else {
            panic!("the /api/v1 catch-all must answer, not 405");
        };
        assert_eq!(matched.spec.endpoint, Endpoint::UnknownApi);
        assert!(matches!(
            resolve("PATCH", "/api/v1/config/demo"),
            Match::MethodNotAllowed { .. }
        ));
    }

    #[test]
    fn exact_public_paths_do_not_claim_a_method_miss() {
        assert!(matches!(resolve("GET", "/login"), Match::NotFound));
        assert!(matches!(resolve("GET", "/logout"), Match::NotFound));
        let Match::MethodNotAllowed { allow, .. } = resolve("POST", "/") else {
            panic!("console POST should be a 405");
        };
        assert_eq!(allow, "GET");
    }

    #[test]
    fn complete_bodies_have_their_own_limit() {
        let limits = crate::platform::limits::test_default();
        let head = http::Head {
            method: "POST".into(),
            path: "/api/v1/blobs/uploads/u/complete".into(),
            version: "HTTP/1.1".into(),
            headers: Vec::new(),
        };
        assert_eq!(
            body_policy(&head, &limits),
            BodyPolicy::Buffered {
                limit: limits.max_complete_body
            }
        );
        let head = http::Head {
            method: "POST".into(),
            path: "/api/v1/trust".into(),
            ..head
        };
        assert_eq!(
            body_policy(&head, &limits),
            BodyPolicy::Buffered {
                limit: limits.max_json_body
            }
        );
    }

    #[test]
    fn a_method_has_no_ambiguous_row() {
        for spec in specs() {
            for method in spec.methods {
                if *method == "*" {
                    continue;
                }
                let path = path_of(spec);
                let parts = segments(&path);
                let matches: Vec<&RouteSpec> = specs()
                    .iter()
                    .filter(|candidate| candidate.matches(&path, &parts))
                    .filter(|candidate| candidate.method_matches(method))
                    .collect();
                let best = matches
                    .iter()
                    .map(|candidate| candidate.score())
                    .max()
                    .unwrap_or(0);
                let matches: Vec<&RouteSpec> = matches
                    .into_iter()
                    .filter(|candidate| candidate.score() == best)
                    .collect();
                let priority = matches
                    .iter()
                    .map(|candidate| candidate.priority)
                    .max()
                    .unwrap_or(0);
                let winners: Vec<&RouteSpec> = matches
                    .iter()
                    .copied()
                    .filter(|candidate| candidate.priority == priority)
                    .collect();
                assert!(
                    winners
                        .iter()
                        .all(|candidate| candidate.endpoint == winners[0].endpoint),
                    "ambiguous {} {}: {:?}",
                    method,
                    path,
                    winners
                        .iter()
                        .map(|candidate| candidate.endpoint)
                        .collect::<Vec<_>>()
                );
            }
        }
    }
}
