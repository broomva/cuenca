//! The agent verbs (spec rev 2.2 section 0), shared by the CLI and the MCP server.
//!
//! Each returns a [`Report`]: the JSON an agent reads (`--json`, or an MCP tool's
//! result) and the text a person reads. None of them is an owner act: minting keys,
//! registering locations, taking a fork over and landing a location without a remote
//! are `cuenca admin`, which this module cannot reach.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use cuenca_core::{ChangesetId, Event, ForkName, LocationName};
use cuenca_fs::{BlobSource, Fetched, SnapshotOptions};
use cuenca_tree::{Changeset, Entry, GitOid, Op, Tree};
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::Client;
use crate::local::{Flush, Local, State};

/// What a verb did: `json` for agents, `text` for people. `exit` is the process's exit
/// status: 0 done, 1 refused or failed, 2 unavailable (the verb is not built yet).
#[derive(Debug, Clone)]
pub struct Report {
    pub json: Value,
    pub text: String,
    pub exit: i32,
}

impl Report {
    fn ok(json: Value, text: impl Into<String>) -> Self {
        Self {
            json,
            text: text.into(),
            exit: 0,
        }
    }
}

/// One path a changeset changes.
#[derive(Debug, Clone, Serialize)]
pub struct Change {
    pub path: String,
    /// `added`, `modified` (content), `mode` (kind or executable bit only), `deleted`.
    pub change: &'static str,
    /// The entry's kind afterwards: `file`, `executable`, `symlink` (absent on delete).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
}

fn kind(e: &Entry) -> &'static str {
    match e {
        Entry::File(_) => "file",
        Entry::Executable(_) => "executable",
        Entry::Symlink(_) => "symlink",
        _ => "other",
    }
}

/// What `changeset` does to `base`, path by path.
pub fn changes(base: &Tree, changeset: &Changeset) -> Vec<Change> {
    changeset
        .ops
        .iter()
        .map(|op| match op {
            Op::Put { path, entry } => Change {
                path: path.as_str().to_string(),
                change: match base.get(path.as_str()) {
                    None => "added",
                    Some(old) if old.oid() == entry.oid() => "mode",
                    Some(_) => "modified",
                },
                kind: Some(kind(entry)),
            },
            Op::Delete { path } => Change {
                path: path.as_str().to_string(),
                change: "deleted",
                kind: None,
            },
            _ => Change {
                path: op.path().as_str().to_string(),
                change: "other",
                kind: None,
            },
        })
        .collect()
}

fn change_lines(changes: &[Change]) -> String {
    changes
        .iter()
        .map(|c| {
            let mark = match c.change {
                "added" => "A",
                "modified" => "M",
                "mode" => "T",
                "deleted" => "D",
                _ => "?",
            };
            format!("{mark} {}\n", c.path)
        })
        .collect()
}

fn fork_name(s: &str) -> anyhow::Result<ForkName> {
    ForkName::new(s).map_err(|e| anyhow::anyhow!("{e}"))
}

fn location_name(s: &str) -> anyhow::Result<LocationName> {
    LocationName::new(s).map_err(|e| anyhow::anyhow!("{e}"))
}

/// The checkout holding `dir`, which must have been made from this client's authority.
fn open(client: &Client, dir: &Path) -> anyhow::Result<Local> {
    let local = Local::find(dir)?;
    if local.state.url != client.url() {
        bail!(
            "{} was checked out from {}, not {}",
            local.root().display(),
            local.state.url,
            client.url()
        );
    }
    Ok(local)
}

/// `cuenca forks`: the locations and forks the caller's key may read.
pub fn forks(client: &Client) -> anyhow::Result<Report> {
    let locations = client.locations()?;
    let forks = client.forks()?;
    let mut text = String::from("locations:\n");
    for l in &locations {
        text.push_str(&format!(
            "  {}  main {}{}\n",
            l.name,
            l.main,
            l.remote
                .as_deref()
                .map(|r| format!("  remote {r}"))
                .unwrap_or_default()
        ));
    }
    text.push_str("forks:\n");
    for f in &forks {
        let holder = f.holder().map(|h| h.to_string()).unwrap_or_default();
        text.push_str(&format!(
            "  {}  of {}  epoch {}  held by {}  head {}\n",
            f.name,
            f.location,
            f.epoch(),
            holder,
            f.head
        ));
    }
    Ok(Report::ok(
        json!({ "locations": locations, "forks": forks }),
        text,
    ))
}

