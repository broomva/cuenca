//! The log's transition rules, in one place.
//!
//! [`apply`] is the only code that decides whether an event may follow the state the
//! log is in, and what state it leaves. Admission checks that need blob or tree
//! content (blob presence, content rules, a draft's ops and target) are the writer's,
//! in `store.rs`, and run before an event reaches these rules. The store's writer runs it against the redb
//! tables inside the transaction that appends the event; [`crate::Store::replay_check`]
//! runs it against an empty in-memory [`Memory`] for every record in order. The two
//! cannot disagree about a rule, because there is one copy of each.

use std::collections::{BTreeMap, HashMap};

use cuenca_tree::{Changeset, GitOid};
use serde::{Deserialize, Serialize};

use crate::checks::{CheckRun, CheckStatus};
use crate::error::{CoreError, Result};
use crate::event::{ChangesetId, Event};
use crate::names::{ForkName, LocationName, PrincipalName};
use crate::owner::{Holder, OWNER_FORK_PREFIX, Writer, is_owner_fork, sibling_name};

/// A location as the log has it now. Fields added later are `#[serde(default)]`, so
/// rows written by this version still decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Location {
    pub name: LocationName,
    /// The git remote it lands to, if any (section 6 of the spec). Landing (BRO-3255)
    /// decides which transports it accepts.
    pub remote: Option<String>,
    /// The tree of its main.
    pub main: GitOid,
    /// The last move of main, if any.
    pub main_moved: Option<MainMove>,
}

/// One move of a location's main.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MainMove {
    pub changeset: ChangesetId,
    /// The fork whose head it was.
    pub fork: ForkName,
    /// The offset of the `MainAdvanced` record.
    pub offset: u64,
}

/// A fork as the log has it now. Until the format freezes (BRO-3257) fields change
/// freely; after that, fields added later are `#[serde(default)]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Fork {
    pub name: ForkName,
    pub location: LocationName,
    /// Who held it, epoch by epoch: `holders[e - 1]` held it under epoch `e`. The last
    /// is the holder now, and the fork's epoch is the length (never 0).
    pub holders: Vec<Holder>,
    /// The head each finished epoch ended at: `released[e - 1]` is the tree the fork
    /// held when epoch `e` was taken over (one entry per takeover, so one fewer than
    /// `holders`). A stale writer's offline chain resumes from there and nowhere else.
    pub released: Vec<GitOid>,
    /// For a sibling fork, the fork whose stale-epoch publishes it keeps.
    pub parent: Option<ForkName>,
    /// For a fork of a fork ([`crate::Event::ForkBranched`]), the fork it was forked
    /// from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<ForkName>,
    /// The tree it started from: main for a fork, the head of the fork it was forked
    /// from for a fork of a fork, the head its parent's stale epoch left for a sibling
    /// ([`Fork::released_at`]).
    pub base: GitOid,
    /// The tree of its latest draft, or `base` before the first.
    pub head: GitOid,
    /// The latest draft's changeset and the offset of its record.
    pub head_changeset: Option<(ChangesetId, u64)>,
    /// The latest verify of the fork: which changeset, against which main.
    pub verified: Option<Verified>,
    /// The holder's latest landing request and where it is.
    pub landing: Option<Landing>,
}

/// A fork's latest verify record ([`Event::VerifyRecorded`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Verified {
    pub changeset: ChangesetId,
    /// The location's main the checks were read from.
    pub base: GitOid,
    /// The offset of the record.
    pub offset: u64,
}

/// A landing request ([`Event::LandingIntent`]) and where it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Landing {
    pub changeset: ChangesetId,
    /// The offset of the request's record.
    pub offset: u64,
    /// The location's main the request's verify was judged against: what the head is
    /// landed onto.
    pub main: GitOid,
    /// The offset of the record that moved it to `step`.
    pub moved: u64,
    pub step: LandingStep,
}

