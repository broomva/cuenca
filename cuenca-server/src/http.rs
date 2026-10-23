//! The agent-facing HTTP API (spec section 0: the agent verbs, never an owner verb).
//!
//! A middleware authenticates every request ([`Authority::authenticate`]) before any
//! handler runs, so no request body is read for a caller who is not authenticated.
//! Reads are scoped ([`Caller::sees_fork`], [`Caller::sees_location`]): what a caller
//! may not read answers 404, the same as what does not exist. Fork and location names
//! are global, so creating a fork under a name taken by a fork the caller cannot read
//! answers 409; that says the name is taken, nothing about the fork. Writes go to
//! cuenca-core as the caller's [`Writer`] (its principal, on the node Tailscale named,
//! under the epoch it states), and the core's rules decide whether that writer holds
//! the fork. A draft may only name blobs the caller could read or has uploaded
//! ([`Authority::check_puts`]).
//!
//! Minting and revoking keys, registering a location, taking a fork over and landing
//! on a location without a git remote have no route here: they are owner acts, on the
//! admin socket only.
//!
//! | Method | Path | Does |
//! |---|---|---|
//! | GET | `/v1/whoami` | the caller's principal, kind, node and grant |
//! | GET | `/v1/locations` | locations whose main the caller reads |
//! | GET | `/v1/locations/{location}/tree` | the location's main tree |
//! | GET | `/v1/locations/{location}/blobs/{oid}` | a blob of that tree |
//! | GET | `/v1/forks` | forks the caller reads |
//! | POST | `/v1/forks` | `{fork, location}`: fork a location the caller is granted |
//! | GET | `/v1/forks/{fork}` | the fork's record |
//! | GET | `/v1/forks/{fork}/tree` | the fork's head tree |
//! | GET | `/v1/forks/{fork}/blobs/{oid}` | a blob of that tree |
//! | GET | `/v1/forks/{fork}/history` | the log records naming the fork (and only forks the caller reads) |
//! | POST | `/v1/forks/{fork}/drafts` | `{epoch, changeset}`: publish a draft |
//! | POST | `/v1/forks/{fork}/landing` | `{epoch, changeset}` (an id): ask to land |
//! | GET | `/v1/forks/{fork}/landing` | the fork's landing request, its step, and the error it is being retried for |
//! | POST | `/v1/forks/{fork}/verdicts` | `{epoch, changeset, base, checks}`: record a verify ([`cuenca_core::checks`]) |
//! | PUT | `/v1/blobs/{oid}` | upload a blob; `Content-Length` is its size |
//! | POST | `/v1/blobs` | upload a batch of blobs ([`batch_header`]), all or none |

use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Extension, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cuenca_core::checks::CheckRun;
use cuenca_core::{
    ChangesetId, CoreError, Event, Fork, ForkName, LocationName, MAX_BLOB_BYTES, Writer,
};
use cuenca_tree::{Changeset, GitOid};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::authority::{AuthError, Authority, Caller};

/// Largest JSON request body (a changeset of tens of thousands of ops fits).
const MAX_JSON_BYTES: usize = 64 * 1024 * 1024;

/// Largest `POST /v1/blobs` body, headers included. A larger blob goes alone, by `PUT`.
pub const MAX_BATCH_BYTES: u64 = 64 * 1024 * 1024;
/// Most blobs in one `POST /v1/blobs`.
pub const MAX_BATCH_BLOBS: usize = 4096;
/// The size of [`batch_header`].
pub const BATCH_HEADER_BYTES: usize = 28;

/// One blob's header in the body of `POST /v1/blobs`: its git id (20 bytes), then its
/// size (8 bytes, little-endian). The body is blobs back to back, each its header then
/// exactly that many bytes, and ends after the last blob's bytes.
pub fn batch_header(oid: GitOid, size: u64) -> [u8; BATCH_HEADER_BYTES] {
    let mut h = [0u8; BATCH_HEADER_BYTES];
    h[..20].copy_from_slice(oid.as_bytes());
    h[20..].copy_from_slice(&size.to_le_bytes());
    h
}