/// `cuenca fork <location> [--name]`: a new fork of the location's main, held by the
/// caller under epoch 1. Without a name, `<location>-<principal>-<6 hex>`.
pub fn fork(client: &Client, location: &str, name: Option<&str>) -> anyhow::Result<Report> {
    let location = location_name(location)?;
    let name = match name {
        Some(n) => fork_name(n)?,
        None => {
            let me = client.whoami()?;
            let suffix: [u8; 3] = rand::random();
            fork_name(&format!(
                "{location}-{}-{}",
                me.principal,
                hex::encode(suffix)
            ))?
        }
    };
    let f = client.create_fork(&name, &location)?;
    let text = format!(
        "forked {} as {} (epoch {}, head {})\n",
        f.location,
        f.name,
        f.epoch(),
        f.head
    );
    Ok(Report::ok(json!({ "fork": f }), text))
}

/// Blobs of one fork's head, fetched from the authority as a checkout needs them.
struct Remote<'a> {
    client: &'a Client,
    fork: &'a ForkName,
}

impl BlobSource for Remote<'_> {
    fn fetch(&self, oid: GitOid) -> std::io::Result<Option<Fetched>> {
        self.client
            .fork_blob(self.fork, oid)
            .map(|b| b.map(Fetched::Bytes))
            .map_err(std::io::Error::other)
    }
}

/// `cuenca checkout <fork> <dir>`: materialise the fork's head in `dir` (missing, or
/// empty) and register it as a checkout of that fork.
pub fn checkout(client: &Client, fork: &str, dir: &Path) -> anyhow::Result<Report> {
    let name = fork_name(fork)?;
    let f = client.fork(&name)?;
    let (oid, tree) = client.fork_tree(&name)?;
    let source = Remote {
        client,
        fork: &name,
    };
    let done = cuenca_fs::checkout(&tree, dir, &source)?;
    let root = std::fs::canonicalize(dir)?;
    let local = Local::create(State {
        format: 1,
        root: root.clone(),
        url: client.url().to_string(),
        fork: name.clone(),
        location: f.location.clone(),
        epoch: f.epoch(),
        published: tree.clone(),
    })?;
    // The checkout's first snapshot reads every file; doing it now builds the stat cache
    // while nothing has changed yet.
    let mut cache = local.load_cache()?;
    let snap = cuenca_fs::snapshot(&root, &tree, &mut cache)?;
    local.save_cache(&cache)?;
    let text = format!(
        "checked out {name} ({} files, head {oid}) into {}\n",
        tree.len(),
        root.display()
    );
    Ok(Report::ok(
        json!({
            "fork": name,
            "location": f.location,
            "epoch": f.epoch(),
            "head": oid,
            "root": root,
            "files": tree.len(),
            "cloned": done.cloned,
            "copied": done.copied,
            "clean": snap.is_clean(),
        }),
        text,
    ))
}

/// `cuenca status`: the checkout's fork, what changed since its last snapshot, what is
/// queued, and the fork as the authority has it (when it answers).
pub fn status(client: Option<&Client>, dir: &Path) -> anyhow::Result<Report> {
    let local = Local::find(dir)?;
    let base = local.local_tree()?;
    let mut cache = local.load_cache()?;
    let snap = cuenca_fs::snapshot(local.root(), &base, &mut cache)?;
    local.save_cache(&cache)?;
    let changed = changes(&base, &snap.changeset);
    let queued = local.outbox()?.len();
    let remote = match client {
        None => json!({ "error": "not logged in" }),
        Some(c) => match c.fork(&local.state.fork) {
            Ok(f) => json!({
                "head": f.head,
                "epoch": f.epoch(),
                "holder": f.holder(),
                "landing_intent": f.landing_intent.map(|(id, _)| id),
            }),
            Err(e) => json!({ "error": e.to_string(), "offline": e.is_offline() }),
        },
    };
    let s = &local.state;
    let mut text = format!(
        "{} is a checkout of fork {} ({}), epoch {}\nacknowledged {}, {queued} snapshot(s) queued\n",
        s.root.display(),
        s.fork,
        s.location,
        s.epoch,
        s.published.git_tree_id(),
    );
    match remote.get("holder") {
        Some(holder) => text.push_str(&format!(
            "authority: head {}, epoch {}, held by {}@{}\n",
            remote["head"].as_str().unwrap_or("?"),
            remote["epoch"],
            holder["principal"].as_str().unwrap_or("?"),
            holder["node"].as_str().unwrap_or("?"),
        )),
        None => text.push_str(&format!(
            "authority: {}\n",
            remote["error"].as_str().unwrap_or("unknown")
        )),
    }
    if changed.is_empty() {
        text.push_str("no changes since the last snapshot\n");
    } else {
        text.push_str("changes since the last snapshot:\n");
        text.push_str(&change_lines(&changed));
    }
    Ok(Report::ok(
        json!({
            "root": s.root,
            "fork": s.fork,
            "location": s.location,
            "epoch": s.epoch,
            "acknowledged": s.published.git_tree_id(),
            "queued": queued,
            "changes": changed,
            "skipped": snap.skipped.iter().map(|k| json!({"path": k.path, "reason": k.reason})).collect::<Vec<_>>(),
            "authority": remote,
        }),
        text,
    ))
}

