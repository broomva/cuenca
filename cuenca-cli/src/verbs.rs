//! The agent verbs (spec rev 2.2 section 0), shared by the CLI and the MCP server.
//!
//! Each returns a [`Report`]: the JSON an agent reads (`--json`, or an MCP tool's
//! result) and the text a person reads. None of them is an owner act: minting keys,
//! registering locations, taking a fork over and landing a location without a remote
//! are `cuenca admin`, which this module cannot reach.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};
use cuenca_core::checks::{CheckRun, CheckStatus, MANIFEST_PATH, Manifest, ManifestId};
use cuenca_core::{Event, ForkName, LocationName};
use cuenca_fs::{BlobSource, Fetched, SnapshotOptions};
use cuenca_tree::{Changeset, ContentHash, Entry, GitOid, Op, Tree};
use serde::Serialize;
use serde_json::{Value, json};

use crate::client::{Client, ClientError};
use crate::local::{Flush, Local, State};
use crate::run;

/// What a verb did: `json` for agents, `text` for people. `exit` is the process's exit
/// status: 0 done, 1 refused or failed, 2 unavailable (the verb is not built yet),
/// 3 verify ran its checks but the authority did not record them.
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

/// What a scan that publishes nothing (checkout, status, diff, the watch probe) found:
/// the changes a snapshot would publish, and the base paths `.forkignore` names, which a
/// snapshot refuses to delete unless told `--drop-ignored`. Such a scan never fails on
/// those paths: it reports them.
struct Look {
    changes: Vec<Change>,
    ignored: Vec<String>,
    /// Entries that are not content (sockets, FIFOs, devices).
    skipped: Vec<Value>,
    target: GitOid,
}

fn look(root: &Path, base: &Tree, cache: &mut cuenca_fs::StatCache) -> anyhow::Result<Look> {
    let options = SnapshotOptions {
        drop_ignored: true,
        ..Default::default()
    };
    let snap = cuenca_fs::snapshot_with(root, base, cache, &options)?;
    let ignored = snap.dropped;
    let changes = changes(base, &snap.changeset)
        .into_iter()
        .filter(|c| !ignored.contains(&c.path))
        .collect();
    Ok(Look {
        changes,
        ignored,
        skipped: snap
            .skipped
            .iter()
            .map(|k| json!({ "path": k.path, "reason": k.reason }))
            .collect(),
        target: snap.tree.git_tree_id(),
    })
}

fn ignored_note(ignored: &[String]) -> String {
    match ignored.first() {
        None => String::new(),
        Some(first) => format!(
            "{} path(s) of the fork are named by .forkignore (first: {first}); a snapshot \
             refuses to delete them unless run with --drop-ignored\n",
            ignored.len()
        ),
    }
}

/// A snapshot's scan error, in the CLI's terms.
fn scan_error(e: cuenca_fs::FsError) -> anyhow::Error {
    match e {
        cuenca_fs::FsError::Ignored(paths) => anyhow::anyhow!(
            "the fork holds {} path(s) .forkignore names (first: {}); nothing was \
             snapshotted. `cuenca snapshot --drop-ignored` deletes them from the fork \
             (they stay on disk)",
            paths.len(),
            paths.first().map_or("", String::as_str)
        ),
        e => e.into(),
    }
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
    let root = Local::prepare(dir)?;
    let f = client.fork(&name)?;
    let (oid, tree) = client.fork_tree(&name)?;
    let source = Remote {
        client,
        fork: &name,
    };
    let done = cuenca_fs::checkout(&tree, &root, &source)?;
    let local = Local::create(State {
        format: 1,
        root: root.clone(),
        root_id: crate::local::identity(&root)?,
        url: client.url().to_string(),
        fork: name.clone(),
        location: f.location.clone(),
        epoch: f.epoch(),
        published: tree.clone(),
    })?;
    // The checkout's first snapshot reads every file; doing it now builds the stat cache
    // while nothing has changed yet.
    let mut cache = local.load_cache()?;
    let seen = look(&root, &tree, &mut cache)?;
    local.save_cache(&cache)?;
    let text = format!(
        "checked out {name} ({} files, head {oid}) into {}\n{}",
        tree.len(),
        root.display(),
        ignored_note(&seen.ignored)
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
            "clean": seen.changes.is_empty(),
            "ignored": seen.ignored,
        }),
        text,
    ))
}

