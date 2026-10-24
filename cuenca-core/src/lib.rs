//! The Cuenca authority's state (spec `docs/specs/2026-10-19-agent-native-change-plane.html`
//! in broomva/workspace, sections 2a, 3 and 6b).
//!
//! A [`Store`] is one directory holding:
//!
//! - **the log**, an append-only sequence of [`Record`]s in a redb database. Each call
//!   that changes it is one write transaction that runs the event through the
//!   transition rules (one copy of them, in `state.rs`), appends exactly one record and
//!   updates the projections, so a crash leaves the record and its effects, or neither;
//!   commits are durable before they return;
//! - **projections** of the log: [`Location`]s (any directory tree the authority tracks,
//!   with an optional git remote), [`Fork`]s with their head, and a changeset index.
//!   [`Store::replay_check`] replays the log through the same transition rules and
//!   compares (admission checks that need blob content are the writer's alone). The
//!   git tree objects every recorded tree is made of sit beside them, checked against
//!   their id on every read;
//! - **blobs**, keyed by git object id. A blob enters the store only after its bytes
//!   hash to the claimed id ([`cuenca_tree::BlobHasher`], with SHA-1 collision
//!   detection), and its SHA-256 is derived from those verified bytes.
//!
//! # One writer per fork
//!
//! Every fork has a holder ([`Holder`]: a principal writing from one machine) and a
//! fencing epoch, 1 when the fork is created. Only the holder, under the current epoch,
//! may publish a draft or ask to land. [`Store::take_over`] moves the fork to a new
//! holder by compare-and-swap on the epoch, which it increments, and drops the old
//! holder's landing request. A revocation leaves requests in place, so each landing
//! step that acts on one (the owner moving main, an export or review intent) checks
//! that its holder is not revoked, in the transaction that records it. A
//! takeover retried after it succeeded gets `EpochMismatch`; read the fork to see who
//! holds it. A publish by the holder of an earlier epoch is not
//! dropped and not merged in: it becomes a draft of that epoch's sibling fork,
//! `<fork>-from-<node>-e<epoch>` ([`sibling_name`]), one level further down if that
//! sibling was taken over too. A sibling starts from the head its epoch ended at and
//! from no other tree, so a stale writer never gains a tree it was not given (another
//! location's main, another fork's head); a stale draft that does not continue that
//! head is refused as `NotAtBase` and stays in the writer's checkout. [`Store::revoke`] makes the
//! log refuse every later write a principal asks for. Who a request comes from (keys,
//! `tailscale whois`) is the authority's to establish (BRO-3251); this layer trusts
//! the [`Writer`] it is given.
//!
//! # Landing
//!
//! The holder asks to land its fork's head once a verify against the location's main
//! is recorded ([`Store::record_landing_intent`]). From there a [`Landing`] moves through
//! [`LandingStep`]s, one record each (spec section 6). On a location with a git remote,
//! the authority records an export intent before it pushes `cp/<changeset id>`, the
//! export after, a review intent before it opens a pull request and the pull request
//! after; GitHub's required checks merge it from there. On a location without one,
//! the request waits until the owner moves main ([`Store::advance_main`]). Each step
//! follows only its predecessor and repeats only itself, so an authority that restarts
//! picks up from the step the log has.
//!
//! # Gates
//!
//! A landing intent carries the gate engine's verdict on it ([`gates`], spec section
//! 4): compiled from the policy of the main it lands onto, never the candidate's,
//! journaled in the same record, and log-only, so it refuses nothing until phase 3
//! (BRO-3307). [`Store::evaluate_landing`] is the dry run.
//!
//! # Why not `lago-journal`
//!
//! `lago-journal` stores Lago's `EventEnvelope`s keyed by session, branch and sequence,
//! behind an async `Journal` trait. The authority needs a single global order, one
//! transaction covering an append and its projections, and synchronous calls. It uses the
//! same engine, redb, directly.

mod blobs;
pub mod checks;
mod error;
mod event;
pub mod gates;
mod names;
mod owner;
mod state;
mod store;

pub use blobs::MAX_BLOB_BYTES;
pub use error::{CoreError, Result};
pub use event::{ChangesetId, Event, Record};
pub use names::{ForkName, LocationName, MAX_NAME_BYTES, NameError, PrincipalName};
pub use owner::{
    Holder, MAX_NODE_BYTES, NodeId, OWNER_FORK_PREFIX, VOLUME_PRINCIPAL, Writer, is_owner_fork,
    sibling_name,
};
pub use state::{
    Export, Fork, Landing, LandingStep, Location, MAX_LANDING_TEXT, MainMove, Verified,
    export_branch,
};
pub use store::{BlobBatch, BlobInfo, Landed, Published, ReplaySummary, Snapshot, Store};