type Shared = State<Arc<Authority>>;
type Me = Extension<Caller>;

pub fn router(authority: Arc<Authority>) -> Router {
    Router::new()
        .route("/v1/whoami", get(whoami))
        .route("/v1/locations", get(locations))
        .route("/v1/locations/{location}/tree", get(location_tree))
        .route("/v1/locations/{location}/blobs/{oid}", get(location_blob))
        .route("/v1/forks", get(forks).post(create_fork))
        .route("/v1/forks/{fork}", get(fork))
        .route("/v1/forks/{fork}/tree", get(fork_tree))
        .route("/v1/forks/{fork}/blobs/{oid}", get(fork_blob))
        .route("/v1/forks/{fork}/history", get(history))
        .route("/v1/forks/{fork}/drafts", post(publish))
        .route("/v1/forks/{fork}/landing", post(landing).get(landing_state))
        .route("/v1/forks/{fork}/verdicts", post(verdict))
        .route(
            "/v1/blobs/{oid}",
            put(upload_blob).layer(DefaultBodyLimit::max(MAX_BLOB_BYTES as usize)),
        )
        .route(
            "/v1/blobs",
            post(upload_blobs).layer(DefaultBodyLimit::max(MAX_BATCH_BYTES as usize)),
        )
        .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
        .route_layer(middleware::from_fn_with_state(
            authority.clone(),
            authenticate,
        ))
        .with_state(authority)
}

/// Authenticate the request and hand its [`Caller`] to the handler; anything else is
/// answered 401 before the handler, and its body, are reached.
async fn authenticate(
    State(a): Shared,
    ConnectInfo(source): ConnectInfo<SocketAddr>,
    mut request: Request,
    next: Next,
) -> Response {
    match a.authenticate(bearer(request.headers()), source).await {
        Ok(caller) => {
            request.extensions_mut().insert(caller);
            next.run(request).await
        }
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// An error answer: a status and `{"error": code, "message": ..}`, plus fields some
/// codes carry (`head` on `not_at_base`).
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": code, "message": message.into() }),
        }
    }

    fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such thing, or not yours to read",
        )
    }

    fn internal(what: impl std::fmt::Display) -> Self {
        tracing::error!(error = %what, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        tracing::warn!(reason = %e, "request refused");
        match e {
            AuthError::Internal(e) => Self::internal(e),
            _ => Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized"),
        }
    }
}

impl From<CoreError> for ApiError {
    fn from(e: CoreError) -> Self {
        use StatusCode as S;
        let message = e.to_string();
        let (status, code) = match &e {
            CoreError::NotAtBase { head, .. } => {
                return Self {
                    status: S::CONFLICT,
                    body: json!({
                        "error": "not_at_base",
                        "message": message,
                        "head": head.to_hex(),
                    }),
                };
            }
            CoreError::BaseMoved { main, .. } => {
                return Self {
                    status: S::CONFLICT,
                    body: json!({
                        "error": "base_moved",
                        "message": message,
                        "main": main.to_hex(),
                    }),
                };
            }
            CoreError::BadManifest(_) => (S::UNPROCESSABLE_ENTITY, "bad_manifest"),
            CoreError::ManifestMismatch(_) => (S::CONFLICT, "manifest_mismatch"),
            CoreError::Name(_)
            | CoreError::BadCheckRun { .. }
            | CoreError::Path(_)
            | CoreError::Tree(_)
            | CoreError::Changeset(_)
            | CoreError::BadRemote(_)
            | CoreError::WrongLocation { .. } => (S::BAD_REQUEST, "invalid"),
            CoreError::ReservedName(_) => (S::BAD_REQUEST, "reserved_name"),
            CoreError::Hash(_) | CoreError::OidMismatch { .. } => {
                (S::UNPROCESSABLE_ENTITY, "bad_blob")
            }
            CoreError::TooLarge { .. } => (S::PAYLOAD_TOO_LARGE, "too_large"),
            CoreError::MissingBlob(_) => (S::UNPROCESSABLE_ENTITY, "missing_blob"),
            CoreError::UnknownLocation(_)
            | CoreError::UnknownFork(_)
            | CoreError::UnknownChangeset(_) => return Self::not_found(),
            CoreError::LocationExists(_) | CoreError::ForkExists(_) => (S::CONFLICT, "exists"),
            CoreError::NotHead { .. } => (S::CONFLICT, "not_head"),
            CoreError::NotVerified { .. } => (S::CONFLICT, "not_verified"),
            CoreError::NoLanding { .. } | CoreError::LandingStep { .. } => {
                (S::CONFLICT, "landing_step")
            }
            CoreError::HasRemote(_) | CoreError::NoRemote(_) => (S::CONFLICT, "remote"),
            CoreError::StaleEpoch { .. } => (S::CONFLICT, "stale_epoch"),
            CoreError::EpochMismatch { .. } => (S::CONFLICT, "epoch_mismatch"),
            CoreError::NotHolder { .. } => (S::FORBIDDEN, "not_holder"),
            CoreError::Revoked(_) => (S::UNAUTHORIZED, "unauthorized"),
            _ => return Self::internal(e),
        };
        Self::new(status, code, message)
    }
}