impl Export {
    /// The export of `id`: `commit` onto `parent`, pushed to [`export_branch`].
    pub fn new(id: ChangesetId, commit: GitOid, parent: GitOid, base_ref: &str) -> Self {
        Self {
            commit,
            parent,
            branch: export_branch(id),
            base_ref: base_ref.to_string(),
        }
    }
}

/// The branch a landing of `id` is pushed to (spec section 6: `cp/<changeset-id>`).
pub fn export_branch(id: ChangesetId) -> String {
    format!("cp/{id}")
}

/// Longest reason a failed landing records, and longest pull request URL.
pub const MAX_LANDING_TEXT: usize = 4096;

/// Where a landing request is (spec section 6's state machine, up to "in review" for
/// a location with a git remote; GitHub's gates merge from there). Each move is one
/// record, and every call to the remote has an intent recorded before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
#[non_exhaustive]
pub enum LandingStep {
    /// Asked for. On a location with a remote the authority exports it next; on one
    /// without, it waits for the owner's `cuenca admin land`.
    Requested,
    /// The commit is built and about to be pushed ([`Event::ExportIntent`]).
    Exporting { export: Export },
    /// The remote's branch is at the commit.
    Exported { export: Export },
    /// A pull request is about to be opened.
    Reviewing { export: Export },
    /// A pull request is open.
    InReview {
        export: Export,
        pr: u64,
        url: String,
    },
    /// The owner landed it: main moved, by the record at `offset`.
    Landed { offset: u64 },
    /// It cannot go on.
    Failed { reason: String },
}

impl LandingStep {
    /// The step's name, as serialized.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Exporting { .. } => "exporting",
            Self::Exported { .. } => "exported",
            Self::Reviewing { .. } => "reviewing",
            Self::InReview { .. } => "in_review",
            Self::Landed { .. } => "landed",
            Self::Failed { .. } => "failed",
        }
    }

    /// The export, from the export intent on.
    pub fn export(&self) -> Option<&Export> {
        match self {
            Self::Exporting { export }
            | Self::Exported { export }
            | Self::Reviewing { export }
            | Self::InReview { export, .. } => Some(export),
            _ => None,
        }
    }
}

/// What a landing exports to the location's remote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Export {
    /// The commit: the changeset's target tree, with `parent` as its one parent.
    pub commit: GitOid,
    /// A commit of the remote's `base_ref` whose tree is the changeset's base.
    pub parent: GitOid,
    /// The branch it is pushed to, `cp/<changeset id>`.
    pub branch: String,
    /// The branch the pull request asks to merge into.
    pub base_ref: String,
}

impl Fork {
    /// The fencing epoch: 1 at creation, plus one per takeover.
    pub fn epoch(&self) -> u64 {
        self.holders.len() as u64
    }

    /// Who holds it now (`None` only for a damaged row; the log always gives a holder).
    pub fn holder(&self) -> Option<&Holder> {
        self.holders.last()
    }

    /// Who held it under `epoch`.
    pub fn holder_at(&self, epoch: u64) -> Option<&Holder> {
        let index = usize::try_from(epoch.checked_sub(1)?).ok()?;
        self.holders.get(index)
    }

    /// The head the fork held when `epoch` ended (`None` while it is current).
    pub fn released_at(&self, epoch: u64) -> Option<GitOid> {
        let ended = usize::try_from(epoch.checked_sub(1)?).ok()?;
        self.released.get(ended).copied()
    }
}

/// What [`apply`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Applied {
    /// The event changes the state; the caller appends it at the offset it gave.
    Fresh,
    /// The event repeats the latest record of its kind for the same fork, at this
    /// offset: a retry of a call that succeeded (a draft only while it is the head).
    /// The writer appends nothing; in a replay it is a duplicated record.
    Repeat(u64),
}

