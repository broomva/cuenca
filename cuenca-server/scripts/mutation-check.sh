#!/usr/bin/env bash
# Mutation check for cuenca-server: each mutant breaks one guarantee, and the tests must
# fail for every one. Run from the crate directory: scripts/mutation-check.sh
# Prints one line per mutant and exits non-zero if any mutant survives. The source is
# restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
backup=$(mktemp -d)
lock=$(git rev-parse --show-toplevel)/Cargo.lock
cp "$lock" "$backup/Cargo.lock"
restore() { [ -n "${current:-}" ] && cp "$backup/file" "$current"; current=; cp "$backup/Cargo.lock" "$lock"; }
trap 'restore; rm -rf "$backup"' EXIT
trap 'exit 130' INT TERM

# A mutant only counts as killed against a suite that passes unmutated.
if ! cargo test -q -p cuenca-server >"$backup/log" 2>&1; then
  echo "baseline is red: fix the suite before reading any mutant verdict"; cat "$backup/log"; exit 2
fi
echo "baseline  green"

survived=0
mutate() { # file, old, new, then the cargo test arguments that should catch it
  local file=$1 old=$2 new=$3; shift 3
  current=$file; cp "$file" "$backup/file"
  if ! python3 - "$file" "$old" "$new" <<'PY'
import sys, pathlib
p = pathlib.Path(sys.argv[1]); s = p.read_text()
if s.count(sys.argv[2]) != 1: sys.exit("anchor not unique: " + sys.argv[2])
p.write_text(s.replace(sys.argv[2], sys.argv[3]))
PY
  then echo "ANCHOR MISSING  $file: $old"; survived=1; restore; return; fi
  if cargo test -q -p cuenca-server "$@" >"$backup/log" 2>&1; then
    echo "SURVIVED  $file: $old"; survived=1
  elif grep -qE '^error\[E[0-9]+\]|could not compile|failed to select a version' "$backup/log"; then
    echo "NO BUILD  $file: $old (a mutant that does not build proves nothing)"; survived=1
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
mutate src/http.rs '        let state = writable_fork(a, &c, &name)?;
        a.check_puts' '        let state = a.store.fork(&name)?.ok_or_else(ApiError::not_found)?;
        a.check_puts' --test api
mutate src/http.rs '    writable_fork(a, c, name)?;
    let writer' '    let writer' --test api
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
mutate src/http.rs 'let offset = a.guarded(&c.key.principal, || {
            a.store.create_fork(&fork, &location, &c.holder)
        })?;' 'let offset = a.store.create_fork(&fork, &location, &c.holder)?;' --test api
mutate src/http.rs 'let p = a.guarded(&c.key.principal, || {
            a.store.publish_draft(&name, &writer, &req.changeset)
        })?;' 'let p = a.store.publish_draft(&name, &writer, &req.changeset)?;' --test api