type ApiResult<T = Json<Value>> = Result<T, ApiError>;

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Run `f` on the blocking pool: the store's calls are synchronous.
async fn blocking<T: Send + 'static>(
    a: &Arc<Authority>,
    f: impl FnOnce(&Authority) -> ApiResult<T> + Send + 'static,
) -> ApiResult<T> {
    let a = a.clone();
    tokio::task::spawn_blocking(move || f(&a))
        .await
        .map_err(ApiError::internal)?
}

fn parse<'s, T, E: std::fmt::Display>(
    s: &'s str,
    f: impl FnOnce(&'s str) -> Result<T, E>,
) -> ApiResult<T> {
    f(s).map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid", e.to_string()))
}

fn to_json(v: impl serde::Serialize) -> ApiResult {
    serde_json::to_value(v)
        .map(Json)
        .map_err(ApiError::internal)
}

/// The fork named `name`, if the caller reads it; otherwise 404.
fn visible_fork(a: &Authority, caller: &Caller, name: &ForkName) -> ApiResult<Fork> {
    match a.store.fork(name)? {
        Some(f) if caller.sees_fork(&f) => Ok(f),
        _ => Err(ApiError::not_found()),
    }
}

/// The fork named `name`, if a write by the caller reaches the core's rules
/// ([`Caller::reaches_fork`]); otherwise 404.
fn writable_fork(a: &Authority, caller: &Caller, name: &ForkName) -> ApiResult<Fork> {
    match a.store.fork(name)? {
        Some(f) if caller.reaches_fork(&f) => Ok(f),
        _ => Err(ApiError::not_found()),
    }
}

fn visible_location(
    a: &Authority,
    caller: &Caller,
    name: &LocationName,
) -> ApiResult<cuenca_core::Location> {
    match a.store.location(name)? {
        Some(l) if caller.sees_location(&l) => Ok(l),
        _ => Err(ApiError::not_found()),
    }
}

/// The body as JSON, read only now that the caller is authenticated.
fn json_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> ApiResult<T> {
    serde_json::from_slice(body)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid", e.to_string()))
}

async fn whoami(Extension(c): Me) -> ApiResult {
    Ok(Json(json!({
        "principal": c.key.principal,
        "kind": c.key.kind,
        "node": c.key.node,
        "grant": c.key.grant,
    })))
}

async fn locations(State(a): Shared, Extension(c): Me) -> ApiResult {
    blocking(&a, move |a| {
        let all = a.store.locations()?;
        to_json(
            all.into_iter()
                .filter(|l| c.sees_location(l))
                .collect::<Vec<_>>(),
        )
    })
    .await
}

