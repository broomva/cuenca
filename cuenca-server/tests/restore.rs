#[path = "support/backup_fixture.rs"]
mod fixture;
use cuenca_core::Store;
use cuenca_server::restore::{backup, restore};
use ed25519_dalek::SigningKey;

#[test]
fn every_historical_blob_and_recovery_state_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture::fixture(&dir.path().join("source"));
    let id = age::x25519::Identity::generate();
    let signer = SigningKey::from_bytes(&[7; 32]);
    let objects = dir.path().join("objects");
    let manifest = backup(&source, &objects, &id.to_public(), &signer, |_| Ok(())).unwrap();
    assert_eq!(manifest.blobs.len(), 4); // includes the overwritten historical draft
    let target = dir.path().join("restored");
    let receipt = restore(&objects, &target, &id, &signer.verifying_key()).unwrap();
    assert_eq!(receipt.source_offset, receipt.restored_offset);
    assert_eq!(receipt.verified_blobs, 4);
    let restored = Store::open(&target).unwrap();
    fixture::assert_same(&source, &restored);
    fixture::resume(&restored);
}

#[test]
fn empty_log_is_distinct_from_offset_zero_and_restore_never_overwrites() {
    let dir = tempfile::tempdir().unwrap();
    let source = Store::open(dir.path().join("source")).unwrap();
    let id = age::x25519::Identity::generate();
    let signer = SigningKey::from_bytes(&[7; 32]);
    let objects = dir.path().join("objects");
    let manifest = backup(&source, &objects, &id.to_public(), &signer, |_| Ok(())).unwrap();
    assert_eq!(manifest.offset, None);
    let target = dir.path().join("restored");
    assert_eq!(
        restore(&objects, &target, &id, &signer.verifying_key())
            .unwrap()
            .records,
        0
    );
    assert!(restore(&objects, &target, &id, &signer.verifying_key()).is_err());
}

#[test]
fn export_is_one_snapshot_and_import_rejects_invalid_offsets_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let source = fixture::fixture(&dir.path().join("source"));
    let mut prefix = source.export_prefix().unwrap();
    let target = Store::open(dir.path().join("target")).unwrap();
    for tree in prefix.trees.values() {
        for (_, e) in tree.entries() {
            let bytes = source.read_blob(e.oid()).unwrap();
            target
                .ingest_blob(e.oid(), bytes.len() as u64, bytes.as_slice())
                .unwrap();
        }
    }
    prefix.records[1].offset = 9;
    assert!(target.import_prefix(&prefix).is_err());
    assert_eq!(target.len().unwrap(), 0);
    prefix.records[1].offset = 1;
    target.import_prefix(&prefix).unwrap();
    fixture::assert_same(&source, &target);
    assert!(target.import_prefix(&prefix).is_err());
}

#[test]
fn concurrent_writes_cannot_split_a_backup_read_snapshot() {
    use cuenca_core::LocationName;
    use cuenca_tree::{Entry, GitOid, RepoPath, Tree};
    let dir = tempfile::tempdir().unwrap();
    let source = std::sync::Arc::new(Store::open(dir.path().join("source")).unwrap());
    let writer = source.clone();
    let task = std::thread::spawn(move || {
        for i in 0..20 {
            let bytes = format!("blob-{i}");
            let oid = GitOid::for_blob(bytes.as_bytes());
            writer
                .ingest_blob(oid, bytes.len() as u64, bytes.as_bytes())
                .unwrap();
            let tree =
                Tree::from_entries([(RepoPath::new("data").unwrap(), Entry::File(oid))]).unwrap();
            writer
                .add_location(&LocationName::new(format!("loc-{i}")).unwrap(), None, &tree)
                .unwrap();
        }
    });
    for i in 0..5 {
        let prefix = source.export_prefix().unwrap();
        let target = Store::open(dir.path().join(format!("target-{i}"))).unwrap();
        for tree in prefix.trees.values() {
            for (_, e) in tree.entries() {
                let bytes = source.read_blob(e.oid()).unwrap();
                target
                    .ingest_blob(e.oid(), bytes.len() as u64, bytes.as_slice())
                    .unwrap();
            }
        }
        target.import_prefix(&prefix).unwrap();
        assert_eq!(target.records(0, usize::MAX).unwrap(), prefix.records);
        assert_eq!(
            target.replay_check().unwrap().records,
            prefix.records.len() as u64
        );
    }
    task.join().unwrap();
}