/// The state [`apply`] reads and writes.
pub(crate) trait View {
    fn location(&self, name: &LocationName) -> Result<Option<Location>>;
    fn fork(&self, name: &ForkName) -> Result<Option<Fork>>;
    /// The offset of the record that revoked `principal`, if one did.
    fn revoked(&self, principal: &PrincipalName) -> Result<Option<u64>>;
    fn put_revoked(&mut self, principal: &PrincipalName, offset: u64) -> Result<()>;
    fn put_location(&mut self, location: &Location) -> Result<()>;
    fn put_fork(&mut self, fork: &Fork) -> Result<()>;
    /// Index a draft's changeset under its id, unless an earlier draft already did.
    fn index_changeset(
        &mut self,
        id: ChangesetId,
        offset: u64,
        changeset: &Changeset,
    ) -> Result<()>;
}

fn location(view: &impl View, name: &LocationName) -> Result<Location> {
    view.location(name)?
        .ok_or_else(|| CoreError::UnknownLocation(name.to_string()))
}

fn fork(view: &impl View, name: &ForkName) -> Result<Fork> {
    view.fork(name)?
        .ok_or_else(|| CoreError::UnknownFork(name.to_string()))
}

fn not_revoked(view: &impl View, principal: &PrincipalName) -> Result<()> {
    match view.revoked(principal)? {
        Some(_) => Err(CoreError::Revoked(principal.to_string())),
        None => Ok(()),
    }
}

/// Whether `writer` may write `state` now. A writer whose principal is revoked is
/// refused first; then the holder under the fork's current epoch may write; the holder
/// of an earlier epoch is told its epoch is stale (its publish goes to a sibling fork);
/// anyone else, or any epoch the fork has not reached, is not the holder.
pub(crate) fn check_writer(view: &impl View, state: &Fork, writer: &Writer) -> Result<()> {
    not_revoked(view, &writer.holder.principal)?;
    let current = state.epoch();
    if state.holder_at(writer.epoch) != Some(&writer.holder) {
        return Err(CoreError::NotHolder {
            fork: state.name.to_string(),
            holder: writer.holder.to_string(),
            epoch: writer.epoch,
        });
    }
    if writer.epoch != current {
        return Err(CoreError::StaleEpoch {
            fork: state.name.to_string(),
            epoch: writer.epoch,
            current,
        });
    }
    Ok(())
}

/// The rules every new fork's name and holder meet, however it starts: the holder is
/// not revoked, the name is free and not kept for siblings, and a fork is an owner fork
/// exactly when the volume holds it.
fn new_fork(view: &impl View, name: &ForkName, holder: &Holder) -> Result<()> {
    not_revoked(view, &holder.principal)?;
    // `-from` is kept for siblings, so a sibling's name is never taken.
    if name.as_str().contains("-from") {
        return Err(CoreError::ReservedName(name.to_string()));
    }
    // A sibling of a fork named `owner` would be named `owner-from-…`: an owner
    // fork's name, held by its stale holder. So `owner` itself is kept.
    if name.as_str() == OWNER_FORK_PREFIX.trim_end_matches('-') {
        return Err(CoreError::ReservedName(name.to_string()));
    }
    if is_owner_fork(name) != holder.is_volume()
        || (holder.is_volume() && *holder != Holder::volume())
    {
        return Err(CoreError::OwnerFork(format!(
            "fork {name:?}: a fork named {OWNER_FORK_PREFIX}* is an owner fork, held by \
             the volume, and the volume holds nothing else (holder {holder})"
        )));
    }
    if view.fork(name)?.is_some() {
        return Err(CoreError::ForkExists(name.to_string()));
    }
    Ok(())
}

