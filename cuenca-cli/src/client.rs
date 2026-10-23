//! The authority's agent API, as `cuenca-server`'s `http` module serves it.
//!
//! Every tree the authority sends is checked against the id it is sent under before it
//! is used; blob content is checked by whoever uses it (a checkout hashes it).

use std::path::Path;
use std::time::Duration;

use cuenca_core::{ChangesetId, Fork, ForkName, Location, LocationName, Record};
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

    fn transport(&self, e: reqwest::Error) -> ClientError {
        if e.is_connect() || e.is_timeout() || e.is_request() {
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
        let url = format!("{}/v1/forks/{name}/blobs/{}", self.url, oid.to_hex());
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
        let file = std::fs::File::open(path)
            .map_err(|e| ClientError::Other(format!("{}: {e}", path.display())))?;
        let url = format!("{}/v1/blobs/{}", self.url, oid.to_hex());
        self.send(self.http.put(url).body(Body::sized(file, size)))?;
        Ok(())
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