/// What `cuenca snapshot` may do besides the default.
#[derive(Debug, Clone, Copy, Default)]
pub struct SnapshotFlags {
    /// Accept that paths `.forkignore` now names are deleted from the fork.
    pub drop_ignored: bool,
}

/// Scan the checkout against its last snapshot, queue the result if anything changed,
/// then publish the outbox.
fn take_snapshot(
    client: &Client,
    local: &mut Local,
    flags: SnapshotFlags,
) -> anyhow::Result<(Vec<super::verbs::Change>, Flush)> {
    let base = local.local_tree()?;
    let mut cache = local.load_cache()?;
    let options = SnapshotOptions {
        drop_ignored: flags.drop_ignored,
        ..Default::default()
    };
    let snap = cuenca_fs::snapshot_with(local.root(), &base, &mut cache, &options)?;
    let changed = changes(&base, &snap.changeset);
    if !snap.is_clean() {
        local.queue(snap.changeset, snap.tree, &snap.new_blobs)?;
    }
    local.save_cache(&cache)?;
    Ok((changed, local.flush(client)))
}

fn snapshot_report(changed: &[Change], flush: &Flush) -> Report {
    let mut text = if changed.is_empty() {
        String::from("no changes\n")
    } else {
        format!(
            "snapshot: {} change(s)\n{}",
            changed.len(),
            change_lines(changed)
        )
    };
    for p in &flush.published {
        text.push_str(&format!(
            "published draft {} to {}{}\n",
            p.id,
            p.fork,
            if p.fenced {
                " (the fork was taken over: this draft is kept in a sibling fork)"
            } else {
                ""
            }
        ));
    }
    if flush.pending > 0 {
        text.push_str(&format!(
            "{} snapshot(s) queued{}\n",
            flush.pending,
            if flush.offline {
                ": the authority is unreachable; they publish with the next snapshot"
            } else {
                ""
            }
        ));
    }
    if let Some(e) = &flush.error {
        text.push_str(&format!("not published: {e}\n"));
    }
    Report {
        json: json!({ "changes": changed, "flush": flush }),
        text,
        // Queued because the authority is unreachable is not a failure: nothing is lost.
        exit: i32::from(flush.error.is_some() && !flush.offline),
    }
}

/// `cuenca snapshot`: publish a draft of the checkout.
pub fn snapshot(client: &Client, dir: &Path, flags: SnapshotFlags) -> anyhow::Result<Report> {
    let mut local = open(client, dir)?;
    let (changed, flush) = take_snapshot(client, &mut local, flags)?;
    Ok(snapshot_report(&changed, &flush))
}

/// How `cuenca snapshot --watch` paces itself (spec section 2a: 2 s after changes stop,
/// and at least every 10 s while they continue).
#[derive(Debug, Clone, Copy)]
pub struct Watch {
    /// Snapshot once the checkout has been unchanged this long.
    pub settle: Duration,
    /// Snapshot at least this often while changes continue.
    pub max_wait: Duration,
    /// How often the checkout is scanned.
    pub poll: Duration,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(2),
            max_wait: Duration::from_secs(10),
            poll: Duration::from_millis(500),
        }
    }
}

