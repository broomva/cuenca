#!/usr/bin/env bash
# Mutation check for cuenca-fs: each mutant breaks one guarantee, and the tests must
# fail for every one. Run from the crate directory: scripts/mutation-check.sh
# Prints one line per mutant and exits non-zero if any mutant survives. The source is
# restored after each mutant, also on interrupt.
set -uo pipefail
cd "$(dirname "$0")/.."
. ../scripts/mutation-guard.sh
backup=$guard_backup
lock=$(git rev-parse --show-toplevel)/Cargo.lock
cp "$lock" "$backup/Cargo.lock"
restore() { [ -n "${current:-}" ] && cp "$backup/file" "$current"; current=; cp "$backup/Cargo.lock" "$lock"; }
trap 'restore; guard_cleanup' EXIT
trap 'exit 130' INT TERM

# A mutant only counts as killed against a suite that passes unmutated.
if ! bounded cargo test -q -p cuenca-fs >"$backup/log" 2>&1; then
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
  bounded cargo test -q -p cuenca-fs "$@" >"$backup/log" 2>&1; local status=$?
  if [ "$status" -eq 0 ]; then
    if ! grep -qE 'test result: ok\. [1-9]' "$backup/log"; then
      echo "NO TESTS  $file: $old (the filter $* ran no test)"; survived=1
    else
      echo "SURVIVED  $file: $old"; survived=1
    fi
  elif grep -qE '^error\[E[0-9]+\]|could not compile|failed to select a version' "$backup/log"; then
    echo "NO BUILD  $file: $old (a mutant that does not build proves nothing)"; survived=1
  elif [ "$status" -eq 124 ] || [ "$status" -eq 125 ]; then
    echo "killed    $file: $old ($(tail -n 1 "$backup/log"))"
  else
    echo "killed    $file: $old"
  fi
  restore
}

# .forkignore
mutate src/ignore.rs 'Errno::NOENT) => return Ok((Self::defaults(), None)),' 'Errno::NOENT) => return Ok((Self::parse("")?, None)),' --test fs forkignored
mutate src/ignore.rs 'let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;' 'let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;' --lib symlink_or_a_directory
mutate src/ignore.rs 'if !file.metadata().map_err(FsError::io(&path))?.is_file() {' 'if false {' --lib symlink_or_a_directory
mutate src/ignore.rs 'if self.is_ignored(&path[..end], true) {' 'if false {' --lib excludes
mutate src/scan.rs 'if ignore.is_ignored(&rel, kind == FileType::Directory) {' 'if false {' --test fs forkignore
mutate src/scan.rs 'if held != parsed.map(GitOid::for_blob) {' 'if false {' --lib rules_that_changed
mutate src/scan.rs 'if after.as_deref() != parsed {' 'if false {' --lib rules_changed_after
# base paths the rules name are dropped, and reported

# what a snapshot holds
mutate src/scan.rs 'if bytes == b"." || bytes == b".." || bytes == b".git" {' 'if bytes == b"." || bytes == b".." {' --test fs git_state
mutate src/scan.rs '                    skipped.push(Skipped {
                        path: rel_str,
                        reason: "git reserves the name",
                    });
                    continue;' '                    continue;' --test fs fails_loudly
mutate src/scan.rs 'FileType::RegularFile => {' 'FileType::RegularFile | FileType::Fifo | FileType::Socket => {' --test fs git_state
mutate src/scan.rs 'let entry = if stamp.is_executable() {' 'let entry = if false {' --test fs
mutate src/scan.rs 'if stamp.size() > MAX_FILE_BYTES {' 'if false {' --test fs blob_limit
mutate src/scan.rs 'check_content(&path, Mode::File, content)?;' '' --test fs gitattributes
mutate src/scan.rs 'if !known.insert(oid) {' 'if false {' --test fs new_blobs
mutate src/scan.rs '            Err(Errno::NOENT) => {
                budget.give_back(held);
                return Ok(None);
            }' '' --lib removed_after_the_listing
# Kept content is the bytes that were hashed, within both bounds; a read not kept gives
# its bytes back (BRO-3456).
mutate src/scan.rs '                    Some(bytes) if keep => Content::Inline(bytes),' '' --lib within_the_budget
mutate src/scan.rs '        if size > self.per_file {' '        if false {' --lib within_the_budget
mutate src/scan.rs '        match left.checked_sub(size) {' '        match Some(*left) {' --lib within_the_budget
mutate src/scan.rs '                budget.give_back(held);
                return Ok(None);' '                return Ok(None);' --lib removed_after_the_listing
mutate src/scan.rs '            Ok(None) => budget.give_back(held),' '            Ok(None) => {}' --lib read_again_takes_from_the_budget_once
# never leaving the root
mutate src/scan.rs 'let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;' 'let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;' --lib renamed_out_of_the_root
mutate src/scan.rs '        Err(Errno::LOOP | Errno::NOTDIR) => return Err(FsError::Unstable(p.abs.clone())),
        Err(Errno::NOENT) => return Ok(None),' '        Err(Errno::LOOP | Errno::NOTDIR) => return Err(FsError::Unstable(p.abs.clone())),' --lib swapped_for_a_symlink
mutate src/scan.rs 'if dev_ino(&fstat(&fd).map_err(errno(&p.abs))?) != p.dev_ino {' 'if false {' --lib swapped_for_a_symlink
mutate src/scan.rs 'let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;' 'let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;' --lib replaced_by_a_symlink
mutate src/scan.rs 'if after != stamp {' 'if false {' --lib stale_stamp
# the stat cache
mutate src/scan.rs 'match cache.lookup(path.as_str(), &stamp) {' 'match None::<Entry> {' --test fs stat_cache
mutate src/scan.rs 'next.record(f.path.to_string(), stamp, f.entry, started);' '' --test fs stat_cache
mutate src/scan.rs '    rules_unchanged(root, parsed.as_deref())?;
    *cache = next;' '    rules_unchanged(root, parsed.as_deref())?;' --test fs stat_cache