/// Apply `event`, to be recorded at `offset`, to `view`. On `Err` the view may be
/// partly written; the caller discards it (the writer aborts its transaction).
pub(crate) fn apply(view: &mut impl View, offset: u64, event: &Event) -> Result<Applied> {
    match event {
        Event::LocationAdded {
            location: name,
            remote,
            main,
        } => {
            if view.location(name)?.is_some() {
                return Err(CoreError::LocationExists(name.to_string()));
            }
            view.put_location(&Location {
                name: name.clone(),
                remote: remote.clone(),
                main: *main,
                main_moved: None,
            })?;
        }
        Event::ForkCreated {
            fork: name,
            location: at,
            base,
            holder,
        } => {
            new_fork(view, name, holder)?;
            let loc = location(view, at)?;
            if *base != loc.main {
                return Err(CoreError::NotAtBase {
                    what: format!("main of {at:?}"),
                    head: loc.main,
                    base: *base,
                });
            }
            view.put_fork(&Fork {
                name: name.clone(),
                location: at.clone(),
                holders: vec![holder.clone()],
                released: Vec::new(),
                parent: None,
                from: None,
                base: *base,
                head: *base,
                head_changeset: None,
                verified: None,
                landing: None,
            })?;
        }
        Event::ForkBranched {
            fork: name,
            from,
            base,
            holder,
        } => {
            new_fork(view, name, holder)?;
            let source = fork(view, from)?;
            if *base != source.head {
                return Err(CoreError::NotAtBase {
                    what: format!("head of {from:?}"),
                    head: source.head,
                    base: *base,
                });
            }
            view.put_fork(&Fork {
                name: name.clone(),
                location: source.location.clone(),
                holders: vec![holder.clone()],
                released: Vec::new(),
                parent: None,
                from: Some(from.clone()),
                base: *base,
                head: *base,
                head_changeset: None,
                verified: None,
                landing: None,
            })?;
        }
        Event::DraftPublished {
            fork: name,
            writer,
            id,
            changeset,
        } => {
            check_id(id, changeset)?;
            let state = fork(view, name)?;
            // Fencing comes before the retry: a fenced-out holder is never told Ok.
            check_writer(view, &state, writer)?;
            return draft(view, offset, state, id, changeset);
        }
        Event::DraftFenced {
            fork: name,
            sibling,
            writer,
            id,
            changeset,
        } => {
            check_id(id, changeset)?;
            let parent = fork(view, name)?;
            match check_writer(view, &parent, writer) {
                Err(CoreError::StaleEpoch { .. }) => {}
                Err(e) => return Err(e),
                Ok(()) => {
                    return Err(CoreError::Inconsistent(format!(
                        "draft {id} is fenced from {name:?}, but its writer holds it"
                    )));
                }
            }
            let state = fenced_target(view, &parent, writer, changeset.base)?;
            if state.name != *sibling {
                return Err(CoreError::Inconsistent(format!(
                    "draft {id} fenced from {name:?} belongs in {:?}, not {sibling:?}",
                    state.name
                )));
            }
            return draft(view, offset, state, id, changeset);
        }
        Event::TakenOver {
            fork: name,
            expected_epoch,
            holder,
        } => {
            not_revoked(view, &holder.principal)?;
            let mut state = fork(view, name)?;
            // Owner forks never move (spec section 2a): there is no fence for Finder to
            // cross. And the volume takes nothing over.
            if is_owner_fork(name) || holder.is_volume() {
                return Err(CoreError::OwnerFork(format!(
                    "fork {name:?} cannot move to {holder}: owner forks never move, and the \
                     volume holds only owner forks"
                )));
            }
            if state.epoch() != *expected_epoch {
                return Err(CoreError::EpochMismatch {
                    fork: name.to_string(),
                    expected: *expected_epoch,
                    actual: state.epoch(),
                });
            }
            state.released.push(state.head);
            state.holders.push(holder.clone());
            // A landing request is the old holder's; it does not pass to the new one.
            state.landing = None;
            view.put_fork(&state)?;
        }
        Event::PrincipalRevoked { principal } => {
            if let Some(at) = view.revoked(principal)? {
                return Ok(Applied::Repeat(at));
            }
            view.put_revoked(principal, offset)?;
        }
        Event::MainAdvanced {
            location: at,
            fork: from,
            changeset: id,
        } => {
            let mut state = fork(view, from)?;
            if state.location != *at {
                return Err(CoreError::WrongLocation {
                    fork: from.to_string(),
                    location: at.to_string(),
                });
            }
            let mut loc = location(view, at)?;
            if let Some(last) = &loc.main_moved
                && last.changeset == *id
                && last.fork == *from
            {
                return Ok(Applied::Repeat(last.offset));
            }
            // A location with a remote lands through GitHub, never around it.
            if loc.remote.is_some() {
                return Err(CoreError::HasRemote(at.to_string()));
            }
            let mut landing = landing_of(&state, id)?;
            if landing.step != LandingStep::Requested {
                return Err(step_error(&state, &landing, "requested"));
            }
            if state.head_changeset.map(|(head, _)| head) != Some(*id) {
                return Err(CoreError::NotHead {
                    fork: from.to_string(),
                    changeset: id.to_hex(),
                });
            }
            // The request is live (a takeover drops it); its holder must not be revoked.
            if let Some(holder) = state.holder() {
                not_revoked(view, &holder.principal)?;
            }
            // Drafts chain, so the head is the fork's whole work since its base. It lands
            // only onto a main that is still that base, or that this fork moved last (to
            // an earlier draft of the same chain): anything else would drop what landed
            // in between.
            let ours =
                loc.main == state.base || loc.main_moved.as_ref().is_some_and(|m| m.fork == *from);
            if !ours {
                return Err(CoreError::NotAtBase {
                    what: format!("main of {at:?}"),
                    head: loc.main,
                    base: state.base,
                });
            }
            loc.main = state.head;
            loc.main_moved = Some(MainMove {
                changeset: *id,
                fork: from.clone(),
                offset,
            });
            view.put_location(&loc)?;
            landing.moved = offset;
            landing.step = LandingStep::Landed { offset };
            state.landing = Some(landing);
            view.put_fork(&state)?;
        }
        Event::LandingIntent {
            fork: name,
            writer,
            changeset: id,
        } => {
            let mut state = fork(view, name)?;
            check_writer(view, &state, writer)?;
            if let Some(l) = &state.landing
                && l.changeset == *id
            {
                return Ok(Applied::Repeat(l.offset));
            }
            if state.head_changeset.map(|(head, _)| head) != Some(*id) {
                return Err(CoreError::NotHead {
                    fork: name.to_string(),
                    changeset: id.to_hex(),
                });
            }
            let loc = location(view, &state.location)?;
            match &state.verified {
                Some(v) if v.changeset == *id && v.base == loc.main => {}
                _ => {
                    return Err(CoreError::NotVerified {
                        fork: name.to_string(),
                        changeset: id.to_hex(),
                    });
                }
            }
            state.landing = Some(Landing {
                changeset: *id,
                offset,
                main: loc.main,
                moved: offset,
                step: LandingStep::Requested,
            });
            view.put_fork(&state)?;
        }
        Event::ExportIntent {
            fork: name,
            changeset: id,
            commit,
            parent,
            branch,
            base_ref,
        } => {
            let export = Export {
                commit: *commit,
                parent: *parent,
                branch: branch.clone(),
                base_ref: base_ref.clone(),
            };
            if *branch != export_branch(*id) || base_ref.is_empty() || base_ref.len() > 255 {
                return Err(CoreError::Inconsistent(format!(
                    "export of {id} names branch {branch:?} onto {base_ref:?}"
                )));
            }
            let (state, landing) = remote_landing(view, name, id)?;
            match &landing.step {
                LandingStep::Exporting { export: e } if *e == export => {
                    return Ok(Applied::Repeat(landing.moved));
                }
                LandingStep::Requested => {}
                _ => return Err(step_error(&state, &landing, "requested")),
            }
            holder_not_revoked(view, &state)?;
            set_step(
                view,
                state,
                landing,
                offset,
                LandingStep::Exporting { export },
            )?;
        }
        Event::Exported {
            fork: name,
            changeset: id,
        } => {
            let (state, landing) = remote_landing(view, name, id)?;
            let export = match &landing.step {
                LandingStep::Exported { .. } => return Ok(Applied::Repeat(landing.moved)),
                LandingStep::Exporting { export } => export.clone(),
                _ => return Err(step_error(&state, &landing, "exporting")),
            };
            set_step(
                view,
                state,
                landing,
                offset,
                LandingStep::Exported { export },
            )?;
        }
        Event::ReviewIntent {
            fork: name,
            changeset: id,
        } => {
            let (state, landing) = remote_landing(view, name, id)?;
            let export = match &landing.step {
                LandingStep::Reviewing { .. } => return Ok(Applied::Repeat(landing.moved)),
                LandingStep::Exported { export } => export.clone(),
                _ => return Err(step_error(&state, &landing, "exported")),
            };
            holder_not_revoked(view, &state)?;
            set_step(
                view,
                state,
                landing,
                offset,
                LandingStep::Reviewing { export },
            )?;
        }
        Event::InReview {
            fork: name,
            changeset: id,
            pr,
            url,
        } => {
            if *pr == 0 || url.is_empty() || url.len() > MAX_LANDING_TEXT {
                return Err(CoreError::Inconsistent(format!(
                    "pull request {pr} at {url:?} for {id}"
                )));
            }
            let (state, landing) = remote_landing(view, name, id)?;
            let export = match &landing.step {
                LandingStep::InReview { pr: p, url: u, .. } if p == pr && u == url => {
                    return Ok(Applied::Repeat(landing.moved));
                }
                LandingStep::Reviewing { export } => export.clone(),
                _ => return Err(step_error(&state, &landing, "reviewing")),
            };
            let step = LandingStep::InReview {
                export,
                pr: *pr,
                url: url.clone(),
            };
            set_step(view, state, landing, offset, step)?;
        }
        Event::LandingFailed {
            fork: name,
            changeset: id,
            reason,
        } => {
            if reason.len() > MAX_LANDING_TEXT {
                return Err(CoreError::Inconsistent(format!(
                    "the reason {id} failed is over {MAX_LANDING_TEXT} bytes"
                )));
            }
            let state = fork(view, name)?;
            let landing = landing_of(&state, id)?;
            match &landing.step {
                LandingStep::Failed { reason: r } if r == reason => {
                    return Ok(Applied::Repeat(landing.moved));
                }
                LandingStep::Requested
                | LandingStep::Exporting { .. }
                | LandingStep::Exported { .. }
                | LandingStep::Reviewing { .. } => {}
                _ => return Err(step_error(&state, &landing, "in flight")),
            }
            let step = LandingStep::Failed {
                reason: reason.clone(),
            };
            set_step(view, state, landing, offset, step)?;
        }
        Event::VerifyRecorded {
            fork: name,
            writer,
            changeset: id,
            candidate,
            base,
            manifest: _,
            checks,
        } => {
            let mut state = fork(view, name)?;
            check_writer(view, &state, writer)?;
            if state.head_changeset.map(|(head, _)| head) != Some(*id) {
                return Err(CoreError::NotHead {
                    fork: name.to_string(),
                    changeset: id.to_hex(),
                });
            }
            if state.head != *candidate {
                return Err(CoreError::Inconsistent(format!(
                    "verify of {id} names candidate {candidate}, but the fork's head is {}",
                    state.head
                )));
            }
            let loc = location(view, &state.location)?;
            if loc.main != *base {
                return Err(CoreError::BaseMoved {
                    location: loc.name.to_string(),
                    main: loc.main,
                    base: *base,
                });
            }
            checks.iter().try_for_each(check_run)?;
            state.verified = Some(Verified {
                changeset: *id,
                base: *base,
                offset,
            });
            view.put_fork(&state)?;
        }
    }
    Ok(Applied::Fresh)
}