/// `cuenca snapshot --watch`: scan every `poll`; once the checkout differs from its last
/// snapshot, snapshot when it has stopped changing for `settle` or has kept changing for
/// `max_wait`. While snapshots are queued, retry publishing every `max_wait`. Runs until
/// the process is stopped; each snapshot's report goes to `on`.
pub fn watch(
    client: &Client,
    dir: &Path,
    flags: SnapshotFlags,
    pace: Watch,
    mut on: impl FnMut(Report),
) -> anyhow::Result<()> {
    let root: PathBuf = open(client, dir)?.root().to_path_buf();
    let mut last_seen: Option<GitOid> = None;
    let mut first_change: Option<Instant> = None;
    let mut last_change = Instant::now();
    let mut last_flush = Instant::now();
    loop {
        std::thread::sleep(pace.poll);
        // The lock is taken per scan, so other verbs run between scans.
        let mut local = open(client, &root)?;
        let base = local.local_tree()?;
        let mut cache = local.load_cache()?;
        let options = SnapshotOptions {
            drop_ignored: flags.drop_ignored,
            ..Default::default()
        };
        let now = Instant::now();
        let target = match cuenca_fs::snapshot_with(&root, &base, &mut cache, &options) {
            Ok(snap) => snap.tree.git_tree_id(),
            Err(e) => {
                // A file mid-write fails the scan as unstable: look again next poll.
                tracing_note(&mut on, &format!("scan: {e}"));
                continue;
            }
        };
        if target == base.git_tree_id() {
            first_change = None;
            last_seen = Some(target);
            if !local.outbox()?.is_empty() && now - last_flush >= pace.max_wait {
                last_flush = now;
                let flush = local.flush(client);
                on(snapshot_report(&[], &flush));
            }
            continue;
        }
        if last_seen != Some(target) {
            last_seen = Some(target);
            last_change = now;
            first_change.get_or_insert(now);
        }
        let settled = now - last_change >= pace.settle;
        let overdue = first_change.is_some_and(|t| now - t >= pace.max_wait);
        if settled || overdue {
            first_change = None;
            last_flush = now;
            match take_snapshot(client, &mut local, flags) {
                Ok((changed, flush)) => on(snapshot_report(&changed, &flush)),
                Err(e) => tracing_note(&mut on, &format!("snapshot: {e:#}")),
            }
        }
    }
}

fn tracing_note(on: &mut impl FnMut(Report), message: &str) {
    on(Report {
        json: json!({ "note": message }),
        text: format!("{message}\n"),
        exit: 0,
    });
}

/// A tree named on the command line: a fork, or a location's main as `location:<name>`.
fn named_tree(client: &Client, name: &str) -> anyhow::Result<(GitOid, Tree)> {
    Ok(match name.strip_prefix("location:") {
        Some(l) => client.location_tree(&location_name(l)?)?,
        None => client.fork_tree(&fork_name(name)?)?,
    })
}

/// `cuenca diff [<a> <b>]`: with no trees named, what changed in the checkout since its
/// last snapshot; otherwise what turns tree `a` into tree `b`, each a fork or
/// `location:<name>`. Paths only: content is compared by blob id.
pub fn diff(client: &Client, dir: &Path, between: Option<(&str, &str)>) -> anyhow::Result<Report> {
    let (from, to, changeset, base) = match between {
        Some((a, b)) => {
            let (a_oid, a_tree) = named_tree(client, a)?;
            let (b_oid, b_tree) = named_tree(client, b)?;
            let cs = Changeset::between(&a_tree, &b_tree);
            (a_oid.to_hex(), b_oid.to_hex(), cs, a_tree)
        }
        None => {
            let local = open(client, dir)?;
            let base = local.local_tree()?;
            let mut cache = local.load_cache()?;
            let snap = cuenca_fs::snapshot(local.root(), &base, &mut cache)?;
            local.save_cache(&cache)?;
            (
                base.git_tree_id().to_hex(),
                "working directory".to_string(),
                snap.changeset,
                base,
            )
        }
    };
    let changed = changes(&base, &changeset);
    let text = if changed.is_empty() {
        "no differences\n".to_string()
    } else {
        change_lines(&changed)
    };
    Ok(Report::ok(
        json!({ "from": from, "to": to, "changes": changed }),
        text,
    ))
}

