# Local backup fixture wire format

Status: accepted for local implementation (BRO-3258); no production custody grant.
Date: 2026-10-25

## Context

The workspace's accepted backup-format ADR fixes stable journal prefixes, age,
authority signatures, manifest-last publication and independently pinned trust.
The backup-custody ADR remains proposed. R2 acceptance and the NUC timer are blocked.

## Options

1. Copy redb while the authority runs: inconsistent; rejected.
2. Export journal records and historical trees in one redb read transaction,
   then read immutable reachable blobs: selected.
3. Require a local S3 daemon for the drill: unnecessary infrastructure; deferred.

## Decision

`Store::export_prefix` exports records and every historical tree referenced by
those records. `Store::import_prefix` preserves original offsets/times and rebuilds
projections through existing transition and admission rules in one transaction.

Use age x25519 recipients and Ed25519 signatures. Sign the domain
`cuenca-backup-manifest-v1` followed by a NUL and serde JSON of the typed manifest
(field order as declared). Format 1 contains a record count, optional zero-based
last offset (`null` for empty), plaintext journal SHA-256/size, encrypted journal
reference, sorted distinct reachable blobs (git oid, independent SHA-256, size,
encrypted object reference), and SHA-256 of the signing public key.

All data objects, including the signed manifest, are age encrypted. Objects live
at `objects/<ciphertext-sha256>.age`. Each is written privately, fsynced, atomically
published, directory-synced and re-read/verified. Blobs precede the journal, which
precedes the signed encrypted manifest. A private temporary completion envelope
is fsynced and published last at
`manifests/<20-digit-record-count>-<manifest-ciphertext-sha256>.json`; its only fields
are format version and the encrypted manifest's SHA-256/size. The record count in
the filename reveals prefix length, but no source paths or plaintext digests.
Dot-prefixed temporary envelopes are never restore points. A directory flock
serializes fixture writers. Immutable object ids prevent partial overwrites.

Restore chooses the highest committed prefix, verifies ciphertext/decryption,
independently pinned Ed25519 trust, version, offsets, journal integrity, exact
blob reachability, each blob's git oid/SHA-256/size, and replay rules. Invalid
newest committed data fails visibly; it never falls back to an older good point.
Restore builds in a private sibling directory and publishes with no replacement.
It never merges into an existing authority state.

## Consequences and limits

The library can export a running store through its existing `Store` handle. The
`cuenca-backup` command and `deploy/backup.sh` are **offline operators**: redb's
exclusive process lock requires stopping the authority first. They accept only a
local directory backend. No R2 resources, credentials, timers or grants are added.

This bounded fixture retains all committed manifests and objects (no garbage
collection); it does not establish production 90-day retention, the monthly remote
1% audit, RPO or fresh-host RTO. Current in-memory export limits are 64 MiB journal,
8 MiB signed manifest and 256 MiB per ciphertext; larger inputs fail visibly.
Uncommitted objects remain harmless and can consume disk; production pruning,
streaming and scheduling belong with remote acceptance.

Bearer-key files, signing seeds, Git caches, owner SMB credentials and age private
keys are not journal content and are not copied into the backup. A fresh authority
requires independent host provisioning and newly minted agent credentials; journal
revocations survive. Signing trust and owner decryption custody remain external.

The drill generates throwaway keys on the SSD; no owner keys or real credentials.
It sends SIGKILL before/after every blob/journal/manifest upload and completion
publication, verifies the prior/new prefix as appropriate, opens a restored owner
fork through the real materialiser and resumes a pending intent without duplication.