/// The fork's landing request for `id`.
fn landing_of(state: &Fork, id: &ChangesetId) -> Result<Landing> {
    match &state.landing {
        Some(l) if l.changeset == *id => Ok(l.clone()),
        _ => Err(CoreError::NoLanding {
            fork: state.name.to_string(),
            changeset: id.to_hex(),
        }),
    }
}

/// The fork and its landing request for `id`, on a location with a git remote: the
/// authority's export steps.
fn remote_landing(view: &impl View, name: &ForkName, id: &ChangesetId) -> Result<(Fork, Landing)> {
    let state = fork(view, name)?;
    if location(view, &state.location)?.remote.is_none() {
        return Err(CoreError::NoRemote(state.location.to_string()));
    }
    let landing = landing_of(&state, id)?;
    Ok((state, landing))
}

/// The fork's holder, who asked for the landing, is not revoked: checked before each
/// call to the remote.
fn holder_not_revoked(view: &impl View, state: &Fork) -> Result<()> {
    match state.holder() {
        Some(holder) => not_revoked(view, &holder.principal),
        None => Err(CoreError::Inconsistent(format!(
            "fork {:?} has no holder",
            state.name
        ))),
    }
}

fn step_error(state: &Fork, landing: &Landing, want: &str) -> CoreError {
    CoreError::LandingStep {
        fork: state.name.to_string(),
        changeset: landing.changeset.to_hex(),
        step: landing.step.name().to_string(),
        want: want.to_string(),
    }
}

