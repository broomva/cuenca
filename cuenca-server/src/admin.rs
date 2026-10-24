//! Owner acts, on a local Unix socket (spec section 0, "Owner acts are local").
//!
//! Minting and revoking keys, registering a location, taking a fork over and landing
//! on a location without a git remote have no HTTP route. They run here, on a socket
//! the authority creates with mode 0600 and that answers only connections whose peer
//! runs as the authority's own OS user (checked with the kernel's peer credentials on
//! every connection). The owner reaches it over SSH as that user. In the first build that is not a boundary against agents on the
//! same host (spec Drawbacks, BRO-3289); it keeps owner acts off the agent surface.
//!
//! The protocol: one JSON request per connection, one line; one JSON answer, one line,
//! `{"ok": true, ..}` or `{"ok": false, "error": ".."}`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use std::sync::Arc;

use cuenca_core::{ChangesetId, ForkName, Holder, LocationName, NodeId, PrincipalName};
use cuenca_tree::Tree;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;

use crate::authority::Authority;
use crate::keys::{Grant, PrincipalKind};

/// Longest request line.
const MAX_REQUEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AdminRequest {
    /// Mint the one key of a new principal, bound to `node`. The answer carries the
    /// token, which is stored nowhere.
    KeyMint {
        principal: PrincipalName,
        kind: PrincipalKind,
        node: NodeId,
        #[serde(default)]
        grant: Grant,
    },
    /// Revoke a principal: the log refuses its writes from now on, and its key stops
    /// authenticating.
    KeyRevoke {
        principal: PrincipalName,
    },
    KeyList,
    /// Register a location. Its main is the tree of `git` (a repository on the
    /// authority's host, read through the git bridge, every blob checked), or a snapshot
    /// of `dir` (any directory on the authority's host, read by cuenca-fs under its
    /// `.forkignore`), or empty. At most one of the two.
    LocationAdd {
        name: LocationName,
        #[serde(default)]
        remote: Option<String>,
        #[serde(default)]
        git: Option<GitSource>,
        #[serde(default)]
        dir: Option<PathBuf>,
    },
    /// Land the fork's landing request on a location without a git remote: main moves
    /// by its changeset. `changeset`, if given, must be the request's (the one the owner
    /// read the diff and verify of).
    Land {
        fork: ForkName,
        #[serde(default)]
        changeset: Option<ChangesetId>,
    },
    /// Create the owner fork `fork` (named `owner-*`) of `location`, held by the volume:
    /// the materialiser shows it writable at `forks/<fork>/` and snapshots what Finder
    /// writes there into its drafts (spec section 2a). One per device, by convention.
    OwnerFork {
        location: LocationName,
        fork: ForkName,
    },
    /// Move a fork to `holder` by compare-and-swap on its epoch.
    TakeOver {
        fork: ForkName,
        expected_epoch: u64,
        principal: PrincipalName,
        node: NodeId,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitSource {
    pub repo: PathBuf,
    pub rev: String,
}

/// Bind the admin socket at `path`, mode 0600. Its directory must be writable by its
/// owner only (anyone else could swap the socket for their own). A socket file left by
/// an earlier run is replaced; any other file there is refused. Call after the store
/// is open: the store's lock is what keeps two authorities off one state directory.
pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let mode = std::fs::metadata(dir)?.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} has mode {:o}: the admin socket's directory must be writable by its owner only",
                dir.display(),
                mode & 0o7777
            ),
        ));
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if std::os::unix::fs::FileTypeExt::is_socket(&m.file_type()) => {
            std::fs::remove_file(path)?
        }
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("{} exists and is not a socket", path.display()),
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve owner acts on `listener` until the task is dropped, to connections whose peer
/// runs as this process's effective uid.
pub async fn serve(listener: UnixListener, authority: Arc<Authority>) {
    let own_uid = rustix::process::geteuid().as_raw();
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "admin accept failed");
                continue;
            }
        };
        let authority = authority.clone();
        tokio::spawn(async move {
            match stream.peer_cred() {
                Ok(cred) if cred.uid() == own_uid => {}
                other => {
                    tracing::warn!(peer = ?other, "admin connection from another user refused");
                    return;
                }
            }
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let mut reader =
                tokio::io::BufReader::new(tokio::io::AsyncReadExt::take(read, MAX_REQUEST_BYTES));
            if reader.read_line(&mut line).await.is_err() {
                return;
            }
            let answer = match serde_json::from_str::<AdminRequest>(&line) {
                Ok(req) => {
                    tracing::info!(request = ?redacted(&req), "owner act");
                    let a = authority.clone();
                    tokio::task::spawn_blocking(move || handle(&a, req))
                        .await
                        .unwrap_or_else(|e| Err(e.to_string()))
                }
                Err(e) => Err(format!("bad request: {e}")),
            };
            let answer = match answer {
                Ok(Value::Object(mut m)) => {
                    m.insert("ok".into(), Value::Bool(true));
                    Value::Object(m)
                }
                Ok(v) => json!({ "ok": true, "result": v }),
                Err(e) => json!({ "ok": false, "error": e }),
            };
            let mut out = answer.to_string();
            out.push('\n');
            let _ = write.write_all(out.as_bytes()).await;
        });
    }
}

