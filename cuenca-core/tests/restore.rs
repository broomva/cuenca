//! Backup import admission, atomicity and a deliberately held read snapshot.
mod common;
use common::*;
use cuenca_core::checks::{CheckRun, CheckStatus, MANIFEST_PATH, Manifest};
use cuenca_core::{ChangesetId, CoreError, Event, JournalPrefix, Record, Store};
use cuenca_tree::{ContentHash, Entry, GitOid, Tree};
use std::collections::BTreeMap;

const CHECKS: &[u8] = b"[[check]]\nname = \"gate\"\nargv = [\"true\"]\n";
fn fixture() -> (tempfile::TempDir, Store, JournalPrefix) {
    let dir = tempfile::tempdir().unwrap();
    let source = Store::open(dir.path()).unwrap();
    let main = tree(&source, &[("base", b"base"), (MANIFEST_PATH, CHECKS)]);
    source.add_location(&loc("l"), None, &main).unwrap();
    source.create_fork(&fork("f"), &loc("l"), &agent()).unwrap();
    let cs = write(&source, &main, &[("draft", b"draft")]);
    let id = source.publish_draft(&fork("f"), &w1(), &cs).unwrap().id;
    let checks = Manifest::parse(CHECKS)
        .unwrap()
        .checks
        .into_iter()
        .map(|check| CheckRun {
            check,
            status: CheckStatus::Passed,
            exit_code: Some(0),
            duration_ms: 1,
        })
        .collect();
    source
        .record_verify(&fork("f"), &w1(), id, main.git_tree_id(), checks)
        .unwrap();
    let prefix = source.export_prefix().unwrap();
    (dir, source, prefix)
}
fn target(source: &Store) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let target = Store::open(dir.path()).unwrap();
    for oid in source.export_prefix().unwrap().reachable_blobs().unwrap() {
        let bytes = source.read_blob(oid).unwrap();
        target
            .ingest_blob(oid, bytes.len() as u64, bytes.as_slice())
            .unwrap();
    }
    (dir, target)
}
fn unchanged(target: &Store, prefix: &JournalPrefix) {
    assert_eq!(target.len().unwrap(), 0);
    assert!(target.locations().unwrap().is_empty());
    assert!(target.forks().unwrap().is_empty());
    assert_eq!(target.replay_check().unwrap().records, 0);
    for oid in prefix.trees.keys() {
        assert!(target.tree(*oid).is_err(), "tree committed on refusal");
    }
}