mutate src/cache.rs '.filter(|c| c.stamp == *stamp)' '.filter(|_| true)' --test fs stat_cache
mutate src/cache.rs 'ctime: (meta.ctime(), meta.ctime_nsec()),' 'ctime: (0, 0),' --lib raw_stat
mutate src/cache.rs 'ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),' 'ctime: (0, 0),' --test fs stat_cache
mutate src/cache.rs 'let at = at.saturating_sub(RACY_MARGIN);' '' --test fs just_now
# checkout
mutate src/checkout.rs '        dir_flags | OFlags::NOFOLLOW,' '        dir_flags,' --test fs link_or_a_file
mutate src/checkout.rs '    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut at' '    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut at' --lib inside_the_destination
mutate src/checkout.rs 'if !matches!(item.file_name().to_bytes(), b"." | b"..") {' 'if false {' --test fs non_empty
mutate src/checkout.rs 'verify(oid, &bytes)?;' '' --test fs wrong_or_missing
mutate src/checkout.rs 'verify(oid, &target)?;' '' --test fs wrong_or_missing
mutate src/checkout.rs 'if now != stamp {' 'if false {' --lib vouched_file_that_changed
mutate src/checkout.rs 'let stamp = verify_file(oid, &file, &abs)?;' 'let stamp = Stamp::of(&file.metadata().map_err(FsError::io(&abs))?);' --test fs corrupt_object
mutate src/checkout.rs 'if !meta.is_file() || meta.nlink() != 1 {' 'if !meta.is_file() {' --lib hard_link
mutate src/checkout.rs '&& Stamp::of(&file.metadata()?) == *stamp' '&& true' --test fs edited_since
mutate src/checkout.rs 'if Blob::of(&content).map(|b| b.oid()) == Some(oid) {' 'if true {' --test fs edited_since
mutate src/checkout.rs '.permissions() != mode {' '.permissions() == u32::MAX {' --test fs every_source

mutate src/scan.rs 'if !dropped.is_empty() && !drop_ignored {' 'if false {' --test fs refused_unless
mutate src/checkout.rs 'Fetched::Vouched(v) if v.oid == oid => {' 'Fetched::Vouched(v) if true => {' --test fs its_own_blob
mutate src/checkout.rs 'if has_content_rule(path, entry.mode()) {' 'if false {' --test fs refused_at_checkout
mutate src/checkout.rs 'check_content(path, entry.mode(), &bytes)?;' '' --test fs refused_at_checkout

mutate src/checkout.rs 'file.take(MAX_SYMLINK_TARGET as u64 + 1)' 'file.take(MAX_FILE_BYTES + 1)' --test fs refused_unread
mutate src/scan.rs '            ignored_files.contains(*path)
                || (ignored_dirs' '            false
                || (ignored_dirs' --test fs refused_unless

mutate src/scan.rs '.any(|(i, _)| ignored_dirs.contains(&path[..i]))' '.any(|(i, _)| ignored_dirs.contains(&path[..i]) || ignored_files.contains(&path[..i]))' --test fs became_a_file

mutate src/scan.rs '|| (ignored_dirs.contains(*path) && ignore.is_ignored(path, false))' '|| false' --test fs both_kinds_is_refused
mutate src/scan.rs '|| (ignored_dirs.contains(*path) && ignore.is_ignored(path, false))' '|| ignored_dirs.contains(*path)' --test fs became_a_directory_under_a_directory_rule

# The macOS clone must not follow a link planted at the name (Linux creates with O_EXCL).
if [ "$(uname)" = Darwin ]; then
  mutate src/checkout.rs '            CLONE_NOFOLLOW,' '            0,' --test fs planted_at_a_file_name
  # A clone would carry the source's flags (uchg): such a file is copied.
  mutate src/checkout.rs 'st_flags(&src.metadata()?) != 0 {' 'st_flags(&src.metadata()?) == u32::MAX {' --test fs locked_source
fi

# A checkout creates dest, never its parent, and never writes what a snapshot refuses.
mutate src/checkout.rs '    let parent =
        openat(CWD, parent,' '    let _ = fs::create_dir_all(parent);
    let parent =
        openat(CWD, parent,' --test fs parent_of_dest
mutate src/checkout.rs '    too_large(abs, stamp.size())?;' '' --test fs larger_than_a_snapshot
# dest is examined, created and opened relative to one parent descriptor; reopening it
# by path lets a writer of the parent's path split created from written (P20 r10, A).
mutate src/checkout.rs '    let fd = match openat(
        &parent,
        &*name,' '    let fd = match openat(
        CWD,
        dest,' --test fs parent_swapped
mutate src/checkout.rs '            too_large(&abs, bytes.len() as u64)?;' '' --test fs larger_than_a_snapshot


# Rules read from a stored tree: as strict as from disk, with the defaults when absent.
mutate src/ignore.rs '            Some(_) => Err(not_a_file()),' '            Some(e) => Self::from_bytes(&read(e.oid()).map_err(FsError::Ignore)?),' --lib rules_from_a_tree
mutate src/ignore.rs '        let text = std::str::from_utf8(bytes)
            .map_err(|_| FsError::Ignore("the file is not UTF-8".into()))?;
        Self::parse(text)' '        Self::parse(&String::from_utf8_lossy(bytes))' --lib rules_from_a_tree
mutate src/ignore.rs '            None => Ok(Self::defaults()),' '            None => Self::parse(""),' --lib rules_from_a_tree

exit $survived