fn redacted(req: &AdminRequest) -> String {
    match req {
        AdminRequest::KeyMint {
            principal,
            kind,
            node,
            ..
        } => format!("key_mint {principal} {kind:?} {node}"),
        other => format!("{other:?}"),
    }
}

fn handle(a: &Authority, req: AdminRequest) -> Result<Value, String> {
    let s = |e: &dyn std::fmt::Display| e.to_string();
    match req {
        AdminRequest::KeyMint {
            principal,
            kind,
            node,
            grant,
        } => {
            volume_is_nobody(&principal)?;
            if a.is_banned(&principal) || a.store.revoked(&principal).map_err(|e| s(&e))?.is_some()
            {
                return Err(format!(
                    "principal {principal} was revoked; a principal name is never reused"
                ));
            }
            let (record, token) = a
                .keys
                .mint(principal, kind, node, grant)
                .map_err(|e| s(&e))?;
            Ok(json!({ "key": record, "token": token }))
        }
        AdminRequest::KeyRevoke { principal } => {
            volume_is_nobody(&principal)?;
            revoke(a, &principal, |p| a.store.revoke(p))
        }
        AdminRequest::KeyList => Ok(json!({ "keys": a.keys.list() })),
        AdminRequest::LocationAdd {
            name,
            remote,
            git,
            dir,
        } => {
            let tree = match (git, dir) {
                (Some(_), Some(_)) => return Err("give git or dir, not both".into()),
                (None, None) => Tree::new(),
                (None, Some(dir)) => snapshot_dir(a, &dir)?,
                (Some(GitSource { repo, rev }), None) => {
                    let repo = cuenca_tree::git::Repo::open(repo);
                    let tree = repo.read_tree(&rev).map_err(|e| s(&e))?;
                    for (oid, content) in repo.read_tree_blobs(&tree).map_err(|e| s(&e))? {
                        a.store
                            .ingest_blob(oid, content.len() as u64, content.as_slice())
                            .map_err(|e| s(&e))?;
                    }
                    tree
                }
            };
            let offset = a
                .store
                .add_location(&name, remote.as_deref(), &tree)
                .map_err(|e| s(&e))?;
            Ok(json!({ "offset": offset, "main": tree.git_tree_id().to_hex() }))
        }
        AdminRequest::Land { fork, changeset } => {
            let state = a
                .store
                .fork(&fork)
                .map_err(|e| s(&e))?
                .ok_or_else(|| format!("no fork {fork:?}"))?;
            let requested = state
                .landing
                .as_ref()
                .map(|l| l.changeset)
                .ok_or_else(|| format!("fork {fork:?} has no landing request"))?;
            if let Some(expected) = changeset
                && expected != requested
            {
                return Err(format!(
                    "fork {fork:?} asks to land {requested}, not {expected}"
                ));
            }
            let offset = a.store.advance_main(&fork, requested).map_err(|e| s(&e))?;
            let main = a
                .store
                .location(&state.location)
                .map_err(|e| s(&e))?
                .map(|l| l.main.to_hex());
            Ok(json!({ "offset": offset, "changeset": requested, "main": main }))
        }
        AdminRequest::OwnerFork { location, fork } => {
            let offset = a
                .store
                .create_fork(&fork, &location, &Holder::volume())
                .map_err(|e| s(&e))?;
            Ok(json!({ "offset": offset, "fork": fork }))
        }
        AdminRequest::TakeOver {
            fork,
            expected_epoch,
            principal,
            node,
        } => {
            let holder = Holder::new(principal, node);
            let epoch = a
                .store
                .take_over(&fork, expected_epoch, &holder)
                .map_err(|e| s(&e))?;
            Ok(json!({ "epoch": epoch }))
        }
    }
}

