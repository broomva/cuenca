//! The `cuenca` CLI's agent side (spec `docs/specs/2026-10-19-agent-native-change-plane.html`
//! in broomva/workspace, sections 0, 2 and 2a; ticket BRO-3252).
//!
//! [`verbs`] holds the agent verbs of spec rev 2.2 section 0, one function each:
//! `forks`, `fork`, `checkout`, `status`, `snapshot`, `diff`, `log`, `verify` and `land`.
//! The binary and the MCP server (BRO-3253) both call them, and nothing else: the owner
//! verbs (`cuenca admin`) live in the binary only, speak to the authority's local socket,
//! and are not reachable from this library, so no MCP tool can reach them either.
//!
//! An agent verb talks to the authority over its HTTP API ([`client`]) with the key
//! `cuenca login` stored ([`config`]). A checkout is any directory; what the CLI knows
//! about it lives outside it, in the user's state directory ([`local`]), so nothing is
//! added to the tree a snapshot reads.
//!
//! # The outbox
//!
//! A snapshot is a full-tree scan of the checkout against its last snapshot (cuenca-fs).
//! Its changeset and a copy of every blob it adds are written to the checkout's outbox
//! before anything is sent; then the outbox is published in order, each draft idempotent
//! by its changeset id. A snapshot taken while the authority is unreachable stays queued
//! and goes out with the next verb that publishes, so an offline hour, a crash or an
//! outage loses nothing and a retry duplicates nothing. A draft the authority refuses
//! (the caller does not hold the fork, the fork moved) stays queued too, and the verb
//! says why; nothing in the checkout is touched.

pub mod client;
pub mod config;
pub mod local;
pub mod mcp;
pub mod verbs;

pub use client::{Client, ClientError};
pub use config::Config;
pub use verbs::Report;