async fn location_tree(
    State(a): Shared,
    Extension(c): Me,
    Path(location): Path<String>,
) -> ApiResult {
    let name = parse(&location, LocationName::new)?;
    blocking(&a, move |a| {
        let l = visible_location(a, &c, &name)?;
        to_json(json!({ "oid": l.main.to_hex(), "tree": a.store.tree(l.main)? }))
    })
    .await
}

async fn location_blob(
    State(a): Shared,
    Extension(c): Me,
    Path((location, oid)): Path<(String, String)>,
) -> ApiResult<Response> {
    let name = parse(&location, LocationName::new)?;
    let oid = parse(&oid, GitOid::from_hex)?;
    blocking(&a, move |a| {
        let tree = visible_location(a, &c, &name)?.main;
        blob_of(a, tree, oid)
    })
    .await
}

async fn forks(State(a): Shared, Extension(c): Me) -> ApiResult {
    blocking(&a, move |a| {
        let all = a.store.forks()?;
        to_json(
            all.into_iter()
                .filter(|f| c.sees_fork(f))
                .collect::<Vec<_>>(),
        )
    })
    .await
}

#[derive(Deserialize)]
struct CreateFork {
    fork: String,
    location: String,
}

async fn create_fork(State(a): Shared, Extension(c): Me, body: axum::body::Bytes) -> ApiResult {
    let req: CreateFork = json_body(&body)?;
    let fork = parse(&req.fork, ForkName::new)?;
    let location = parse(&req.location, LocationName::new)?;
    blocking(&a, move |a| {
        visible_location(a, &c, &location)?;
        let offset = a.guarded(&c.key.principal, || {
            a.store.create_fork(&fork, &location, &c.holder)
        })?;
        to_json(json!({ "offset": offset, "fork": a.store.fork(&fork)? }))
    })
    .await
}

async fn fork(State(a): Shared, Extension(c): Me, Path(fork): Path<String>) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    blocking(&a, move |a| to_json(visible_fork(a, &c, &name)?)).await
}

async fn fork_tree(State(a): Shared, Extension(c): Me, Path(fork): Path<String>) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    blocking(&a, move |a| {
        let f = visible_fork(a, &c, &name)?;
        to_json(json!({ "oid": f.head.to_hex(), "tree": a.store.tree(f.head)? }))
    })
    .await
}

async fn fork_blob(
    State(a): Shared,
    Extension(c): Me,
    Path((fork, oid)): Path<(String, String)>,
) -> ApiResult<Response> {
    let name = parse(&fork, ForkName::new)?;
    let oid = parse(&oid, GitOid::from_hex)?;
    blocking(&a, move |a| {
        let tree = visible_fork(a, &c, &name)?.head;
        blob_of(a, tree, oid)
    })
    .await
}

/// Blob `oid`, if `tree` names it; otherwise 404. The content is verified against its
/// id on the way out.
fn blob_of(a: &Authority, tree: GitOid, oid: GitOid) -> ApiResult<Response> {
    if !a.blob_set(tree)?.contains(&oid) {
        return Err(ApiError::not_found());
    }
    let bytes = a.store.read_blob(oid)?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response())
}

/// The fork's records, less drafts fenced into a sibling the caller does not read: a
/// fenced draft is the sibling's content, shown only to those who read the sibling. Who
/// may read what is decided on the same snapshot the records come from, so a takeover
/// landing during the request cannot hand a former holder its successor's drafts.
async fn history(State(a): Shared, Extension(c): Me, Path(fork): Path<String>) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    blocking(&a, move |a| {
        let snapshot = a.store.snapshot()?;
        if !snapshot.fork(&name)?.is_some_and(|f| c.sees_fork(&f)) {
            return Err(ApiError::not_found());
        }
        let mut shown = Vec::new();
        for record in snapshot.fork_history(&name)? {
            let hidden = match &record.event {
                Event::DraftFenced { sibling, .. } => {
                    !snapshot.fork(sibling)?.is_some_and(|f| c.sees_fork(&f))
                }
                _ => false,
            };
            if hidden {
                continue;
            }
            shown.push(record);
        }
        to_json(shown)
    })
    .await
}

