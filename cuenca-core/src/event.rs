use cuenca_tree::{Changeset, ContentHash, GitOid};
use serde::{Deserialize, Serialize};

use crate::checks::{CheckRun, ManifestId};
use crate::names::{ForkName, LocationName, PrincipalName};
use crate::owner::{Holder, Writer};

/// A changeset's id: the SHA-256 of its canonical JSON. Changesets are canonical (ops
/// sorted, one per path, each a real change), so two equal changesets share an id, and
/// a publish retried while its draft is still the fork's head is recognised as the
/// same publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangesetId(ContentHash);

impl ChangesetId {
    pub fn of(changeset: &Changeset) -> Self {
        let json = serde_json::to_vec(changeset).expect("a changeset always serializes");
        Self(ContentHash::of(&json))
    }

    pub fn from_hex(s: &str) -> Result<Self, cuenca_tree::TreeError> {
        ContentHash::from_hex(s).map(Self)
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex()
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }
}

impl std::fmt::Display for ChangesetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// One fact in the log. The locations, forks, changeset index and revocations are
/// projections of these events, rebuilt and compared by [`crate::Store::replay_check`].
///
/// The event set is not frozen yet: no log has been deployed, so a change needs no
/// migration; the format freezes when the authority first runs on the VPS (BRO-3257).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Event {
    /// A directory tree became a location, with `main` as its first tree.
    LocationAdded {
        location: LocationName,
        remote: Option<String>,
        main: GitOid,
    },
    /// A fork of a location, starting from the location's main at that moment, held by
    /// `holder` under epoch 1.
    ForkCreated {
        fork: ForkName,
        location: LocationName,
        base: GitOid,
        holder: Holder,
    },
    /// A fork of the fork `from`, starting from `from`'s head at that moment (`base`),
    /// in `from`'s location, held by `holder` under epoch 1. Forking an owner fork
    /// snapshots it first (spec section 2a), so `base` holds every write the
    /// snapshot read; what is written later stays in `from`.
    ForkBranched {
        fork: ForkName,
        from: ForkName,
        base: GitOid,
        holder: Holder,
    },
    /// A draft: a changeset moving the fork's head from `changeset.base` to
    /// `changeset.target`, stored whole, written by the fork's holder under its
    /// current epoch. (A location's first tree is named by id only; its objects are in
    /// the store's tree table.)
    DraftPublished {
        fork: ForkName,
        writer: Writer,
        id: ChangesetId,
        changeset: Changeset,
    },
    /// A draft written under a stale epoch of `fork` by the holder of that epoch, kept
    /// as a draft of the sibling fork `sibling` ([`crate::sibling_name`]), which this
    /// event creates if it does not exist yet. Never dropped, never merged into `fork`.
    DraftFenced {
        fork: ForkName,
        sibling: ForkName,
        writer: Writer,
        id: ChangesetId,
        changeset: Changeset,
    },
    /// A fork's ownership moved to `holder`, by compare-and-swap on the epoch: the fork
    /// was at `expected_epoch`, and is now at `expected_epoch + 1`.
    TakenOver {
        fork: ForkName,
        expected_epoch: u64,
        holder: Holder,
    },
    /// A principal's keys were revoked: from here on the log refuses every write it
    /// asks for. Permanent; a new key means a new principal.
    PrincipalRevoked { principal: PrincipalName },
    /// A location's main moved by the changeset at the head of one of its forks: the
    /// owner landed the fork's landing request on a location without a git remote
    /// (spec section 6). An owner act, so it names no writer.
    MainAdvanced {
        location: LocationName,
        fork: ForkName,
        changeset: ChangesetId,
    },
    /// The fork's holder asked to land the fork's head changeset, which a verify of
    /// the location's main now has judged ([`crate::Verified`]). What happens to it is
    /// recorded by the events below, each about this request ([`crate::Landing`]).
    LandingIntent {
        fork: ForkName,
        writer: Writer,
        changeset: ChangesetId,
    },
    /// The authority built `commit` (the changeset's target tree, with `parent`, a
    /// commit of the remote's `base_ref` whose tree is the main the request's verify
    /// judged, [`crate::Landing::main`]), or found it already on the remote's `branch`,
    /// and is about to push it there. Written before the push.
    ExportIntent {
        fork: ForkName,
        changeset: ChangesetId,
        commit: GitOid,
        parent: GitOid,
        branch: String,
        base_ref: String,
    },
    /// The remote's `branch` is at the export's commit. Written after the push, or
    /// after finding the branch already there.
    Exported {
        fork: ForkName,
        changeset: ChangesetId,
    },
    /// The authority is about to open a pull request from the export's branch.
    ReviewIntent {
        fork: ForkName,
        changeset: ChangesetId,
    },
    /// A pull request from the export's branch at its commit: opened now, or found (in
    /// any state, so a retry never opens a second one).
    InReview {
        fork: ForkName,
        changeset: ChangesetId,
        pr: u64,
        url: String,
    },
    /// The landing request cannot go on, for `reason`; asking again needs a new draft.
    LandingFailed {
        fork: ForkName,
        changeset: ChangesetId,
        reason: String,
    },
    /// The fork's holder ran the checks the base declares ([`crate::checks`]) on the
    /// fork's head changeset, whose target is `candidate`, and reported how they ended.
    /// `base` is the location's main the checks were read from, `manifest` that tree's
    /// `.cuenca/checks.toml` as the authority found it (`None`: the base declares no
    /// checks), and `checks` the base's list, exactly, in order, with each result.
    /// Advisory in phase 2: it refuses nothing, and the results are the caller's own.
    VerifyRecorded {
        fork: ForkName,
        writer: Writer,
        changeset: ChangesetId,
        candidate: GitOid,
        base: GitOid,
        manifest: Option<ManifestId>,
        checks: Vec<CheckRun>,
    },
}

impl Event {
    /// The forks this event is about: the history of a fork is the records that name it.
    pub fn forks(&self) -> Vec<&ForkName> {
        match self {
            Event::LocationAdded { .. } | Event::PrincipalRevoked { .. } => vec![],
            Event::ForkCreated { fork, .. }
            | Event::ForkBranched { fork, .. }
            | Event::DraftPublished { fork, .. }
            | Event::TakenOver { fork, .. }
            | Event::MainAdvanced { fork, .. }
            | Event::LandingIntent { fork, .. }
            | Event::ExportIntent { fork, .. }
            | Event::Exported { fork, .. }
            | Event::ReviewIntent { fork, .. }
            | Event::InReview { fork, .. }
            | Event::LandingFailed { fork, .. }
            | Event::VerifyRecorded { fork, .. } => vec![fork],
            Event::DraftFenced { fork, sibling, .. } => vec![fork, sibling],
        }
    }
}

/// An event at its position in the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Position in the log, from 0 with no gaps.
    pub offset: u64,
    /// Wall-clock time of the append, in milliseconds since the Unix epoch. Informative
    /// only: order is the offset.
    pub at_ms: u64,
    pub event: Event,
}
