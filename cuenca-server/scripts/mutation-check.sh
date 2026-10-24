#!/usr/bin/env bash
# Mutation check for cuenca-server: each mutant breaks one guarantee, and the tests must
# fail for every one. Run from the crate directory: scripts/mutation-check.sh
# Prints one line per mutant and exits non-zero if any mutant survives. The source is
# restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
. ../scripts/mutation-guard.sh
backup=$guard_backup
lock=$(git rev-parse --show-toplevel)/Cargo.lock
cp "$lock" "$backup/Cargo.lock"
restore() { [ -n "${current:-}" ] && cp "$backup/file" "$current"; current=; rm -f "$backup/path"; cp "$backup/Cargo.lock" "$lock"; }
trap 'restore; guard_cleanup' EXIT
trap 'exit 130' INT TERM

# A mutant only counts as killed against a suite that passes unmutated.
if ! bounded cargo test -q -p cuenca-server >"$backup/log" 2>&1; then
  echo "baseline is red: fix the suite before reading any mutant verdict"; cat "$backup/log"; exit 2
fi
echo "baseline  green"

survived=0
mutate() { # file, old, new, then the cargo test arguments that should catch it
  local file=$1 old=$2 new=$3; shift 3
  current=$file; cp "$file" "$backup/file"; echo "$PWD/$file" >"$backup/path"
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); s = p.read_text()
if s.count(sys.argv[2]) != 1: sys.exit("anchor not unique: " + sys.argv[2])
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
PY
  then echo "ANCHOR MISSING  $file: $old"; survived=1; restore; return; fi
  bounded cargo test -q -p cuenca-server "$@" >"$backup/log" 2>&1; local status=$?
  if [ "$status" -ge 128 ]; then
    # The guard or the test run was itself killed by a signal: no verdict is possible.
    echo "ABORTED   $file: $old (status $status)"; exit 3
  elif [ "$status" -eq 0 ]; then
    echo "SURVIVED  $file: $old"; survived=1
  elif grep -qE '^error\[E[0-9]+\]|could not compile|failed to select a version' "$backup/log"; then
    echo "NO BUILD  $file: $old (a mutant that does not build proves nothing)"; survived=1
  elif [ "$status" -eq 124 ] || [ "$status" -eq 125 ]; then
    # Stopped by the guard: a kill only if the tests had started, not mid-build.
    if grep -qE '^running [0-9]+ tests?$' "$backup/log"; then
      echo "killed    $file: $old ($(tail -n 1 "$backup/log"))"
    else
      echo "NO TEST   $file: $old (stopped before any test ran: $(tail -n 1 "$backup/log"))"; survived=1
    fi
  else
    echo "killed    $file: $old"
  fi
  restore
}