#[derive(Deserialize)]
struct Publish {
    epoch: u64,
    changeset: Changeset,
}

async fn publish(
    State(a): Shared,
    Extension(c): Me,
    Path(fork): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    let req: Publish = json_body(&body)?;
    blocking(&a, move |a| {
        let state = writable_fork(a, &c, &name)?;
        a.check_puts(&c, &state, &req.changeset)?;
        let writer = Writer::new(c.holder.clone(), req.epoch);
        let p = a.guarded(&c.key.principal, || {
            a.store.publish_draft(&name, &writer, &req.changeset)
        })?;
        Ok(Json(json!({
            "id": p.id.to_hex(),
            "fork": p.fork,
            "epoch": p.epoch,
            "fenced": p.fenced,
            "offset": p.offset,
            "fresh": p.fresh,
        })))
    })
    .await
}

#[derive(Deserialize)]
struct Landing {
    epoch: u64,
    changeset: String,
}

async fn landing(
    State(a): Shared,
    Extension(c): Me,
    Path(fork): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    let req: Landing = json_body(&body)?;
    let id = parse(&req.changeset, ChangesetId::from_hex)?;
    blocking(&a, move |a| {
        writable_fork(a, &c, &name)?;
        let writer = Writer::new(c.holder.clone(), req.epoch);
        let offset = a.guarded(&c.key.principal, || {
            a.store.record_landing_intent(&name, &writer, id)
        })?;
        a.wake_lander();
        Ok(Json(json!({ "offset": offset })))
    })
    .await
}

/// The fork's latest landing request, where it is, and the error the lander last hit
/// on it while it retries (`null` if none).
async fn landing_state(State(a): Shared, Extension(c): Me, Path(fork): Path<String>) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    blocking(&a, move |a| {
        let f = visible_fork(a, &c, &name)?;
        let remote = a
            .store
            .location(&f.location)?
            .is_some_and(|l| l.remote.is_some());
        Ok(Json(json!({
            "landing": f.landing,
            "remote": remote,
            "error": a.landing_errors.get(&name),
        })))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Verdict {
    epoch: u64,
    changeset: String,
    base: String,
    checks: Vec<CheckRun>,
}

/// Record the holder's verify of the fork's head: the core refuses it unless `checks`
/// is exactly the list `base` (the location's main now) declares.
async fn verdict(
    State(a): Shared,
    Extension(c): Me,
    Path(fork): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult {
    let name = parse(&fork, ForkName::new)?;
    let req: Verdict = json_body(&body)?;
    let id = parse(&req.changeset, ChangesetId::from_hex)?;
    let base = parse(&req.base, GitOid::from_hex)?;
    blocking(&a, move |a| {
        writable_fork(a, &c, &name)?;
        let writer = Writer::new(c.holder.clone(), req.epoch);
        let (offset, manifest) = a.guarded(&c.key.principal, || {
            a.store.record_verify(&name, &writer, id, base, req.checks)
        })?;
        Ok(Json(json!({ "offset": offset, "manifest": manifest })))
    })
    .await
}

/// Upload a blob. The body is streamed to a staging file, which the store ingests: it
/// hashes the bytes as git does and keeps them only if they are exactly
/// `Content-Length` long and hash to `oid` (hyper delivers a length-delimited body of
/// exactly that length, refuses a longer one, and drops a Content-Length sent with
/// chunked encoding, which this handler then refuses as unsized). The store ingests a
/// blob it already holds the same way as a new one, so neither the answer nor its
/// timing says whether it had it; either way the caller has now shown it holds the
/// bytes ([`Authority::check_puts`]).
async fn upload_blob(
    State(a): Shared,
    Extension(c): Me,
    Path(oid): Path<String>,
    h: HeaderMap,
    body: Body,
) -> ApiResult {
    let oid = parse(&oid, GitOid::from_hex)?;
    let size: u64 = h
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::LENGTH_REQUIRED,
                "length_required",
                "Content-Length is the blob's size",
            )
        })?;
    if size > MAX_BLOB_BYTES {
        return Err(CoreError::TooLarge {
            size,
            limit: MAX_BLOB_BYTES,
        }
        .into());
    }
    let staged = stage(&a, body).await?;
    blocking(&a, move |a| {
        let content = staged.reopen().map_err(ApiError::internal)?;
        let info = a.guarded(&c.key.principal, || {
            let info = a.store.ingest_blob(oid, size, content)?;
            a.possesses(&c.key.principal, oid);
            Ok::<_, CoreError>(info)
        })?;
        Ok(Json(json!({
            "oid": oid.to_hex(),
            "size": info.size,
            "sha256": info.sha256.to_hex(),
        })))
    })
    .await
}