fn set_step(
    view: &mut impl View,
    mut state: Fork,
    mut landing: Landing,
    offset: u64,
    step: LandingStep,
) -> Result<()> {
    landing.moved = offset;
    landing.step = step;
    state.landing = Some(landing);
    view.put_fork(&state)
}

/// The fork a draft fenced from `parent` (written by `writer` under one of `parent`'s
/// stale epochs) goes to: the sibling for that parent and epoch
/// ([`sibling_name`]), new if it does not exist yet, starting from the head the stale
/// epoch ended at ([`Fork::released_at`]) and held by the writer under epoch 1. A draft
/// whose base is any other tree is refused as `NotAtBase`: the writer could read only
/// what its epoch left, so a sibling never starts from a tree it was not given (another
/// location's main, another fork's head). If that sibling has since been taken over, the
/// writer is stale there too, and the draft goes one level down, to the sibling's own
/// sibling for epoch 1, and so on. So a stale draft always has a fork of the writer's
/// own to go to, never one someone else holds. Once there it is a draft like any
/// other: it must start from that fork's head (the writer's chain continues), or it
/// is refused as `NotAtBase` and stays in the writer's checkout, unacknowledged.
pub(crate) fn fenced_target(
    view: &impl View,
    parent: &Fork,
    writer: &Writer,
    base: GitOid,
) -> Result<Fork> {
    let (mut at, mut epoch) = (parent.clone(), writer.epoch);
    loop {
        let name = sibling_name(&at.name, epoch, &writer.holder.node);
        let Some(sibling) = view.fork(&name)? else {
            let released = at.released_at(epoch).ok_or_else(|| {
                CoreError::Inconsistent(format!("{:?} has no end for epoch {epoch}", at.name))
            })?;
            if base != released {
                return Err(CoreError::NotAtBase {
                    what: format!("fork {:?} as epoch {epoch} left it", at.name),
                    head: released,
                    base,
                });
            }
            return Ok(Fork {
                name,
                location: at.location.clone(),
                holders: vec![writer.holder.clone()],
                released: Vec::new(),
                parent: Some(at.name.clone()),
                from: None,
                base,
                head: base,
                head_changeset: None,
                verified: None,
                landing: None,
            });
        };
        // Only this rule creates a fork under a sibling's name (fork creation refuses
        // them), from this parent's holder under this epoch: the writer. A different
        // first holder means two (parent, epoch) pairs share a name (a hash collision):
        // refuse rather than mix two holders' drafts.
        if sibling.holder_at(1) != Some(&writer.holder) {
            return Err(CoreError::Inconsistent(format!(
                "sibling {name:?} was first held by another writer"
            )));
        }
        if sibling.epoch() == 1 {
            return Ok(sibling);
        }
        (at, epoch) = (sibling, 1);
    }
}

