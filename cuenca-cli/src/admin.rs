//! `cuenca admin`: owner acts, sent to the authority's local admin socket
//! (`cuenca_server::admin`). Only the binary has this module: the agent verbs the MCP
//! server reuses live in the library and cannot reach it.
//!
//! Landing on a location without a remote is one (`cuenca admin land`); bypassing a
//! gate is another, and arrives with the blocking gates (BRO-3307).

use std::path::PathBuf;

use anyhow::bail;
use clap::{Args, Subcommand, ValueEnum};
use cuenca_cli::Report;
use cuenca_core::{ChangesetId, ForkName, LocationName, NodeId, PrincipalName};
use cuenca_server::admin::{AdminRequest, GitSource, call};
use cuenca_server::{Grant, PrincipalKind};

#[derive(Args)]
pub struct Admin {
    /// The authority's admin socket.
    #[arg(long, env = "CUENCA_ADMIN_SOCKET")]
    socket: PathBuf,
    #[command(subcommand)]
    act: Act,
}

#[derive(Subcommand)]
enum Act {
    /// Mint, revoke and list keys.
    #[command(subcommand)]
    Key(Key),
    /// Register locations.
    #[command(subcommand)]
    Location(LocationAct),
    /// Land a fork's landing request on a location without a git remote: main moves by
    /// its changeset, and the earlier main stays in the log. Read its `cuenca diff` and
    /// `cuenca log` (the verify record) first.
    Land {
        fork: String,
        /// The changeset you read; refused if the request is for another.
        #[arg(long)]
        changeset: Option<String>,
    },
    /// Create an owner fork (named `owner-<device>`) of a location, held by the volume:
    /// shown writable at `forks/<fork>/` for Finder, its edits snapshotted into drafts
    /// every 10 s. One per device. Refused if the location's main tracks paths its
    /// `.forkignore` names.
    OwnerFork { location: String, fork: String },
    /// Move a fork to a new holder, by compare-and-swap on its epoch.
    TakeOver {
        fork: String,
        /// The fork's epoch now; the takeover fails if it moved.
        #[arg(long)]
        expected_epoch: u64,
        #[arg(long)]
        principal: String,
        /// The new holder's Tailscale stable node ID.
        #[arg(long)]
        node: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Agent,
    OwnerDevice,
}

#[derive(Subcommand)]
enum Key {
    /// Mint the one key of a new principal, bound to one node. The key is printed once
    /// and stored nowhere.
    Mint {
        principal: String,
        #[arg(long, value_enum)]
        kind: Kind,
        /// The Tailscale stable node ID the key is bound to.
        #[arg(long)]
        node: String,
        /// A location whose main it reads and which it may fork (repeatable).
        #[arg(long = "location")]
        locations: Vec<String>,
        /// Another fork it may read, if it is a fork of a `--location` (repeatable).
        #[arg(long = "fork")]
        forks: Vec<String>,
    },
    /// Revoke a principal: its key stops working and the log refuses its writes.
    Revoke { principal: String },
    /// List the keys (never their tokens).
    List,
}

#[derive(Subcommand)]
enum LocationAct {
    /// Register a location: empty, a git repository's tree, or any directory on the
    /// authority's host.
    Add {
        name: String,
        /// The git remote it lands to.
        #[arg(long)]
        remote: Option<String>,
        /// A git repository on the authority's host.
        #[arg(long, conflicts_with = "dir")]
        git: Option<PathBuf>,
        /// With --git: the revision whose tree becomes main.
        #[arg(long, default_value = "HEAD", requires = "git")]
        rev: String,
        /// A directory on the authority's host, snapshotted under its `.forkignore`.
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

fn named<T, E: std::fmt::Display>(
    s: String,
    f: impl FnOnce(String) -> Result<T, E>,
) -> anyhow::Result<T> {
    f(s).map_err(|e| anyhow::anyhow!("{e}"))
}

pub fn run(a: Admin) -> anyhow::Result<Report> {
    let req = match a.act {
        Act::Key(Key::Mint {
            principal,
            kind,
            node,
            locations,
            forks,
        }) => AdminRequest::KeyMint {
            principal: named(principal, PrincipalName::new)?,
            kind: match kind {
                Kind::Agent => PrincipalKind::Agent,
                Kind::OwnerDevice => PrincipalKind::OwnerDevice,
            },
            node: named(node, NodeId::new)?,
            grant: Grant {
                locations: locations
                    .into_iter()
                    .map(|l| named(l, LocationName::new))
                    .collect::<anyhow::Result<_>>()?,
                forks: forks
                    .into_iter()
                    .map(|f| named(f, ForkName::new))
                    .collect::<anyhow::Result<_>>()?,
            },
        },
        Act::Key(Key::Revoke { principal }) => AdminRequest::KeyRevoke {
            principal: named(principal, PrincipalName::new)?,
        },
        Act::Key(Key::List) => AdminRequest::KeyList,
        Act::Location(LocationAct::Add {
            name,
            remote,
            git,
            rev,
            dir,
        }) => AdminRequest::LocationAdd {
            name: named(name, LocationName::new)?,
            remote,
            git: git.map(|repo| GitSource { repo, rev }),
            dir: dir.map(std::fs::canonicalize).transpose()?,
        },
        Act::Land { fork, changeset } => AdminRequest::Land {
            fork: named(fork, ForkName::new)?,
            changeset: changeset
                .map(|c| named(c, |c| ChangesetId::from_hex(&c)))
                .transpose()?,
        },
        Act::OwnerFork { location, fork } => AdminRequest::OwnerFork {
            location: named(location, LocationName::new)?,
            fork: named(fork, ForkName::new)?,
        },
        Act::TakeOver {
            fork,
            expected_epoch,
            principal,
            node,
        } => AdminRequest::TakeOver {
            fork: named(fork, ForkName::new)?,
            expected_epoch,
            principal: named(principal, PrincipalName::new)?,
            node: named(node, NodeId::new)?,
        },
    };
    let answer = call(&a.socket, &req)
        .map_err(|e| anyhow::anyhow!("admin socket {}: {e}", a.socket.display()))?;
    if answer["ok"] != true {
        bail!(
            "the authority refused: {}",
            answer["error"].as_str().unwrap_or("no reason given")
        );
    }
    let text = match answer.get("token").and_then(|t| t.as_str()) {
        // The one place a key is shown: give it to the principal, then it is gone.
        Some(token) => format!("{token}\n"),
        None => format!("{}\n", serde_json::to_string_pretty(&answer)?),
    };
    Ok(Report {
        json: answer,
        text,
        exit: 0,
    })
}
