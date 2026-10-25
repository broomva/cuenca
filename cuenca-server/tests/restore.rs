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
fn import_rejects_invalid_offsets_atomically() {
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
fn relative_single_component_destination_succeeds_and_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let keys = dir.path().join("keys");
    cuenca_server::restore::test_keys(&keys).unwrap();
    let source = Store::open(dir.path().join("source")).unwrap();
    let recipient = std::fs::read_to_string(keys.join("recipient"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let seed: [u8; 32] = hex::decode(std::fs::read_to_string(keys.join("signer")).unwrap().trim())
        .unwrap()
        .try_into()
        .unwrap();
    let objects = dir.path().join("objects");
    backup(
        &source,
        &objects,
        &recipient,
        &SigningKey::from_bytes(&seed),
        |_| Ok(()),
    )
    .unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_cuenca-backup"))
            .current_dir(dir.path())
            .args(["restore", "--destination", "new-store", "--objects"])
            .arg(&objects)
            .arg("--identity")
            .arg(keys.join("identity"))
            .arg("--trust-key")
            .arg(keys.join("trust"))
            .output()
            .unwrap()
    };
    let out = run();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let target = dir.path().join("new-store");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(Store::open(&target).unwrap().len().unwrap(), 0);
    let again = run();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
}