# Keys: only the right secret, with the right prefix, verifies; one key per principal;
# removal ends a key; the file is private.
mutate src/keys.rs 'constant_time_eq(presented.as_bytes(), record.secret_sha256.as_bytes())' 'true' --lib
mutate src/keys.rs 'if prefix != TOKEN_PREFIX {' 'if false {' --lib
mutate src/keys.rs 'if keys.values().any(|k| k.principal == principal) {' 'if false {' --lib
mutate src/keys.rs 'next.retain(|_, k| &k.principal != principal);' '' --lib
mutate src/keys.rs 'if mode & 0o077 != 0 {' 'if false {' --lib
mutate src/keys.rs 'file.set_permissions(std::fs::Permissions::from_mode(0o600))?;' 'file.set_permissions(std::fs::Permissions::from_mode(0o644))?;' --lib
# Authentication on every request: revocation in the log, the key's node, routes, an
# unnamed source.
mutate src/authority.rs '            .is_some()
        {
            return Err(AuthError::Revoked' '            .is_some()
            && false
        {
            return Err(AuthError::Revoked' --test api
mutate src/authority.rs 'if peer.node != key.node {' 'if false {' --test api
mutate src/authority.rs 'if peer.serves_routes {' 'if false {' --test api
mutate src/authority.rs '.ok_or(AuthError::UnnamedSource(ip))?;' '.unwrap_or(crate::identity::Peer { node: key.node.clone(), serves_routes: false });' --test api
# Read scoping.
mutate src/authority.rs 'self.owner_device() || self.key.grant.locations.contains(&location.name)' 'true' --test api
mutate src/authority.rs '            || self.key.grant.forks.contains(&fork.name)
' '' --test api
mutate src/authority.rs '|| fork.holder() == Some(&self.holder)' '|| fork.holder().is_some()' --test api
mutate src/authority.rs '|| fork.holder() == Some(&self.holder)' '|| fork.holder().is_some_and(|h| h.principal == self.key.principal)' --test api
mutate src/authority.rs '|| fork.holder() == Some(&self.holder)' '|| false' --test api
# A former holder reads the fork no more, but its stale drafts still reach the rules.
mutate src/authority.rs '|| fork.holder() == Some(&self.holder)' '|| fork.holders.contains(&self.holder)' --test api
mutate src/authority.rs 'self.sees_fork(fork) || fork.holders.contains(&self.holder)' 'self.sees_fork(fork)' --test api
mutate src/authority.rs 'self.sees_fork(fork) || fork.holders.contains(&self.holder)' 'self.sees_fork(fork) || fork.holders.iter().any(|h| h.principal == self.key.principal)' --test api
mutate src/http.rs 'Some(f) if caller.reaches_fork(&f) => Ok(f),' 'Some(f) => Ok(f),' --test api
mutate src/authority.rs 'self.key.kind == PrincipalKind::OwnerDevice' 'true' --test api
mutate src/http.rs 'Some(f) if caller.sees_fork(&f) => Ok(f),' 'Some(f) => Ok(f),' --test api
mutate src/http.rs 'Some(l) if caller.sees_location(&l) => Ok(l),' 'Some(l) => Ok(l),' --test api
mutate src/http.rs '        visible_location(a, &c, &location)?;
' '' --test api
mutate src/http.rs 'if !snapshot.fork(&name)?.is_some_and(|f| c.sees_fork(&f)) {' 'if false {' --test api
mutate src/http.rs '        writable_fork(a, &c, &name)?;
        let writer = Writer::new(c.holder.clone(), req.epoch);
        let offset' '        let writer = Writer::new(c.holder.clone(), req.epoch);
        let offset' --test api
mutate src/http.rs '        writable_fork(a, &c, &name)?;
        let writer = Writer::new(c.holder.clone(), req.epoch);
        let (offset, manifest)' '        let writer = Writer::new(c.holder.clone(), req.epoch);
        let (offset, manifest)' --test api a_verify_is_recorded
# A draft names only blobs the caller reads or has sent: no laundering, no oracle.
mutate src/authority.rs 'if !known {' 'if false {' --test api
mutate src/authority.rs 'let head = if caller.sees_fork(fork) {' 'let head = if true {' --test api
mutate src/authority.rs 'let known = head.as_ref().is_some_and(|set| set.contains(&oid))' 'let known = false' --test api
mutate src/authority.rs '|| own.is_some_and(|set| set.contains(&oid));' ';' --test api
mutate src/http.rs 'let info = a.store.ingest_blob(oid, size, content)?;
            a.possesses(&c.key.principal, oid);' 'let info = a.store.ingest_blob(oid, size, content)?;' --test api
# History hides drafts fenced into a sibling the caller does not read.
mutate src/http.rs '!snapshot.fork(sibling)?.is_some_and(|f| c.sees_fork(&f))' 'false' --test api
# Staging is cleared on open.
mutate src/authority.rs '            std::fs::remove_dir_all(&staging)?;' '' --test api
mutate src/http.rs 'if !a.blob_set(tree)?.contains(&oid) {' 'if false {' --test api
# Binding: tailnet ranges only, unless unsafe.
mutate src/lib.rs 'if unsafe_bind || is_tailnet(addr.ip()) {' 'if true {' --lib
mutate src/lib.rs 'o[0] == 100 && (o[1] & 0xc0) == 64' 'o[0] == 100' --lib
mutate src/lib.rs 's[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0' 's[0] == 0xfd7a' --lib
# Owner acts: revocation goes to the log, a revoked name is not minted again, the
# socket is private.
mutate src/admin.rs '|p| a.store.revoke(p)' '|_| Ok(0)' --test api
mutate src/admin.rs 'if a.is_banned(&principal) || a.store.revoked(&principal).map_err(|e| s(&e))?.is_some()' 'if a.is_banned(&principal)' --test api
mutate src/admin.rs 'std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;' '' --test api
mutate src/admin.rs 'if mode & 0o022 != 0 {' 'if false {' --test api
# Identity: routes from either source mark a peer; a node outside the netmap is unnamed.
mutate src/identity.rs 'serves_routes: routes(&whois.node.primary_routes) || routes(&entry.primary_routes),' 'serves_routes: routes(&entry.primary_routes),' --lib
mutate src/identity.rs 'serves_routes: routes(&whois.node.primary_routes) || routes(&entry.primary_routes),' 'serves_routes: routes(&whois.node.primary_routes),' --lib
mutate src/identity.rs '    let Some(entry) = in_netmap else {
        return Ok(None);
    };' '    let Some(entry) = in_netmap else {
        return Ok(Some(Peer { node, serves_routes: false }));
    };' --lib

# Revocation refuses in memory first, before any write.
mutate src/authority.rs 'if self.is_banned(&key.principal) {' 'if false {' --test api
mutate src/admin.rs '    a.ban(principal);
    let offset' '    let offset' --lib
mutate src/authority.rs '        drop(self.revocation.write().expect("revocation lock"));' '' --lib
# Every agent write checks revocation again right before it commits, serialized with
# the ban.
mutate src/authority.rs 'if self.is_banned(principal) || self.store.revoked(principal)?.is_some() {' 'if false {' --test api
mutate src/authority.rs '        let _writes = self.revocation.read().expect("revocation lock");' '' --lib
mutate src/http.rs '                a.guarded(&c.key.principal, || {
                    a.store.create_fork(&fork, &location, &c.holder)
                })?' '                a.store.create_fork(&fork, &location, &c.holder)?' --test api
mutate src/http.rs 'let p = a.guarded(&c.key.principal, || {
            a.store.publish_draft(&name, &writer, &req.changeset)
        })?;' 'let p = a.store.publish_draft(&name, &writer, &req.changeset)?;' --test api
