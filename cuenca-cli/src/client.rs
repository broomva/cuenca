//! The authority's agent API, as `cuenca-server`'s `http` module serves it.
//!
//! Every tree the authority sends is checked against the id it is sent under before it
//! is used; blob content is checked by whoever uses it (a checkout hashes it).

use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cuenca_core::checks::{CheckRun, ManifestId};
use cuenca_core::{ChangesetId, Fork, ForkName, Location, LocationName, Record};
use cuenca_server::http::{BATCH_HEADER_BYTES, MAX_BATCH_BLOBS, MAX_BATCH_BYTES, batch_header};
use cuenca_tree::{Changeset, GitOid, Tree};
use reqwest::StatusCode;
use reqwest::blocking::{Body, RequestBuilder, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::config::Config;

/// How long to wait for the authority to accept a connection: past this it is offline.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one request may take, an upload of a large blob included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The authority could not be reached; nothing was decided.
    #[error("the authority at {url} is unreachable: {reason}")]
    Offline { url: String, reason: String },
    /// The authority answered with an error.
    #[error("{message} ({code}, HTTP {status})")]
    Api {
        status: u16,
        code: String,
        message: String,
        /// The fork's head, on `not_at_base`.
        head: Option<String>,
    },
    #[error("{0}")]
    Other(String),
}