/// One log record, with drafts shown by their id and size rather than whole.
fn summarise(record: &cuenca_core::Record) -> (Value, String) {
    let mut v = serde_json::to_value(&record.event).unwrap_or(Value::Null);
    if let Some(cs) = v.get_mut("changeset").filter(|c| c.is_object()) {
        let ops = cs["ops"].as_array().map_or(0, Vec::len);
        *cs = json!({ "base": cs["base"], "target": cs["target"], "ops": ops });
    }
    let line = match &record.event {
        Event::ForkCreated {
            fork,
            location,
            holder,
            ..
        } => {
            format!("fork {fork} of {location} created, held by {holder}")
        }
        Event::DraftPublished {
            fork,
            writer,
            id,
            changeset,
        } => format!(
            "draft {id} on {fork} by {} (epoch {}): {} change(s)",
            writer.holder,
            writer.epoch,
            changeset.ops.len()
        ),
        Event::DraftFenced {
            fork, sibling, id, ..
        } => {
            format!("draft {id} for {fork} written under a stale epoch, kept in {sibling}")
        }
        Event::TakenOver {
            fork,
            expected_epoch,
            holder,
        } => format!(
            "{fork} taken over by {holder} (epoch {} -> {})",
            expected_epoch,
            expected_epoch + 1
        ),
        Event::MainAdvanced {
            location,
            fork,
            changeset,
        } => {
            format!("main of {location} advanced to {changeset} from {fork}")
        }
        Event::LandingIntent {
            fork, changeset, ..
        } => {
            format!("asked to land {changeset} from {fork}")
        }
        other => format!("{other:?}"),
    };
    (
        json!({ "offset": record.offset, "at_ms": record.at_ms, "event": v }),
        format!("{:>6}  {line}\n", record.offset),
    )
}

/// `cuenca log [<fork>]`: the log records naming the fork (the checkout's by default).
pub fn log(client: &Client, dir: &Path, fork: Option<&str>) -> anyhow::Result<Report> {
    let name = match fork {
        Some(f) => fork_name(f)?,
        None => open(client, dir)?.state.fork.clone(),
    };
    let records = client.history(&name)?;
    let (json, text): (Vec<_>, String) = records.iter().map(summarise).unzip();
    Ok(Report::ok(json!({ "fork": name, "records": json }), text))
}

/// `cuenca verify`: run the location's checks, read from the base tree, on the
/// checkout's head. Not built yet (BRO-3254): it says so and exits 2.
pub fn verify(client: &Client, dir: &Path) -> anyhow::Result<Report> {
    let local = open(client, dir)?;
    let reason = "verify is not built yet: checks read from the base tree arrive with BRO-3254";
    Ok(Report {
        json: json!({ "fork": local.state.fork, "status": "unavailable", "reason": reason }),
        text: format!("{reason}\n"),
        exit: 2,
    })
}

/// `cuenca land`: ask to land the checkout's fork at its head. Everything queued must be
/// published first, and the authority's head must be the checkout's last snapshot, so
/// what is asked to land is what the checkout holds. Landing itself (the PR, the gates)
/// is the authority's (BRO-3255); this records the request.
pub fn land(client: &Client, dir: &Path) -> anyhow::Result<Report> {
    let mut local = open(client, dir)?;
    let flush = local.flush(client);
    if let Some(e) = flush.error {
        bail!(
            "{} snapshot(s) are still queued, so nothing was asked to land: {e}",
            flush.pending
        );
    }
    let f = client.fork(&local.state.fork)?;
    let ours = local.state.published.git_tree_id();
    if f.head != ours {
        bail!(
            "the fork's head is {}, not this checkout's last snapshot {ours}; nothing was asked to land",
            f.head
        );
    }
    let Some((id, _)) = f.head_changeset else {
        bail!("{} has no draft to land", f.name);
    };
    let offset = client
        .landing(&f.name, local.state.epoch, id)
        .with_context(|| format!("asking to land {id}"))?;
    Ok(Report::ok(
        json!({ "fork": f.name, "changeset": id, "offset": offset, "status": "requested" }),
        format!(
            "asked to land {id} from {}; landing itself arrives with BRO-3255\n",
            f.name
        ),
    ))
}

/// The id of a changeset, as the authority names it.
pub fn id_of(changeset: &Changeset) -> ChangesetId {
    ChangesetId::of(changeset)
}