#[test]
fn import_round_trips_records_and_refuses_nonempty_destinations() {
    let (_d, source, prefix) = fixture();
    let (_t, target) = target(&source);
    target.import_prefix(&prefix).unwrap();
    assert_eq!(target.records(0, usize::MAX).unwrap(), prefix.records);
    assert_eq!(target.forks().unwrap(), source.forks().unwrap());
    assert_eq!(
        target.replay_check().unwrap(),
        source.replay_check().unwrap()
    );
    assert!(target.import_prefix(&prefix).is_err());
    // Replaying duplicate events already refuses a nonempty prefix. An empty
    // prefix must ALSO refuse a nonempty destination: this pins the entry guard.
    let empty = JournalPrefix {
        records: vec![],
        trees: BTreeMap::new(),
    };
    assert!(target.import_prefix(&empty).is_err());
    assert_eq!(target.len().unwrap(), prefix.records.len() as u64);
}
#[test]
fn import_rejects_missing_and_unreferenced_roots() {
    let (_d, source, original) = fixture();
    for extra in [false, true] {
        let mut prefix = original.clone();
        let (_t, target) = target(&source);
        if extra {
            let bytes = b"unreferenced";
            let oid = GitOid::for_blob(bytes);
            target
                .ingest_blob(oid, bytes.len() as u64, bytes.as_slice())
                .unwrap();
            let tree = Tree::from_entries([(path("extra"), Entry::File(oid))]).unwrap();
            prefix.trees.insert(tree.git_tree_id(), tree);
        } else {
            prefix.trees.pop_first();
        }
        let error = target.import_prefix(&prefix).unwrap_err();
        assert!(
            error.to_string().contains("restore tree roots mismatch"),
            "{error}"
        );
        unchanged(&target, &prefix);
    }
}
#[test]
fn import_rejects_a_tree_under_the_wrong_git_id() {
    let (_d, source, mut prefix) = fixture();
    let (_t, target) = target(&source);
    let oid = *prefix.trees.keys().next().unwrap();
    prefix.trees.insert(oid, Tree::new());
    assert!(matches!(target.import_prefix(&prefix), Err(CoreError::CorruptTree(at)) if at == oid));
    unchanged(&target, &prefix);
}
#[test]
fn import_checks_blob_presence_and_content_admission() {
    for symlink in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let target = Store::open(dir.path()).unwrap();
        let bytes = b"outside\0invalid";
        let oid = GitOid::for_blob(bytes);
        if symlink {
            target
                .ingest_blob(oid, bytes.len() as u64, bytes.as_slice())
                .unwrap();
        }
        let entry = if symlink {
            Entry::Symlink(oid)
        } else {
            Entry::File(oid)
        };
        let tree = Tree::from_entries([(path("link"), entry)]).unwrap();
        let root = tree.git_tree_id();
        let prefix = JournalPrefix {
            records: vec![Record {
                offset: 0,
                at_ms: 0,
                event: Event::LocationAdded {
                    location: loc("l"),
                    remote: None,
                    main: root,
                },
            }],
            trees: BTreeMap::from([(root, tree)]),
        };
        assert!(target.import_prefix(&prefix).is_err());
        unchanged(&target, &prefix);
    }
}
#[test]
fn import_rejects_invalid_remotes() {
    let (_d, source, mut prefix) = fixture();
    let (_t, target) = target(&source);
    if let Event::LocationAdded { remote, .. } = &mut prefix.records[0].event {
        *remote = Some("-forged".into());
    } else {
        panic!();
    }
    assert!(matches!(
        target.import_prefix(&prefix),
        Err(CoreError::BadRemote(_))
    ));
    unchanged(&target, &prefix);
}
#[test]
fn import_reapplies_changesets_to_check_their_target() {
    let (_d, source, mut prefix) = fixture();
    let (_t, target) = target(&source);
    prefix.records.truncate(3);
    let wrong = tree(&source, &[("forged", b"wrong target")]);
    for (_, e) in wrong.entries() {
        let bytes = source.read_blob(e.oid()).unwrap();
        target
            .ingest_blob(e.oid(), bytes.len() as u64, bytes.as_slice())
            .unwrap();
    }
    if let Event::DraftPublished { changeset, id, .. } = &mut prefix.records[2].event {
        prefix.trees.remove(&changeset.target);
        changeset.target = wrong.git_tree_id();
        *id = ChangesetId::of(changeset);
    } else {
        panic!();
    }
    prefix.trees.insert(wrong.git_tree_id(), wrong);
    assert!(matches!(
        target.import_prefix(&prefix),
        Err(CoreError::Changeset(_))
    ));
    unchanged(&target, &prefix);
}
#[test]
fn import_rejects_changeset_ids_offsets_and_duplicate_events_atomically() {
    let (_d, source, original) = fixture();
    let mut prefix = original.clone();
    let (_t, target) = target(&source);
    if let Event::DraftPublished { id, .. } = &mut prefix.records[2].event {
        *id = ChangesetId::from_hex(&"00".repeat(32)).unwrap();
    } else {
        panic!();
    }
    let error = target.import_prefix(&prefix).unwrap_err();
    assert!(error.to_string().contains("restore changeset id"));
    unchanged(&target, &prefix);
    let mut prefix = original.clone();
    let (_t, target) = self::target(&source);
    prefix.records[1].offset = 99;
    assert!(matches!(
        target.import_prefix(&prefix),
        Err(CoreError::CorruptLog { .. })
    ));
    unchanged(&target, &prefix);
    source
        .revoke(&cuenca_core::PrincipalName::new("retired").unwrap())
        .unwrap();
    let mut prefix = source.export_prefix().unwrap();
    let (_t, target) = self::target(&source);
    let mut duplicate = prefix.records.last().unwrap().clone();
    duplicate.offset += 1;
    prefix.records.push(duplicate);
    let error = target.import_prefix(&prefix).unwrap_err();
    assert!(error.to_string().contains("restore duplicated event"));
    unchanged(&target, &prefix);
}
#[test]
fn import_rechecks_verify_manifest_identity_and_declared_checks() {
    let (_d, source, original) = fixture();
    for kind in 0..3 {
        let mut prefix = original.clone();
        let (_t, target) = target(&source);
        if let Event::VerifyRecorded {
            manifest, checks, ..
        } = &mut prefix.records.last_mut().unwrap().event
        {
            match kind {
                0 => manifest.as_mut().unwrap().sha256 = ContentHash::of(b"forged"),
                1 => checks[0].check.argv = vec!["false".into()],
                _ => checks.clear(),
            }
        } else {
            panic!();
        }
        let error = target.import_prefix(&prefix).unwrap_err();
        assert!(matches!(error, CoreError::ManifestMismatch(_)), "{error}");
        unchanged(&target, &prefix);
    }
}
#[test]
fn held_snapshot_exports_the_original_cut_after_later_writes_commit() {
    let (_d, source, expected) = fixture();
    let snapshot = source.snapshot().unwrap();
    source
        .add_location(&loc("after"), None, &Tree::new())
        .unwrap();
    let frozen = snapshot.export_prefix().unwrap();
    let latest = source.export_prefix().unwrap();
    assert_eq!(frozen.records, expected.records);
    assert_eq!(frozen.trees, expected.trees);
    assert_eq!(frozen.records.len() + 1, latest.records.len());
    assert_ne!(frozen.records, latest.records);
}