mutate src/http.rs 'let landed = a.guarded(&c.key.principal, || {
            a.store.record_landing_intent(&name, &writer, id, &request)
        })?;' 'let landed = a.store.record_landing_intent(&name, &writer, id, &request)?;' --test api
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
        if what.is_owner() {' '        forget_record(&record)?;
        write_record(&record, tree)?;
        if what.is_owner() {' --test materialise a_failed_swap_records_nothing
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
mutate src/materialise.rs '    let snap = cuenca_fs::snapshot(dir, &head, cache)' '    let snap = cuenca_fs::snapshot_with(dir, &head, cache, &cuenca_fs::SnapshotOptions { rules: Some(&cuenca_fs::ForkIgnore::parse("").unwrap()), drop_ignored: true })' --test owner_forks finder_metadata
mutate src/admin.rs '    if principal.as_str() == cuenca_core::VOLUME_PRINCIPAL {' '    if false {' --test owner_forks keeps_the_volume_keyless
mutate src/http.rs '            CoreError::OwnerFork(_) => (S::FORBIDDEN, "owner_fork"),' '' --test owner_forks no_key_creates
mutate src/admin.rs '            ignored_in_main(a, &location)?;' '' --lib an_owner_fork_of_a_main_tracking
mutate src/admin.rs '        .filter(|p| rules.excludes(p))' '        .filter(|p| rules.is_ignored(p, false))' --lib an_owner_fork_of_a_main_tracking
mutate src/admin.rs '        None => cuenca_fs::ForkIgnore::defaults(),' '        None => cuenca_fs::ForkIgnore::parse("").unwrap(),' --lib an_owner_fork_of_a_main_tracking
mutate src/authority.rs '            .any(|k| k.principal.as_str() == cuenca_core::VOLUME_PRINCIPAL)' '            .any(|_| false)' --lib a_volume_key_on_file
mutate src/materialise.rs 'rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK' 'rustix::fs::OFlags::NONBLOCK' --lib an_upload_opens_only
mutate src/materialise.rs '    if !file.metadata().map_err(|e| fail(&e))?.is_file() {' '    if false {' --lib an_upload_opens_only
mutate src/materialise.rs '            if !is_owner(&fork) {' '            if is_owner(&fork) {' --test owner_forks
mutate src/materialise.rs '                if snapped_at.is_none_or(|t| t.elapsed() >= config.snapshot_interval) {' '                if snapped_at.is_none() {' --test owner_forks a_draft_within_60_s

# Gate engine (BRO-3313): each built-in gate fails what it should, the base policy
# (never the candidate's) is the one compiled, main's history is read from the log, and
# the author the gates see is the key's kind.
mutate ../cuenca-core/src/gates/builtin.rs '.filter(|(_, re)| re.is_match(file.path.as_str()))' '.filter(|_| false)' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs '.filter(|(_, re)| re.is_match(content))' '.filter(|_| false)' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'Some(before) if before.oid() == entry.oid() => None,' 'Some(_) if false => None,' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'Some(c) if c.len() as u64 > self.0 =>' 'Some(c) if false && c.is_empty() =>' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'if cx.author == AuthorKind::Human {' 'if false {' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs '.filter(|rule| path.contains(rule.as_str()))' '.filter(|_| false)' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'let path = format!("/{}", file.path.as_str().to_lowercase());' 'let path = file.path.as_str().to_lowercase();' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'if recent < self.max {' 'if true {' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'Op::Delete { path } if !declared.covers(path.as_str()) => Some(Finding {' 'Op::Delete { path } if false && path.as_str().is_empty() => Some(Finding {' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'let held = cx.history.held.contains(&(path.clone(), oid));' 'let held = false;' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'let unchanged = cx.base.get(path.as_str()).is_some_and(|e| e.oid() == oid);' 'let unchanged = false;' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'if change.from == change.main {' 'if true {' --test gate_log
mutate ../cuenca-core/src/gates/builtin.rs 'if self.0.iter().any(|w| inside(w, cx.at_ms)) {' 'if true {' --test gate_log
mutate ../cuenca-core/src/gates/mod.rs 'if let Some(error) = &self.policy_error {' 'if let Some(error) = None::<&String> {' --test gate_log
mutate ../cuenca-core/src/store.rs 'let policy = self.base_file(&base, POLICY_PATH);' 'let policy = self.base_file(&self.tree(changeset.target)?, POLICY_PATH);' --test gate_log
mutate ../cuenca-core/src/store.rs '                for (path, entry) in load_tree(trees, main)?.entries() {
                    history.held.insert((path.clone(), entry.oid()));
                }' '' --test gate_log
mutate src/http.rs 'PrincipalKind::OwnerDevice => AuthorKind::Human,' 'PrincipalKind::OwnerDevice => AuthorKind::Agent,' --test gate_log
mutate src/http.rs '        let gates = a.store.evaluate_landing(&name, &writer, id, &request)?;' '        let gates = a.store.evaluate_landing(&name, &writer, id, &LandingRequest::new(AuthorKind::Agent))?;' --test gate_log

# P20 r1 (A): after an intent, the dry run answers the journaled verdict.
mutate ../cuenca-core/src/store.rs '            return Ok(self.journaled_landing(offset)?.gates);' '            let _ = offset;' --test gate_log

# P20 r1 (B): the check manifest is the base's; main's history includes landed content; the history search skips nothing it needs; declarations are bounded in bytes.
mutate ../cuenca-core/src/store.rs 'let checks = self.base_file(&base, CHECKS_PATH);' 'let checks = self.base_file(&self.tree(changeset.target)?, CHECKS_PATH);' --test gate_log
mutate ../cuenca-core/src/store.rs '            if let Op::Put { path, entry } = op {
                history.held.insert((path.clone(), entry.oid()));
            }' '' --test gate_log
mutate ../cuenca-core/src/store.rs 'if !history_tags().is_match(v.value()) {' 'if true {' --test gate_log
mutate ../cuenca-core/src/gates/mod.rs 'if bytes > MAX_DECLARATION_BYTES {' 'if false {' --test gate_log

# P20 r2 (A, B): an intent for a superseded head is not a retry; only used declarations are journaled.
mutate ../cuenca-core/src/state.rs '    check_writer(view, &state, writer)?;
    // The head first:' '    check_writer(view, &state, writer)?;
    if let Some(l) = &state.landing
        && l.changeset == id
    {
        return Ok(Some(l.offset));
    }
    // The head first:' --test gate_log
mutate ../cuenca-core/src/gates/mod.rs '            declarations: input.request.declarations.used(input.changeset),' '            declarations: input.request.declarations.clone(),' --test gate_log

exit $survived