impl ClientError {
    pub fn is_offline(&self) -> bool {
        matches!(self, Self::Offline { .. })
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

/// Who the key belongs to, as the authority says.
#[derive(Debug, Clone, Deserialize)]
pub struct Whoami {
    pub principal: String,
    pub kind: String,
    pub node: String,
}

/// Where a published draft went.
#[derive(Debug, Clone, Deserialize)]
pub struct Published {
    pub id: String,
    pub fork: ForkName,
    pub epoch: u64,
    pub fenced: bool,
    pub offset: u64,
    pub fresh: bool,
}

#[derive(Deserialize)]
struct TreeAnswer {
    oid: GitOid,
    tree: Tree,
}

pub struct Client {
    http: reqwest::blocking::Client,
    url: String,
    token: String,
}

impl Client {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| ClientError::Other(e.to_string()))?;
        Ok(Self {
            http,
            url: config.url.trim_end_matches('/').to_string(),
            token: config.token.clone(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn send(&self, req: RequestBuilder) -> Result<Response> {
        let r = req
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| self.transport(e))?;
        if r.status().is_success() {
            return Ok(r);
        }
        let status = r.status();
        let body: Value = r.json().unwrap_or(Value::Null);
        let text = |k: &str| body[k].as_str().map(str::to_string);
        Err(ClientError::Api {
            status: status.as_u16(),
            code: text("error").unwrap_or_else(|| "http".into()),
            message: text("message")
                .unwrap_or_else(|| status.canonical_reason().unwrap_or("error").to_lowercase()),
            head: text("head"),
        })
    }

    /// A request that got no HTTP answer: the authority is unreachable, and whether it
    /// acted is unknown. Every write the CLI sends is idempotent (a draft by its
    /// changeset id, an upload by its blob), so the caller keeps it queued and sends it
    /// again. A refused connection, a timeout, and a connection reset mid-request (what
    /// Linux gives for an authority killed under a request) all land here; only a
    /// malformed answer to a successful request is not this.
    fn transport(&self, e: reqwest::Error) -> ClientError {
        if !(e.is_decode() || e.is_builder() || e.is_redirect()) {
            ClientError::Offline {
                url: self.url.clone(),
                reason: e.to_string(),
            }
        } else {
            ClientError::Other(e.to_string())
        }
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self.send(self.http.get(format!("{}{path}", self.url)))?;
        r.json().map_err(|e| ClientError::Other(e.to_string()))
    }

    fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        let r = self.send(self.http.post(format!("{}{path}", self.url)).json(&body))?;
        r.json().map_err(|e| ClientError::Other(e.to_string()))
    }

    pub fn whoami(&self) -> Result<Whoami> {
        self.get("/v1/whoami")
    }

    pub fn locations(&self) -> Result<Vec<Location>> {
        self.get("/v1/locations")
    }

    pub fn forks(&self) -> Result<Vec<Fork>> {
        self.get("/v1/forks")
    }

    pub fn fork(&self, name: &ForkName) -> Result<Fork> {
        self.get(&format!("/v1/forks/{name}"))
    }

    pub fn create_fork(&self, fork: &ForkName, location: &LocationName) -> Result<Fork> {
        #[derive(Deserialize)]
        struct Created {
            fork: Fork,
        }
        let c: Created = self.post("/v1/forks", json!({ "fork": fork, "location": location }))?;
        Ok(c.fork)
    }

    /// A fork's head tree, checked against its id.
    pub fn fork_tree(&self, name: &ForkName) -> Result<(GitOid, Tree)> {
        checked(self.get(&format!("/v1/forks/{name}/tree"))?)
    }

    /// A location's main tree, checked against its id.
    pub fn location_tree(&self, name: &LocationName) -> Result<(GitOid, Tree)> {
        checked(self.get(&format!("/v1/locations/{name}/tree"))?)
    }

    pub fn history(&self, name: &ForkName) -> Result<Vec<Record>> {
        self.get(&format!("/v1/forks/{name}/history"))
    }

    /// Blob `oid` of the fork's head, `None` if the head does not name it. Unchecked:
    /// whoever writes it hashes it.
    pub fn fork_blob(&self, name: &ForkName, oid: GitOid) -> Result<Option<Vec<u8>>> {
        self.blob(&format!("/v1/forks/{name}/blobs/{}", oid.to_hex()))
    }

    /// Blob `oid` of the location's main, `None` if main does not name it, checked
    /// against its id.
    pub fn location_blob(&self, name: &LocationName, oid: GitOid) -> Result<Option<Vec<u8>>> {
        let bytes = self.blob(&format!("/v1/locations/{name}/blobs/{}", oid.to_hex()))?;
        match bytes {
            Some(b) if GitOid::for_blob(&b) != oid => Err(ClientError::Other(format!(
                "the authority sent blob {oid} with content that is not {oid}"
            ))),
            b => Ok(b),
        }
    }

    fn blob(&self, path: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}{path}", self.url);
        match self.send(self.http.get(url)) {
            Ok(r) => r
                .bytes()
                .map(|b| Some(b.to_vec()))
                .map_err(|e| self.transport(e)),
            Err(ClientError::Api { status, .. }) if status == StatusCode::NOT_FOUND.as_u16() => {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Upload the blob stored at `path`, `size` bytes, as `oid`. The authority hashes
    /// what it receives and refuses bytes that are not `oid`.
    pub fn put_blob(&self, oid: GitOid, path: &Path, size: u64) -> Result<()> {
        self.upload_retried(&format!("blob {oid}"), || {
            let file = std::fs::File::open(path)
                .map_err(|e| ClientError::Other(format!("{}: {e}", path.display())))?;
            let url = format!("{}/v1/blobs/{}", self.url, oid.to_hex());
            self.send(self.http.put(url).body(Body::sized(file, size)))?;
            Ok(())
        })
    }

    /// Upload blobs in one request, each `(oid, path, size)`: the authority stores all
    /// of them or none, with one durable commit. At most [`MAX_BATCH_BLOBS`] blobs and
    /// [`MAX_BATCH_BYTES`] bytes, headers included.
    pub fn put_blobs(&self, blobs: &[(GitOid, &Path, u64)]) -> Result<()> {
        let len = batch_len(blobs.iter().map(|b| b.2));
        if blobs.len() > MAX_BATCH_BLOBS || len > MAX_BATCH_BYTES {
            return Err(ClientError::Other(format!(
                "a batch of {} blobs, {len} bytes, is over the authority's limit",
                blobs.len()
            )));
        }
        self.upload_retried(&format!("a batch of {} blobs", blobs.len()), || {
            let mut parts: Vec<Box<dyn Read + Send>> = Vec::with_capacity(blobs.len() * 2);
            for &(oid, path, size) in blobs {
                // A missing file is an error here, not a cut-off body; this opens nothing.
                std::fs::metadata(path)
                    .map_err(|e| ClientError::Other(format!("{}: {e}", path.display())))?;
                parts.push(Box::new(std::io::Cursor::new(batch_header(oid, size))));
                parts.push(Box::new(Lazy {
                    path: path.to_path_buf(),
                    size,
                    file: None,
                }));
            }
            let body = Concat(parts.into());
            let url = format!("{}/v1/blobs", self.url);
            self.send(self.http.post(url).body(Body::sized(body, len)))?;
            Ok(())
        })
    }

    /// Run an upload, telling a refusal from a lost connection.
    ///
    /// The authority refuses a request it will not serve (a revoked or unknown key)
    /// before reading its body, and closing the connection on an upload larger than the
    /// socket's buffers resets it: the refusal arrives as no answer at all. So an upload
    /// that got none asks a small question to tell the two apart. If that is refused,
    /// the refusal is the answer. If that is answered too, the upload is tried once more,
    /// and a second cut-off is reported as an error, not as offline.
    fn upload_retried(&self, what: &str, once: impl Fn() -> Result<()>) -> Result<()> {
        let (url, reason) = match once() {
            Ok(()) => return Ok(()),
            Err(ClientError::Offline { url, reason }) => (url, reason),
            Err(e) => return Err(e),
        };
        match self.whoami() {
            Ok(_) => once().map_err(|e| match e {
                ClientError::Offline { reason, .. } => ClientError::Other(format!(
                    "the authority answers, but cut off the upload of {what} twice: {reason}"
                )),
                e => e,
            }),
            Err(ClientError::Offline { .. }) => Err(ClientError::Offline { url, reason }),
            Err(e) => Err(e),
        }
    }

    pub fn publish(&self, fork: &ForkName, epoch: u64, changeset: &Changeset) -> Result<Published> {
        self.post(
            &format!("/v1/forks/{fork}/drafts"),
            json!({ "epoch": epoch, "changeset": changeset }),
        )
    }

    /// Ask to land the fork's head draft `id`. Returns the intent's offset.
    pub fn landing(&self, fork: &ForkName, epoch: u64, id: ChangesetId) -> Result<u64> {
        #[derive(Deserialize)]
        struct Offset {
            offset: u64,
        }
        let o: Offset = self.post(
            &format!("/v1/forks/{fork}/landing"),
            json!({ "epoch": epoch, "changeset": id.to_hex() }),
        )?;
        Ok(o.offset)
    }

    /// Record a verify of `id`, the fork's head, run against `base` (the location's
    /// main): the record's offset, and the manifest the authority judged it by.
    pub fn verdict(
        &self,
        fork: &ForkName,
        epoch: u64,
        id: ChangesetId,
        base: GitOid,
        checks: &[CheckRun],
    ) -> Result<(u64, Option<ManifestId>)> {
        #[derive(Deserialize)]
        struct Recorded {
            offset: u64,
            manifest: Option<ManifestId>,
        }
        let r: Recorded = self.post(
            &format!("/v1/forks/{fork}/verdicts"),
            json!({ "epoch": epoch, "changeset": id.to_hex(), "base": base.to_hex(), "checks": checks }),
        )?;
        Ok((r.offset, r.manifest))
    }
}

/// The body size of a batch of blobs of these sizes.
pub fn batch_len(sizes: impl Iterator<Item = u64>) -> u64 {
    sizes.map(|s| s + BATCH_HEADER_BYTES as u64).sum()
}

/// The first `size` bytes of the file at `path`, opened on the first read and closed
/// at its end, so a batch body holds one open file at a time, not one per blob.
struct Lazy {
    path: PathBuf,
    size: u64,
    file: Option<std::io::Take<std::fs::File>>,
}

impl Read for Lazy {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let file = match &mut self.file {
            Some(f) => f,
            None => {
                let f = std::fs::File::open(&self.path).map_err(|e| {
                    std::io::Error::new(e.kind(), format!("{}: {e}", self.path.display()))
                })?;
                self.file.insert(f.take(self.size))
            }
        };
        let n = file.read(buf)?;
        if n == 0 && !buf.is_empty() {
            self.file = None;
        }
        Ok(n)
    }
}

/// Readers read one after another, each dropped once it has ended.
struct Concat(VecDeque<Box<dyn Read + Send>>);

impl Read for Concat {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while let Some(part) = self.0.front_mut() {
            match part.read(buf)? {
                0 if !buf.is_empty() => {
                    self.0.pop_front();
                }
                n => return Ok(n),
            }
        }
        Ok(0)
    }
}

fn checked(a: TreeAnswer) -> Result<(GitOid, Tree)> {
    a.tree
        .validate()
        .map_err(|e| ClientError::Other(format!("the authority sent a bad tree: {e}")))?;
    let actual = a.tree.git_tree_id();
    if actual != a.oid {
        return Err(ClientError::Other(format!(
            "the authority sent tree {actual} as {}",
            a.oid
        )));
    }
    Ok((a.oid, a.tree))
}