/// A run's status and exit code agree: `passed` exited 0, `failed` exited otherwise or
/// by a signal, and a check that timed out or never started has no exit code.
fn check_run(run: &CheckRun) -> Result<()> {
    let ok = match run.status {
        CheckStatus::Passed => run.exit_code == Some(0),
        CheckStatus::Failed => run.exit_code != Some(0),
        CheckStatus::TimedOut | CheckStatus::NotRun => run.exit_code.is_none(),
    };
    if ok {
        Ok(())
    } else {
        Err(CoreError::BadCheckRun {
            name: run.check.name.clone(),
            reason: format!(
                "status {:?} does not go with exit code {:?}",
                run.status, run.exit_code
            ),
        })
    }
}

fn check_id(id: &ChangesetId, changeset: &Changeset) -> Result<()> {
    if ChangesetId::of(changeset) != *id {
        return Err(CoreError::Inconsistent(format!(
            "draft {id} is not its changeset's id"
        )));
    }
    Ok(())
}

/// Move `state`'s head by `changeset`, which must start from it. Publishing the
/// changeset that already is the head is a retry.
fn draft(
    view: &mut impl View,
    offset: u64,
    mut state: Fork,
    id: &ChangesetId,
    changeset: &Changeset,
) -> Result<Applied> {
    if let Some((head, at)) = state.head_changeset
        && head == *id
    {
        return Ok(Applied::Repeat(at));
    }
    if changeset.base != state.head {
        return Err(CoreError::NotAtBase {
            what: format!("fork {:?}", state.name),
            head: state.head,
            base: changeset.base,
        });
    }
    state.head = changeset.target;
    state.head_changeset = Some((*id, offset));
    view.put_fork(&state)?;
    view.index_changeset(*id, offset, changeset)?;
    Ok(Applied::Fresh)
}