/// Stream a request body to a staging file, removed when the handle drops.
async fn stage(a: &Authority, body: Body) -> ApiResult<tempfile::NamedTempFile> {
    let staged = tempfile::NamedTempFile::new_in(&a.staging).map_err(ApiError::internal)?;
    let mut file = tokio::fs::File::from_std(staged.reopen().map_err(ApiError::internal)?);
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "bad_body", e.to_string()))?;
        file.write_all(&chunk).await.map_err(ApiError::internal)?;
    }
    file.flush().await.map_err(ApiError::internal)?;
    Ok(staged)
}

/// Upload a batch of blobs ([`batch_header`]): one full sync and one log commit for
/// all of them, which costs the same as one blob's. Each blob is verified as by
/// `PUT /v1/blobs/{oid}`, and the batch is all or nothing: if any blob is not the
/// bytes it claims, or the body does not end exactly after the last one, nothing is
/// stored and the answer names the first fault. Once the batch is stored, the caller
/// has shown it holds every blob in it ([`Authority::check_puts`]).
async fn upload_blobs(State(a): Shared, Extension(c): Me, h: HeaderMap, body: Body) -> ApiResult {
    let size: u64 = h
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse().ok())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::LENGTH_REQUIRED,
                "length_required",
                "Content-Length is the batch's size",
            )
        })?;
    if size > MAX_BATCH_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("a batch is at most {MAX_BATCH_BYTES} bytes; send larger blobs alone"),
        ));
    }
    let staged = stage(&a, body).await?;
    blocking(&a, move |a| {
        let mut content = std::io::BufReader::new(staged.reopen().map_err(ApiError::internal)?);
        let bad = |message: String| ApiError::new(StatusCode::BAD_REQUEST, "bad_batch", message);
        let mut batch = a.store.blob_batch();
        let mut blobs = Vec::new();
        let mut oids = Vec::new();
        while let Some((oid, size)) =
            read_batch_header(&mut content).map_err(|e| bad(e.to_string()))?
        {
            if oids.len() == MAX_BATCH_BLOBS {
                return Err(bad(format!("a batch is at most {MAX_BATCH_BLOBS} blobs")));
            }
            let info = batch.add(oid, size, (&mut content).take(size))?;
            oids.push(oid);
            blobs.push(json!({
                "oid": oid.to_hex(),
                "size": info.size,
                "sha256": info.sha256.to_hex(),
            }));
        }
        a.guarded(&c.key.principal, || {
            batch.commit()?;
            for oid in oids {
                a.possesses(&c.key.principal, oid);
            }
            Ok::<_, CoreError>(())
        })?;
        Ok(Json(json!({ "blobs": blobs })))
    })
    .await
}

/// The next blob's header, or `None` where the body ends cleanly between blobs.
fn read_batch_header(r: &mut impl Read) -> std::io::Result<Option<(GitOid, u64)>> {
    let mut h = [0u8; BATCH_HEADER_BYTES];
    let mut got = 0;
    while got < h.len() {
        match r.read(&mut h[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the body ends inside a blob's header",
                ));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let oid = GitOid::from_bytes(h[..20].try_into().expect("20 bytes"));
    let size = u64::from_le_bytes(h[20..].try_into().expect("8 bytes"));
    Ok(Some((oid, size)))
}