mutate src/http.rs 'let offset = a.guarded(&c.key.principal, || {
            a.store.record_landing_intent(&name, &writer, id)
        })?;' 'let offset = a.store.record_landing_intent(&name, &writer, id)?;' --test api
mutate src/http.rs 'let info = a.guarded(&c.key.principal, || {
            let info = a.store.ingest_blob(oid, size, content)?;' 'let info = a.guarded(&cuenca_core::PrincipalName::new("unrevoked").unwrap(), || {
            let info = a.store.ingest_blob(oid, size, content)?;' --test api
# Batched upload (BRO-3416): its limits, its framing, possession of every blob, and
# the revocation check right before its commit.
mutate src/http.rs 'if oids.len() == MAX_BATCH_BLOBS {' 'if false {' --test api a_blob_batch_over
mutate src/http.rs '    if size > MAX_BATCH_BYTES {' '    if false {' --test api a_blob_batch_over
mutate src/http.rs 'Ok(0) if got == 0 => return Ok(None),' 'Ok(0) => return Ok(None),' --test api a_blob_batch_is_all
mutate src/http.rs 'for oid in oids {' 'for oid in oids.into_iter().take(1) {' --test api a_blob_batch_is_stored
mutate src/http.rs '        a.guarded(&c.key.principal, || {
            batch.commit()?;' '        a.guarded(&cuenca_core::PrincipalName::new("unrevoked").unwrap(), || {
            batch.commit()?;' --test api a_write_in_flight


# Verify records (BRO-3254): the holder's only, the request pinned, a moved base answered
# with the main to rerun against.
mutate src/http.rs '#[serde(deny_unknown_fields)]
struct Verdict {' 'struct Verdict {' --test api a_verify_is_recorded
mutate src/http.rs '                        "main": main.to_hex(),' '' --test api a_verify_is_recorded
mutate src/http.rs '        let (offset, manifest) = a.guarded(&c.key.principal, || {
            a.store.record_verify(&name, &writer, id, base, req.checks)' '        let (offset, manifest) = a.guarded(&c.key.principal, || {
            a.store.record_verify(&name, &Writer::new(a.store.fork(&name)?.and_then(|f| f.holder().cloned()).unwrap(), req.epoch), id, base, req.checks)' --test api a_verify_is_recorded

# Landing: idempotent against the remote at every step, refusals recorded.
mutate src/land.rs 'Some(at) if at == export.commit => return Ok(()),' 'Some(at) if false => return Ok(()),' --test land a_kill
mutate src/land.rs '        if let Some(found) = gh.find(export)? {
            return Ok(found);
        }' '' --test land a_kill
mutate src/land.rs 'if !pushed && !git.has_commit(export.commit)? {' 'if false {' --test land a_kill
mutate src/land.rs '                    self.a.store.record_export_intent(name, id, &export)?;' '' --test land a_draft_lands
mutate src/land.rs '            Some(at) => {
                return Err(LandError::Fail(format!(' '            Some(_) if false => {
                return Err(LandError::Fail(format!(' --test land a_branch_that
mutate src/land.rs '                && t == tree.to_hex()' '' --test land a_remote_without
mutate src/land.rs '            let Some(remote) = location.remote.clone() else {
                return Ok(());
            };' '            let remote = location.remote.clone().unwrap_or_default();' --test land without_a_remote
mutate src/admin.rs '                && expected != requested' '                && false' --test land without_a_remote
mutate src/http.rs '            CoreError::NotVerified { .. } => (S::CONFLICT, "not_verified"),' '' --test api publishes_and

mutate src/land.rs '            && tree_of(ids[0])? == changeset.target.to_hex()' '' --test land a_branch_that
mutate src/land.rs '                    let export = match git.remote_branch(&remote, &export_branch(id))? {
                        Some(at) => git.adopt(&remote, &self.a.store, id, landing.main, at)?,
                        None => self.build(&git, &remote, &landing, None)?,
                    };' '                    let export = self.build(&git, &remote, &landing, None)?;' --test land a_branch_with_the_drafts_tree
mutate src/land.rs '            Ok(None) if std::time::Instant::now() >= deadline => {' '            Ok(None) if false => {' --test land a_hung_call

mutate src/land.rs '            if !on_main.contains(&oid)' '            if false' --test land a_fork_with_several
mutate src/land.rs '        let main = landing.main;' '        let main = changeset.base;' --test land a_fork_with_several
mutate src/land.rs '            Some(p) if p.head_ref_oid == export.commit.to_hex() => Ok(Some((p.number, p.url))),' '            Some(p) => Ok(Some((p.number, p.url))),' --test land a_pull_request_from

mutate src/land.rs '            && tree_of(ids[1])? == main.to_hex();' ';' --test land a_branch_with_the_drafts_tree
mutate src/land.rs '                    if !pushed {
                        git.push(&remote, export)?;
                    }' '' --test land a_kill

mutate src/land.rs '        let ours = ids.len() == 2' '        let ours = ids.len() >= 2' --test land a_merge_commit

# The volume (materialise.rs): a tree is built in staging and swapped in whole; modes are
# read-only; a killed pass leaves nothing; one materialiser; records skip work; a failed
# tree waits before it is tried again.
mutate src/materialise.rs '        cuenca_fs::checkout(&full, staged, &self.objects)?;' '        let here = self.root.join(what.rel());
        remove_any(&here)?;
        cuenca_fs::checkout(&full, &here, &self.objects)?;
        fs::rename(&here, staged)?;' --test materialise killed_while_materialising
mutate src/materialise.rs '        set_modes(staged, Modes::of(owner))?;' '' --test materialise a_draft_is_shown
mutate src/materialise.rs 'const FILE_MODE: u32 = 0o640;' 'const FILE_MODE: u32 = 0o644;' --test materialise a_draft_is_shown
mutate src/materialise.rs 'const DIR_MODE: u32 = 0o2750;' 'const DIR_MODE: u32 = 0o2755;' --test materialise a_draft_is_shown
mutate src/materialise.rs '.permissions().mode() & 0o100 != 0;' '.permissions().mode() & 0o100 == 0;' --test materialise a_draft_is_shown
mutate src/materialise.rs '            fs::remove_dir_all(&staging)?;' '' --test materialise killed_while_materialising
mutate src/materialise.rs 'FlockOperation::NonBlockingLockExclusive' 'FlockOperation::NonBlockingLockShared' --test materialise one_materialiser
mutate src/materialise.rs '            if self.shown.get(&what) == Some(&tree)
' '            if false && self.shown.get(&what) == Some(&tree)
' --test materialise a_reopened_volume
mutate src/materialise.rs '                && fs::symlink_metadata(self.root.join(what.rel())).is_ok_and(|m| m.is_dir())' '                && fs::symlink_metadata(self.root.join(what.rel())).is_ok()' --test materialise a_reopened_volume
mutate src/materialise.rs '        write_record(&record, tree)?;' '' --test materialise a_reopened_volume
mutate src/materialise.rs '                && at.elapsed() < self.retry_after' '                && false' --test materialise a_tree_that_fails
mutate src/materialise.rs '        if let Err(e) = remove_any(&staged)' '        if let Err(e) = Ok::<(), io::Error>(())' --test materialise a_draft_is_shown
mutate src/materialise.rs '        fs::set_permissions(root, fs::Permissions::from_mode(DIR_MODE))?;' '' --test materialise a_draft_is_shown
mutate src/materialise.rs '    match rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::EXCHANGE) {' '    let _ = remove_any(dest);
    match rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::EXCHANGE) {' --test materialise a_reader_never_sees
mutate src/materialise.rs '            .any(|(_, at)| at.elapsed() >= self.retry_after)' '            .any(|(_, at)| at.elapsed() >= self.retry_after && false)' --test materialise a_failed_tree_is_tried_again
mutate src/materialise.rs ' || volume.retry_due()' '' --test materialise a_failed_tree_is_tried_again
mutate src/materialise.rs '    let tmp = path.with_file_name(format!(".{name}.tmp"));' '    let tmp = path.with_extension("tmp");' --test materialise records_of_dotted_names

# Narrower windows than delete-then-rename: three renames (aside, in, back), and one
# exchange per top-level entry (never missing, but mixed). The reader catches both.
mutate src/materialise.rs '    match rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::EXCHANGE) {' '    if dest.exists() {
        let aside = staged.with_extension("aside");
        fs::rename(dest, &aside)?;
        fs::rename(staged, dest)?;
        return fs::rename(&aside, staged);
    }
    match rustix::fs::renameat_with(CWD, staged, CWD, dest, RenameFlags::EXCHANGE) {' --test materialise a_reader_never_sees
mutate src/materialise.rs '            swap_into_place(staged, &dest)?;' '        if dest.exists() {
            for e in fs::read_dir(staged)? {
                let e = e?;
                let to = dest.join(e.file_name());
                if fs::symlink_metadata(&to).is_ok() {
                    rustix::fs::renameat_with(CWD, e.path(), CWD, &to, RenameFlags::EXCHANGE)
                        .map_err(io::Error::from)?;
                } else {
                    fs::rename(e.path(), &to)?;
                }
            }
        } else {
            swap_into_place(staged, &dest)?;
        }' --test materialise a_reader_never_sees

# The record follows the swap: written first, a failed swap leaves it naming a tree
# that is not shown.
mutate src/materialise.rs '        forget_record(&record)?;
        if owner {' '        forget_record(&record)?;
        write_record(&record, tree)?;
        if owner {' --test materialise a_failed_swap_records_nothing
# What the path shows is forgotten before the swap, in memory and on disk: kept, a
# failure after the swap leaves a record of a tree no longer shown.
mutate src/materialise.rs '        self.shown.remove(what);
' '' --test materialise a_tree_whose_record_failed
mutate src/materialise.rs '        forget_record(&record)?;
' '' --test materialise a_tree_whose_record_failed
# A record that cannot be forgotten stops the swap (BRO-3447).
mutate src/materialise.rs '        forget_record(&record)?;
' '        let _ = forget_record(&record);
' --test materialise a_record_that_cannot_be_forgotten

# Owner forks (part 2): only owner-* is the volume's, and owner forks never move
# (cuenca-core's own mutants cover the rules); shown once, writable, never exchanged or
# rebuilt; adopted without a record; snapshotted into drafts under .forkignore; a
# failure publishes nothing.
mutate src/materialise.rs 'const OWNER_DIR_MODE: u32 = 0o2770;' 'const OWNER_DIR_MODE: u32 = 0o2750;' --test owner_forks shown_once
mutate src/materialise.rs 'const OWNER_FILE_MODE: u32 = 0o660;' 'const OWNER_FILE_MODE: u32 = 0o640;' --test owner_forks shown_once
mutate src/materialise.rs 'const OWNER_EXEC_MODE: u32 = 0o770;' 'const OWNER_EXEC_MODE: u32 = 0o660;' --test owner_forks shown_once
mutate src/materialise.rs '            if owner && self.owner_shown(&what, tree)? {' '            if false && self.owner_shown(&what, tree)? {' --test owner_forks shown_once
mutate src/materialise.rs '        if owner {
            // Shown once' '        if false {
            // Shown once' --lib an_owner_fork_is_never_exchanged
mutate src/materialise.rs '        if !self.shown.contains_key(what) {
            write_record' '        if false {
            write_record' --test owner_forks adopted
mutate src/materialise.rs '    if snap.is_clean() {' '    if false {' --test owner_forks shown_once
mutate src/materialise.rs '    cuenca_fs::snapshot_with(dir, head, cache, &options)' '    let none = cuenca_fs::ForkIgnore::parse("").unwrap();
    let options = cuenca_fs::SnapshotOptions { rules: Some(&none), drop_ignored: true, ..options };
    cuenca_fs::snapshot_with(dir, head, cache, &options)' --test owner_forks finder_metadata
# A small file is uploaded as the scan read it, so a client still writing cannot fail
# the snapshot (BRO-3456).
mutate src/materialise.rs '        keep: KEEP,' '        keep: cuenca_fs::Keep::default(),' --lib a_small_file_changed_after_the_scan
# A scan that publishes nothing puts the stat cache back (P20 r8 A).
mutate src/materialise.rs 'publish(store, fork, scanned.snap).inspect_err(|_| *cache = scanned.before)' 'publish(store, fork, scanned.snap)' --lib a_failed_snapshot_leaves_the_cache
mutate src/admin.rs '    if principal.as_str() == cuenca_core::VOLUME_PRINCIPAL {' '    if false {' --test owner_forks keeps_the_volume_keyless
mutate src/http.rs '            CoreError::OwnerFork(_) => (S::FORBIDDEN, "owner_fork"),' '' --test owner_forks no_key_creates
mutate src/admin.rs '            let main = ignored_in_main(a, &location)?;' '            let main = a.store.main_tree(&location).map_err(|e| s(&e))?.git_tree_id();' --lib an_owner_fork_of_a_main_tracking
mutate src/admin.rs '        .filter(|p| rules.excludes(p))' '        .filter(|p| rules.is_ignored(p, false))' --lib an_owner_fork_of_a_main_tracking
mutate src/authority.rs '            .any(|k| k.principal.as_str() == cuenca_core::VOLUME_PRINCIPAL)' '            .any(|_| false)' --lib a_volume_key_on_file
mutate src/materialise.rs 'rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK' 'rustix::fs::OFlags::NONBLOCK' --lib an_upload_opens_only
mutate src/materialise.rs '    if !file.metadata().map_err(|e| fail(&e))?.is_file() {' '    if false {' --lib an_upload_opens_only
mutate src/materialise.rs '            if !is_owner(&fork) {' '            if is_owner(&fork) {' --test owner_forks
mutate src/materialise.rs '                if snapped_at.is_none_or(|t| t.elapsed() >= config.snapshot_interval) {' '                if snapped_at.is_none() {' --test owner_forks a_draft_within_60_s


# Forking an owner fork (part 3): it is snapshotted first, under the volume's lock, and
# a snapshot that keeps failing refuses the fork; only a fork the caller reads is forked;
# an owner fork's main and its rules are checked strictly, and the fork starts from that
# main.
mutate src/authority.rs '            let error = match locked.snapshot_owner_fork(&self.store, &source) {' '            let error = match None::<Result<crate::materialise::SnapshotOne, String>> {' --test owner_forks unsnapshotted_edits
# The volume is unlocked between attempts (BRO-3456).
mutate src/authority.rs '            drop(locked);
' '' --test owner_forks leaves_the_volume_to_others
mutate src/authority.rs '        self.guarded(principal, || self.store.check_branch(fork, from, holder))?;' '' --test owner_forks refused_fork
mutate src/http.rs '                a.branch_fork(&fork, &from, &c.holder)?' '                a.guarded(&c.key.principal, || a.branch_fork(&fork, &from, &c.holder))?' --test owner_forks revocation_does_not_wait
mutate src/authority.rs '            .filter(|_| source.holder().is_some_and(Holder::is_volume))' '            .filter(|_| false)' --test owner_forks unsnapshotted_edits
mutate src/authority.rs '                Some(Err(error)) => error,' '                Some(Err(error)) if false => error,
                Some(Err(_)) => {
                    return Ok(self.guarded(principal, || {
                        self.store.branch_fork(fork, from, holder)
                    })?);
                }' --test owner_forks cannot_be_snapshotted
mutate src/http.rs '                let source = visible_fork(a, &c, &from)?;' '                let source = a.store.fork(&from)?.ok_or_else(ApiError::not_found)?;' --test owner_forks needs_reading
mutate src/admin.rs 'cuenca_fs::ForkIgnore::for_tree(&main, |oid|' 'cuenca_fs::ForkIgnore::for_tree(&Tree::new(), |oid|' --lib an_owner_fork_of_a_main_tracking

mutate src/authority.rs 'std::time::Duration::from_secs(5);' 'std::time::Duration::from_secs(0);' --test owner_forks tried_again
mutate src/authority.rs '                    error: without_host_paths(&error, locked.root(), from),' '                    error,' --test owner_forks cannot_be_snapshotted
mutate src/authority.rs '        .replace(&dir, fork.as_str())' '' --lib a_snapshot_error_names_no_host_path
mutate src/authority.rs '.replace(&format!("{root}/"), "<volume>/")' '.replace(&root, "<volume>")' --lib a_snapshot_error_names_no_host_path
mutate src/http.rs '                visible_location(a, &c, &source.location)?;' '' --test owner_forks needs_reading

# Only what the scan read is uploaded: a folder swapped for a link since never brings an
# outside file in.
mutate src/materialise.rs '            cuenca_fs::Content::File(path) => batch.add(blob.oid, blob.size, open_regular(path)?),' '            cuenca_fs::Content::File(path) => {
                let now = fs::read(path).map_err(|e| e.to_string())?;
                batch.add(GitOid::for_blob(&now), now.len() as u64, now.as_slice())
            }' --lib swapped_for_a_link
mutate src/materialise.rs '        added.map_err(|e| match (&blob.content, e) {' '        added.map_err(|e| match (&blob.content, e) {
            (_, e) if true => e.to_string(),' --lib swapped_for_a_link

exit $survived