/// The projections held in memory, as a replay builds them.
#[derive(Debug, Default)]
pub(crate) struct Memory {
    pub(crate) locations: BTreeMap<LocationName, Location>,
    pub(crate) forks: BTreeMap<ForkName, Fork>,
    pub(crate) changesets: HashMap<ChangesetId, (u64, Changeset)>,
    pub(crate) revoked: BTreeMap<PrincipalName, u64>,
}

impl View for Memory {
    fn location(&self, name: &LocationName) -> Result<Option<Location>> {
        Ok(self.locations.get(name).cloned())
    }

    fn fork(&self, name: &ForkName) -> Result<Option<Fork>> {
        Ok(self.forks.get(name).cloned())
    }

    fn revoked(&self, principal: &PrincipalName) -> Result<Option<u64>> {
        Ok(self.revoked.get(principal).copied())
    }

    fn put_revoked(&mut self, principal: &PrincipalName, offset: u64) -> Result<()> {
        self.revoked.insert(principal.clone(), offset);
        Ok(())
    }

    fn put_location(&mut self, location: &Location) -> Result<()> {
        self.locations
            .insert(location.name.clone(), location.clone());
        Ok(())
    }

    fn put_fork(&mut self, fork: &Fork) -> Result<()> {
        self.forks.insert(fork.name.clone(), fork.clone());
        Ok(())
    }

    fn index_changeset(
        &mut self,
        id: ChangesetId,
        offset: u64,
        changeset: &Changeset,
    ) -> Result<()> {
        self.changesets
            .entry(id)
            .or_insert_with(|| (offset, changeset.clone()));
        Ok(())
    }
}