/// `cuenca status`: the checkout's fork, what changed since its last snapshot, what is
/// queued, and the fork as the authority has it (when it answers).
pub fn status(client: Option<&Client>, dir: &Path) -> anyhow::Result<Report> {
    let local = Local::find(dir)?;
    let client = client.filter(|c| c.url() == local.state.url);
    let base = local.local_tree()?;
    let mut cache = local.load_cache()?;
    let seen = look(local.root(), &base, &mut cache)?;
    local.save_cache(&cache)?;
    let changed = seen.changes;
    let queued = local.outbox()?.len();
    let remote = match client {
        None => json!({ "error": format!("not logged in to {}", local.state.url) }),
        Some(c) => match c.fork(&local.state.fork) {
            Ok(f) => json!({
                "head": f.head,
                "epoch": f.epoch(),
                "holder": f.holder(),
                "landing": f.landing,
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
        Some(holder) => {
            text.push_str(&format!(
                "authority: head {}, epoch {}, held by {}@{}\n",
                remote["head"].as_str().unwrap_or("?"),
                remote["epoch"],
                holder["principal"].as_str().unwrap_or("?"),
                holder["node"].as_str().unwrap_or("?"),
            ));
            if let Some(l) = remote.get("landing").filter(|l| !l.is_null()) {
                text.push_str(&format!(
                    "landing {}: {}\n",
                    l["changeset"].as_str().unwrap_or("?"),
                    step_line(&l["step"])
                ));
            }
        }
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
    text.push_str(&ignored_note(&seen.ignored));
    Ok(Report::ok(
        json!({
            "root": s.root,
            "fork": s.fork,
            "location": s.location,
            "epoch": s.epoch,
            "acknowledged": s.published.git_tree_id(),
            "queued": queued,
            "changes": changed,
            "ignored": seen.ignored,
            "skipped": seen.skipped,
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
    let snap =
        cuenca_fs::snapshot_with(local.root(), &base, &mut cache, &options).map_err(scan_error)?;
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
    // Since when the checkout has had changes no snapshot could take (a scan or a
    // snapshot failed: a file that keeps changing while it is read), and when that was
    // last reported as a failure. Cleared by a clean scan or a snapshot taken.
    let mut stuck: Option<(Instant, Instant)> = None;
    // A scan or snapshot failed: look again next poll. Once that has lasted `max_wait`, a
    // snapshot is overdue: say so, as a failure, every `max_wait` until one is taken
    // (spec section 2a's "at least every 10 s" cannot hold while a file never holds
    // still; ignore it in `.forkignore`).
    loop {
        std::thread::sleep(pace.poll);
        // The lock is taken per scan, so other verbs run between scans.
        let mut local = open(client, &root)?;
        let base = local.local_tree()?;
        let mut cache = local.load_cache()?;
        let now = Instant::now();
        // The probe publishes nothing; base paths `.forkignore` names show as a change
        // here, and the snapshot that follows refuses them unless --drop-ignored, which
        // `failed` then reports.
        let (target, clean) = match look(&root, &base, &mut cache) {
            // Base paths `.forkignore` names are no change unless --drop-ignored is set:
            // `status` calls such a checkout clean, and so does the watch.
            Ok(seen) => (
                seen.target,
                seen.changes.is_empty() && (seen.ignored.is_empty() || !flags.drop_ignored),
            ),
            Err(e) => {
                failed(
                    &mut stuck,
                    now,
                    pace.max_wait,
                    format!("scan: {e}"),
                    &mut on,
                );
                continue;
            }
        };
        if clean {
            stuck = None;
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
                Ok((changed, flush)) => {
                    stuck = None;
                    on(snapshot_report(&changed, &flush));
                }
                Err(e) => failed(
                    &mut stuck,
                    now,
                    pace.max_wait,
                    format!("snapshot: {e:#}"),
                    &mut on,
                ),
            }
        }
    }
}

/// A watch scan or snapshot failed: look again next poll. Once that has lasted
/// `max_wait`, a snapshot is overdue: say so, as a failure, every `max_wait` until one is
/// taken (spec section 2a's "at least every 10 s" cannot hold while a file never holds
/// still; ignore it in `.forkignore`).
fn failed(
    stuck: &mut Option<(Instant, Instant)>,
    now: Instant,
    max_wait: Duration,
    e: String,
    on: &mut impl FnMut(Report),
) {
    let (since, reported) = *stuck.get_or_insert((now, now));
    if now - since >= max_wait && now - reported >= max_wait {
        *stuck = Some((since, now));
        let message = format!("no snapshot for {:.0} s: {e}", (now - since).as_secs_f64());
        on(Report {
            json: json!({ "error": message, "overdue": true }),
            text: format!("{message}\n"),
            exit: 1,
        });
    } else {
        tracing_note(on, &e);
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
    let (from, to, changed, ignored) = match between {
        Some((a, b)) => {
            let (a_oid, a_tree) = named_tree(client, a)?;
            let (b_oid, b_tree) = named_tree(client, b)?;
            let cs = Changeset::between(&a_tree, &b_tree);
            (
                a_oid.to_hex(),
                b_oid.to_hex(),
                changes(&a_tree, &cs),
                vec![],
            )
        }
        None => {
            let local = open(client, dir)?;
            let base = local.local_tree()?;
            let mut cache = local.load_cache()?;
            let seen = look(local.root(), &base, &mut cache)?;
            local.save_cache(&cache)?;
            (
                base.git_tree_id().to_hex(),
                "working directory".to_string(),
                seen.changes,
                seen.ignored,
            )
        }
    };
    let mut text = if changed.is_empty() {
        "no differences\n".to_string()
    } else {
        change_lines(&changed)
    };
    text.push_str(&ignored_note(&ignored));
    Ok(Report::ok(
        json!({ "from": from, "to": to, "changes": changed, "ignored": ignored }),
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
        Event::ExportIntent {
            fork,
            changeset,
            commit,
            branch,
            ..
        } => format!("exporting {changeset} from {fork} as commit {commit} to {branch}"),
        Event::Exported { fork, changeset } => {
            format!("exported {changeset} from {fork}")
        }
        Event::ReviewIntent { fork, changeset } => {
            format!("opening a pull request for {changeset} from {fork}")
        }
        Event::InReview {
            fork,
            changeset,
            url,
            ..
        } => format!("{changeset} from {fork} is in review: {url}"),
        Event::LandingFailed {
            fork,
            changeset,
            reason,
        } => format!("landing {changeset} from {fork} failed: {reason}"),
        Event::VerifyRecorded {
            fork,
            changeset,
            checks,
            manifest,
            ..
        } => {
            let passed = checks
                .iter()
                .filter(|c| c.status == CheckStatus::Passed)
                .count();
            format!(
                "verify of {changeset} on {fork}: {passed} of {} check(s) passed ({})",
                checks.len(),
                manifest.map_or("no manifest".to_string(), |m| format!("manifest {}", m.oid))
            )
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

/// `cuenca verify`: run the checks the location's main declares
/// ([`cuenca_core::checks`], `.cuenca/checks.toml` of the base, never the candidate's) on
/// the fork's head, in a temporary checkout of it, and have the authority record how
/// they ended beside the head changeset. Everything queued is published first, and the
/// authority's head must be the checkout's last snapshot, so what is verified is what
/// the checkout holds (edits since the last snapshot are not). Exits 0 when every check
/// passed (a base that declares none passes with zero), 1 otherwise.
///
/// Advisory (spec rev 2.2 section 6): the results are the caller's own and a check
/// reads the candidate's files, so the record blocks nothing; GitHub's required checks
/// are the gate at landing.
pub fn verify(client: &Client, dir: &Path) -> anyhow::Result<Report> {
    let mut local = open(client, dir)?;
    let flush = local.flush(client);
    if let Some(e) = flush.error {
        bail!(
            "{} snapshot(s) are still queued, so nothing was verified: {e}",
            flush.pending
        );
    }
    let f = client.fork(&local.state.fork)?;
    let ours = local.state.published.git_tree_id();
    if f.head != ours {
        bail!(
            "the fork's head is {}, not this checkout's last snapshot {ours}; nothing was verified",
            f.head
        );
    }
    let Some((id, _)) = f.head_changeset else {
        bail!("{} has no draft to verify", f.name);
    };

    let (base, main) = client.location_tree(&f.location)?;
    let manifest_bytes = match main.get(MANIFEST_PATH) {
        None => None,
        Some(Entry::File(oid) | Entry::Executable(oid)) => Some(
            client
                .location_blob(&f.location, *oid)?
                .with_context(|| format!("the main of {} moved while it was read", f.location))?,
        ),
        Some(_) => bail!(
            "{MANIFEST_PATH} in the base {base} of {} is not a regular file",
            f.location
        ),
    };
    let manifest = match &manifest_bytes {
        None => Manifest::default(),
        Some(bytes) => {
            Manifest::parse(bytes).with_context(|| format!("the base {base} of {}", f.location))?
        }
    };

    // The candidate as the authority has it, in a directory of this verify's own.
    let scratch = tempfile::Builder::new()
        .prefix("cuenca-verify-")
        .tempdir()?;
    let root = scratch.path().join("candidate");
    let (head, tree) = client.fork_tree(&f.name)?;
    if head != f.head {
        bail!("{} moved while it was read; nothing was verified", f.name);
    }
    let source = Remote {
        client,
        fork: &f.name,
    };
    cuenca_fs::checkout(&tree, &root, &source)?;
    let ran: Vec<run::Ran> = manifest
        .checks
        .iter()
        .map(|c| run::check(c, &root))
        .collect();
    drop(scratch);

    let runs: Vec<CheckRun> = ran.iter().map(|r| r.run.clone()).collect();
    // A refused or failed record (the head moved, a takeover, a revoked key, the
    // authority gone) must not cost the caller the run: the results are still reported,
    // marked unrecorded, with exit 3.
    let recorded = client.verdict(&f.name, local.state.epoch, id, base, &runs);
    let passed = runs
        .iter()
        .filter(|r| r.status == CheckStatus::Passed)
        .count();
    let all = passed == runs.len();
    let (offset, manifest_id, record_error) = match &recorded {
        Ok((offset, m)) => (Some(*offset), *m, None),
        Err(e) => (
            None,
            manifest_bytes.as_deref().map(|b| ManifestId {
                oid: GitOid::for_blob(b),
                sha256: ContentHash::of(b),
            }),
            Some(e),
        ),
    };

    let mut text = format!(
        "verify of {id} on {}: {passed} of {} check(s) passed, judged by {}\n",
        f.name,
        runs.len(),
        match &manifest_id {
            Some(m) => format!("{MANIFEST_PATH} {} of base {base}", m.oid),
            None => format!("base {base}, which declares no checks"),
        }
    );
    let mut checks = Vec::new();
    for r in &ran {
        let c = &r.run;
        let verdict = match c.status {
            CheckStatus::Passed => "pass".to_string(),
            CheckStatus::Failed => match c.exit_code {
                Some(code) => format!("FAIL (exit {code})"),
                None => "FAIL (killed by a signal)".to_string(),
            },
            CheckStatus::TimedOut => format!("FAIL (timed out after {} s)", c.check.timeout_secs),
            CheckStatus::NotRun => "FAIL (not run)".to_string(),
        };
        text.push_str(&format!(
            "  {verdict}  {}  {} ms\n",
            c.check.name, c.duration_ms
        ));
        if c.status != CheckStatus::Passed {
            for line in r.output.lines() {
                text.push_str(&format!("      {line}\n"));
            }
        }
        let mut v = serde_json::to_value(c)?;
        v["output"] = json!(r.output);
        checks.push(v);
    }
    match (offset, record_error) {
        (Some(offset), _) => text.push_str(&format!(
            "recorded at log offset {offset}; advisory, it blocks nothing\n"
        )),
        (None, Some(e)) => text.push_str(&format!(
            "NOT recorded: the authority did not take the record of {id}: {e}\n"
        )),
        (None, None) => unreachable!("a verify is either recorded or has a record error"),
    }
    Ok(Report {
        json: json!({
            "fork": f.name,
            "changeset": id,
            "candidate": f.head,
            "base": base,
            "manifest": manifest_id,
            "checks": checks,
            "passed": all,
            "recorded": offset.is_some(),
            "offset": offset,
            "record_error": record_error.map(client_error),
        }),
        text,
        exit: match (offset, all) {
            (None, _) => 3,
            (Some(_), all) => i32::from(!all),
        },
    })
}

/// A client error as structured JSON, the shape MCP tools report errors in.
pub(crate) fn client_error(e: &ClientError) -> Value {
    let mut v = json!({ "message": e.to_string() });
    match e {
        ClientError::Api { status, code, .. } => {
            v["code"] = json!(code);
            v["status"] = json!(status);
        }
        ClientError::Offline { .. } => v["offline"] = json!(true),
        ClientError::Other(_) => {}
    }
    v
}

/// One line for a landing step (`{"step": .., ..}` as the authority serializes it).
fn step_line(step: &Value) -> String {
    match step["step"].as_str().unwrap_or("?") {
        "requested" => "requested".to_string(),
        "in_review" => format!(
            "in review, pull request {} ({})",
            step["pr"],
            step["url"].as_str().unwrap_or("?")
        ),
        "landed" => format!("landed (main moved at record {})", step["offset"]),
        "failed" => format!("failed: {}", step["reason"].as_str().unwrap_or("?")),
        other => format!("{other} (the authority is exporting it)"),
    }
}

/// `cuenca land`: ask to land the checkout's fork at its head, which must be verified
/// against the location's main (`cuenca verify`; its result does not matter, GitHub's
/// required checks or the owner decide). Everything queued must be published first,
/// and the authority's head must be the checkout's last snapshot, so what is asked to
/// land is what the checkout holds.
///
/// On a location with a git remote the authority exports the draft as a branch and a
/// pull request; this waits up to `wait` for the pull request and reports it, or the
/// step the landing has reached. On a location without one the request waits for the
/// owner's `cuenca admin land`. Exits 1 if the landing failed.
pub fn land(client: &Client, dir: &Path, wait: std::time::Duration) -> anyhow::Result<Report> {
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
    let deadline = std::time::Instant::now() + wait;
    let state = loop {
        let state = client.landing_state(&f.name)?;
        let step = state["landing"]["step"]["step"].as_str().unwrap_or("");
        let settled = state["landing"]["changeset"] != json!(id)
            || matches!(step, "in_review" | "landed" | "failed")
            || state["remote"] != true;
        if settled || std::time::Instant::now() >= deadline {
            break state;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    };
    let landing = &state["landing"];
    let step = &landing["step"];
    // A takeover (or the holder's newer request) dropped this one while it waited.
    let status = if landing["changeset"] != json!(id) {
        "dropped".to_string()
    } else {
        step["step"].as_str().unwrap_or("unknown").to_string()
    };
    let waiting_for = match status.as_str() {
        "requested" if state["remote"] != true => Some("owner"),
        "requested" | "exporting" | "exported" | "reviewing" => Some("authority"),
        _ => None,
    };
    let mut text = format!("asked to land {id} from {}: {}\n", f.name, step_line(step));
    match waiting_for {
        Some("owner") => text.push_str(
            "the location has no git remote: the owner lands it with `cuenca admin land`\n",
        ),
        Some(_) => text.push_str("the authority is still exporting it; see `cuenca status`\n"),
        None => {}
    }
    if let Some(e) = state["error"].as_str() {
        text.push_str(&format!("the authority is retrying after: {e}\n"));
    }
    Ok(Report {
        exit: if matches!(status.as_str(), "failed" | "dropped") {
            1
        } else {
            0
        },
        json: json!({
            "fork": f.name,
            "changeset": id,
            "offset": offset,
            "status": status,
            "waiting_for": waiting_for,
            "pr": step.get("pr"),
            "url": step.get("url"),
            "reason": step.get("reason"),
            "retrying_after": state["error"],
        }),
        text,
    })
}