/// The volume's principal has no key and is never revoked: minting one would let a
/// request write owner forks as the volume, and revoking it would stop every owner
/// fork's snapshots for good.
fn volume_is_nobody(principal: &PrincipalName) -> Result<(), String> {
    if principal.as_str() == cuenca_core::VOLUME_PRINCIPAL {
        return Err(format!(
            "principal {principal:?} is the volume's, which holds owner forks; it has no key"
        ));
    }
    Ok(())
}

/// Snapshot `dir` against an empty tree and ingest every blob it names. The store
/// hashes each file as it ingests it, so a file that changed since the scan is refused
/// (`OidMismatch`) and nothing is registered: run it again.
fn snapshot_dir(a: &Authority, dir: &Path) -> Result<Tree, String> {
    let snap = cuenca_fs::snapshot(dir, &Tree::new(), &mut cuenca_fs::StatCache::new())
        .map_err(|e| e.to_string())?;
    for blob in &snap.new_blobs {
        let ingested = match &blob.content {
            cuenca_fs::Content::File(path) => {
                let file =
                    std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
                a.store.ingest_blob(blob.oid, blob.size, file)
            }
            cuenca_fs::Content::Inline(bytes) => {
                a.store.ingest_blob(blob.oid, blob.size, bytes.as_slice())
            }
        };
        ingested.map_err(|e| e.to_string())?;
    }
    Ok(snap.tree)
}

/// Revoke `principal`: in memory first, so the key is refused from its next request
/// on even if nothing below can be written (a full disk); then `record` it in the log,
/// which makes it permanent; then drop its key from the keys file.
fn revoke(
    a: &Authority,
    principal: &PrincipalName,
    record: impl FnOnce(&PrincipalName) -> cuenca_core::Result<u64>,
) -> Result<Value, String> {
    a.ban(principal);
    let offset = record(principal).map_err(|e| {
        format!(
            "{principal} is refused until this authority restarts; \
             the log could not record the revocation: {e}"
        )
    })?;
    let had_key = a.keys.remove(principal).map_err(|e| e.to_string())?;
    Ok(json!({ "offset": offset, "had_key": had_key }))
}

/// Send one owner act to the admin socket at `path` and read the answer (for the CLI's
/// `cuenca admin`, and tests).
pub fn call(path: &Path, req: &AdminRequest) -> std::io::Result<Value> {
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    let mut line = serde_json::to_string(req).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    let mut answer = String::new();
    BufReader::new(stream).read_line(&mut answer)?;
    serde_json::from_str(&answer).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Nobody;

    impl crate::Whois for Nobody {
        fn whois(&self, _ip: std::net::IpAddr) -> crate::WhoisFuture<'_> {
            Box::pin(async { Ok(None) })
        }
    }

    /// A revocation the log cannot record still refuses the key at once, and says so.
    #[test]
    fn a_revocation_the_log_cannot_record_still_refuses_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let a = Authority::open(dir.path(), Arc::new(Nobody)).unwrap();
        let p = PrincipalName::new("a1").unwrap();
        let err = revoke(&a, &p, |_| {
            Err(cuenca_core::CoreError::Io(std::io::Error::other(
                "disk full",
            )))
        })
        .unwrap_err();
        assert!(
            err.contains("refused until this authority restarts"),
            "{err}"
        );
        assert!(a.is_banned(&p));
        assert!(a.store.revoked(&p).unwrap().is_none());
    }
}
