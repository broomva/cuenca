//! Local object-store implementation of the accepted backup-prefix ADR (BRO-3258).
//!
//! Only an atomically published completion envelope is a restore point. Objects
//! and the signed manifest are age-encrypted; the restore trust key is supplied
//! independently. An invalid newest committed point fails closed, never falls back.
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use age::secrecy::ExposeSecret;
use cuenca_core::{JournalPrefix, Store};
use cuenca_tree::{Blob, GitOid};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const FORMAT_VERSION: u32 = 1;
const MAX_OBJECT: u64 = 256 * 1024 * 1024;
const MAX_MANIFEST: u64 = 8 * 1024 * 1024;
const MAX_JOURNAL: u64 = 64 * 1024 * 1024;
const DOMAIN: &[u8] = b"cuenca-backup-manifest-v1\0";

type Result<T> = anyhow::Result<T>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Object {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobEntry {
    pub oid: GitOid,
    pub sha256: String,
    pub size: u64,
    pub object: Object,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    /// Last zero-based journal offset. None is an empty log, not offset zero.
    pub offset: Option<u64>,
    pub records: u64,
    pub journal_sha256: String,
    pub journal_size: u64,
    pub journal: Object,
    pub blobs: Vec<BlobEntry>,
    /// SHA-256 of the Ed25519 public key; the public key is NOT trusted from here.
    pub signing_key_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Signed {
    manifest: Manifest,
    signature: String,
}

/// The only public metadata: version, ciphertext digest and ciphertext size.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    format_version: u32,
    manifest: Object,
}

#[derive(Debug, Serialize)]
pub struct Receipt {
    pub source_offset: Option<u64>,
    pub restored_offset: Option<u64>,
    pub records: u64,
    pub verified_blobs: usize,
    pub fork_ids: Vec<String>,
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn signed_bytes(manifest: &Manifest) -> Result<Vec<u8>> {
    let mut bytes = DOMAIN.to_vec();
    bytes.extend(serde_json::to_vec(manifest)?);
    Ok(bytes)
}

fn encrypt(recipient: &age::x25519::Recipient, bytes: &[u8]) -> Result<Vec<u8>> {
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(recipient as &dyn age::Recipient))?;
    let mut encrypted = Vec::new();
    let mut writer = encryptor.wrap_output(&mut encrypted)?;
    writer.write_all(bytes)?;
    writer.finish()?;
    Ok(encrypted)
}

fn decrypt(identity: &age::x25519::Identity, bytes: &[u8], limit: u64) -> Result<Vec<u8>> {
    let decryptor = age::Decryptor::new(bytes)?;
    let reader = decryptor.decrypt(std::iter::once(identity as &dyn age::Identity))?;
    let mut plain = Vec::new();
    reader.take(limit + 1).read_to_end(&mut plain)?;
    anyhow::ensure!(
        plain.len() as u64 <= limit,
        "decrypted object exceeds limit"
    );
    Ok(plain)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    anyhow::ensure!(file.metadata()?.is_file(), "object is not a regular file");
    anyhow::ensure!(file.metadata()?.len() <= limit, "object exceeds limit");
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(bytes.len() as u64 <= limit, "object exceeds limit");
    Ok(bytes)
}

fn hex_digest(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn object_bytes(root: &Path, object: &Object) -> Result<Vec<u8>> {
    anyhow::ensure!(
        hex_digest(&object.sha256) && object.size <= MAX_OBJECT,
        "invalid ciphertext reference"
    );
    let bytes = read_bounded(
        &root.join("objects").join(format!("{}.age", object.sha256)),
        MAX_OBJECT,
    )?;
    anyhow::ensure!(
        bytes.len() as u64 == object.size,
        "ciphertext size mismatch"
    );
    anyhow::ensure!(digest(&bytes) == object.sha256, "ciphertext hash mismatch");
    Ok(bytes)
}

fn durable_write(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let mut pending = tempfile::NamedTempFile::new_in(dir)?;
    pending.write_all(bytes)?;
    pending.as_file().sync_all()?;
    pending
        .persist_noclobber(dir.join(name))
        .map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn upload(
    root: &Path,
    bytes: Vec<u8>,
    label: &str,
    transition: &mut impl FnMut(&str) -> Result<()>,
) -> Result<Object> {
    anyhow::ensure!(bytes.len() as u64 <= MAX_OBJECT, "ciphertext exceeds limit");
    let object = Object {
        sha256: digest(&bytes),
        size: bytes.len() as u64,
    };
    transition(&format!("before:{label}"))?;
    let dir = root.join("objects");
    durable_write(&dir, &format!("{}.age", object.sha256), &bytes)?;
    // Verify the durable object, rather than trusting upload success.
    object_bytes(root, &object)?;
    transition(&format!("after:{label}"))?;
    Ok(object)
}

/// Export from ONE read snapshot; verify every reachable historical blob against
/// both its git oid and stored independent SHA-256/size before encrypting it.
/// The caller controls cadence; this local backend never creates cloud resources.
pub fn backup(
    store: &Store,
    root: &Path,
    recipient: &age::x25519::Recipient,
    signer: &SigningKey,
    mut transition: impl FnMut(&str) -> Result<()>,
) -> Result<Manifest> {
    fs::create_dir_all(root.join("objects"))?;
    fs::create_dir_all(root.join("manifests"))?;
    // Serialize backups to prevent competing commit ordering within this fixture.
    let lock = File::open(root)?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    let prefix = store.export_prefix()?;
    let journal = serde_json::to_vec(&prefix)?;
    anyhow::ensure!(journal.len() as u64 <= MAX_JOURNAL, "journal exceeds limit");
    let reachable: BTreeSet<_> = prefix
        .trees
        .values()
        .flat_map(|t| t.entries().map(|(_, e)| e.oid()))
        .collect();
    let mut blobs = Vec::new();
    for (index, oid) in reachable.into_iter().enumerate() {
        let plain = store.read_blob(oid)?;
        let verified = Blob::of(&plain).ok_or_else(|| anyhow::anyhow!("SHA-1 collision"))?;
        let info = store
            .blob_info(oid)?
            .ok_or_else(|| anyhow::anyhow!("missing blob index"))?;
        anyhow::ensure!(
            verified.oid() == oid
                && verified.sha256() == info.sha256
                && verified.size() == info.size,
            "source blob integrity mismatch"
        );
        let object = upload(
            root,
            encrypt(recipient, &plain)?,
            &format!("blob:{index}"),
            &mut transition,
        )?;
        blobs.push(BlobEntry {
            oid,
            sha256: digest(&plain),
            size: plain.len() as u64,
            object,
        });
    }
    let journal_object = upload(
        root,
        encrypt(recipient, &journal)?,
        "journal",
        &mut transition,
    )?;
    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        offset: prefix.records.last().map(|r| r.offset),
        records: prefix.records.len() as u64,
        journal_sha256: digest(&journal),
        journal_size: journal.len() as u64,
        journal: journal_object,
        blobs,
        signing_key_id: digest(signer.verifying_key().as_bytes()),
    };
    let signature = hex::encode(signer.sign(&signed_bytes(&manifest)?).to_bytes());
    let signed = serde_json::to_vec(&Signed {
        manifest: manifest.clone(),
        signature,
    })?;
    anyhow::ensure!(
        signed.len() as u64 <= MAX_MANIFEST,
        "manifest exceeds limit"
    );
    let encrypted = upload(
        root,
        encrypt(recipient, &signed)?,
        "manifest",
        &mut transition,
    )?;
    let name = format!("{}.json", encrypted.sha256);
    let envelope = serde_json::to_vec(&Envelope {
        format_version: FORMAT_VERSION,
        manifest: encrypted,
    })?;
    transition("before:commit")?;
    durable_write(&root.join("manifests"), &name, &envelope)?;
    transition("after:commit")?;
    Ok(manifest)
}

fn read_manifest(
    root: &Path,
    name: &str,
    identity: &age::x25519::Identity,
    trust: &VerifyingKey,
) -> Result<Manifest> {
    let envelope: Envelope = serde_json::from_slice(&read_bounded(
        &root.join("manifests").join(name),
        MAX_MANIFEST,
    )?)?;
    anyhow::ensure!(
        envelope.format_version == FORMAT_VERSION,
        "unknown envelope format"
    );
    anyhow::ensure!(
        envelope.manifest.sha256 == name[..64],
        "envelope name mismatch"
    );
    let signed: Signed = serde_json::from_slice(&decrypt(
        identity,
        &object_bytes(root, &envelope.manifest)?,
        MAX_MANIFEST,
    )?)?;
    let manifest = signed.manifest;
    let signature = Signature::from_slice(&hex::decode(&signed.signature)?)?;
    anyhow::ensure!(
        trust
            .verify_strict(&signed_bytes(&manifest)?, &signature)
            .is_ok(),
        "invalid authority signature"
    );
    anyhow::ensure!(
        manifest.signing_key_id == digest(trust.as_bytes()),
        "wrong signing key id"
    );
    anyhow::ensure!(
        manifest.format_version == FORMAT_VERSION,
        "unknown manifest format"
    );
    anyhow::ensure!(
        manifest.offset == manifest.records.checked_sub(1),
        "manifest offset mismatch"
    );
    Ok(manifest)
}

/// Restore the newest committed point into a NEW destination. Validation occurs
/// in private staging; refusal never creates or modifies the destination.
/// Pin `trust` outside the object store. A substituted key in a manifest is useless.
pub fn restore(
    root: &Path,
    destination: &Path,
    identity: &age::x25519::Identity,
    trust: &VerifyingKey,
) -> Result<Receipt> {
    anyhow::ensure!(
        !destination.try_exists()?,
        "restore destination already exists"
    );
    let mut latest: Option<Manifest> = None;
    for entry in fs::read_dir(root.join("manifests"))? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("non-UTF8 commit name"))?;
        if name.starts_with('.') {
            continue;
        } // uncommitted temporary writes
        anyhow::ensure!(
            name.is_ascii()
                && name.len() == 69
                && name.ends_with(".json")
                && hex_digest(&name[..64]),
            "invalid committed manifest name"
        );
        let manifest = read_manifest(root, &name, identity, trust)?;
        if let Some(previous) = &latest {
            anyhow::ensure!(
                manifest.records != previous.records
                    || manifest.journal_sha256 == previous.journal_sha256,
                "conflicting signed prefixes at one offset"
            );
        }
        if latest.as_ref().is_none_or(|m| manifest.records > m.records) {
            latest = Some(manifest);
        }
    }
    let manifest = latest.ok_or_else(|| anyhow::anyhow!("no complete restore point"))?;
    let journal = decrypt(
        identity,
        &object_bytes(root, &manifest.journal)?,
        MAX_JOURNAL,
    )?;
    anyhow::ensure!(
        journal.len() as u64 == manifest.journal_size
            && digest(&journal) == manifest.journal_sha256,
        "journal integrity mismatch"
    );
    let prefix: JournalPrefix = serde_json::from_slice(&journal)?;
    anyhow::ensure!(
        prefix.records.len() as u64 == manifest.records
            && prefix
                .records
                .iter()
                .enumerate()
                .all(|(i, r)| r.offset == i as u64),
        "journal offset mismatch"
    );
    let reachable: BTreeSet<_> = prefix
        .trees
        .values()
        .flat_map(|t| t.entries().map(|(_, e)| e.oid()))
        .collect();
    let listed: BTreeSet<_> = manifest.blobs.iter().map(|b| b.oid).collect();
    anyhow::ensure!(
        reachable == listed && listed.len() == manifest.blobs.len(),
        "blob reachability mismatch"
    );
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow::anyhow!("destination needs a parent"))?;
    let staged = tempfile::Builder::new()
        .prefix(".cuenca-restore-")
        .tempdir_in(parent)?;
    let store = Store::open(staged.path())?;
    for blob in &manifest.blobs {
        let plain = decrypt(identity, &object_bytes(root, &blob.object)?, MAX_OBJECT)?;
        let verified = Blob::of(&plain).ok_or_else(|| anyhow::anyhow!("SHA-1 collision"))?;
        anyhow::ensure!(
            digest(&plain) == blob.sha256,
            "plaintext blob hash mismatch"
        );
        anyhow::ensure!(
            plain.len() as u64 == blob.size,
            "plaintext blob size mismatch"
        );
        anyhow::ensure!(verified.oid() == blob.oid, "plaintext blob oid mismatch");
        store.ingest_blob(blob.oid, blob.size, plain.as_slice())?;
    }
    store.import_prefix(&prefix)?;
    store.replay_check()?;
    let receipt = Receipt {
        source_offset: manifest.offset,
        restored_offset: store.records(0, usize::MAX)?.last().map(|r| r.offset),
        records: store.len()?,
        verified_blobs: listed.len(),
        fork_ids: store.forks()?.iter().map(|f| f.name.to_string()).collect(),
    };
    drop(store);
    // No replace: concurrent restores must never overwrite an existing state.
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        staged.path(),
        rustix::fs::CWD,
        destination,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    File::open(parent)?.sync_all()?;
    Ok(receipt)
}

/// Generate throwaway drill keys. Never called by a routine backup.
pub fn test_keys(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let identity = age::x25519::Identity::generate();
    let mut seed = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut seed);
    let signer = SigningKey::from_bytes(&seed);
    for (name, value) in [
        ("identity", identity.to_string().expose_secret().to_string()),
        ("recipient", identity.to_public().to_string()),
        ("signer", hex::encode(seed)),
        ("trust", hex::encode(signer.verifying_key().as_bytes())),
    ] {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(name))?;
        writeln!(file, "{value}")?;
        file.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cuenca_core::LocationName;
    use cuenca_tree::{Entry, RepoPath, Tree};
    use redb::ReadableTable;

    struct Fixture {
        dir: tempfile::TempDir,
        objects: std::path::PathBuf,
        identity: age::x25519::Identity,
        signer: SigningKey,
        current: std::cell::RefCell<std::path::PathBuf>,
    }
    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let objects = dir.path().join("objects");
            let identity = age::x25519::Identity::generate();
            let signer = SigningKey::from_bytes(&[3; 32]);
            let source = Store::open(dir.path().join("source")).unwrap();
            backup(
                &source,
                &objects,
                &identity.to_public(),
                &signer,
                |_| Ok(()),
            )
            .unwrap();
            let bytes = b"secret test content";
            let oid = GitOid::for_blob(bytes);
            source
                .ingest_blob(oid, bytes.len() as u64, bytes.as_slice())
                .unwrap();
            let tree =
                Tree::from_entries([(RepoPath::new("private-name").unwrap(), Entry::File(oid))])
                    .unwrap();
            source
                .add_location(&LocationName::new("fixture").unwrap(), None, &tree)
                .unwrap();
            backup(
                &source,
                &objects,
                &identity.to_public(),
                &signer,
                |_| Ok(()),
            )
            .unwrap();
            let current = fs::read_dir(objects.join("manifests"))
                .unwrap()
                .map(|p| p.unwrap().path())
                .max_by_key(|p| {
                    read_manifest(
                        &objects,
                        p.file_name().unwrap().to_str().unwrap(),
                        &identity,
                        &signer.verifying_key(),
                    )
                    .unwrap()
                    .records
                })
                .unwrap();
            Self {
                dir,
                objects,
                identity,
                signer,
                current: std::cell::RefCell::new(current),
            }
        }
        fn newest(&self) -> std::path::PathBuf {
            self.current.borrow().clone()
        }
        fn signed(&self) -> Signed {
            let envelope: Envelope =
                serde_json::from_slice(&fs::read(self.newest()).unwrap()).unwrap();
            serde_json::from_slice(
                &decrypt(
                    &self.identity,
                    &object_bytes(&self.objects, &envelope.manifest).unwrap(),
                    MAX_MANIFEST,
                )
                .unwrap(),
            )
            .unwrap()
        }
        fn publish(&self, signed: &Signed) {
            let old = self.newest();
            let cipher = encrypt(
                &self.identity.to_public(),
                &serde_json::to_vec(signed).unwrap(),
            )
            .unwrap();
            let object = upload(&self.objects, cipher, "test", &mut |_| Ok(())).unwrap();
            let name = format!("{}.json", object.sha256);
            let envelope = Envelope {
                format_version: FORMAT_VERSION,
                manifest: object,
            };
            fs::remove_file(old).unwrap();
            durable_write(
                &self.objects.join("manifests"),
                &name,
                &serde_json::to_vec(&envelope).unwrap(),
            )
            .unwrap();
            *self.current.borrow_mut() = self.objects.join("manifests").join(name);
        }
        fn edit(&self, edit: impl FnOnce(&mut Manifest)) {
            let mut signed = self.signed();
            edit(&mut signed.manifest);
            signed.signature = hex::encode(
                self.signer
                    .sign(&signed_bytes(&signed.manifest).unwrap())
                    .to_bytes(),
            );
            self.publish(&signed);
        }
        fn refuses(&self, reason: &str) {
            let target = self.dir.path().join("restored");
            let error = restore(
                &self.objects,
                &target,
                &self.identity,
                &self.signer.verifying_key(),
            )
            .unwrap_err();
            assert!(
                error.to_string().contains(reason),
                "expected {reason}, got {error:#}"
            );
            assert!(!target.exists());
        }
        fn path(&self, object: &Object) -> std::path::PathBuf {
            self.objects
                .join("objects")
                .join(format!("{}.age", object.sha256))
        }
    }
    #[test]
    fn local_objects_refuse_symlinks_and_fifos_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        fs::write(&plain, b"test-only bytes").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&plain, &link).unwrap();
        assert!(read_bounded(&link, 100).is_err());
        let fifo = dir.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(read_bounded(&fifo, 100).is_err());
    }
    #[test]
    fn forged_manifest_is_refused_without_falling_back() {
        let f = Fixture::new();
        let mut signed = f.signed();
        signed.signature = hex::encode([0u8; 64]);
        f.publish(&signed);
        f.refuses("invalid authority signature");
    }
    #[test]
    fn independently_pinned_trust_refuses_an_attacker_key() {
        let f = Fixture::new();
        let mut signed = f.signed();
        let attacker = SigningKey::from_bytes(&[9; 32]);
        signed.manifest.signing_key_id = digest(attacker.verifying_key().as_bytes());
        signed.signature = hex::encode(
            attacker
                .sign(&signed_bytes(&signed.manifest).unwrap())
                .to_bytes(),
        );
        f.publish(&signed);
        f.refuses("invalid authority signature");
    }
    #[test]
    fn signed_offset_and_journal_offset_guards_refuse() {
        let f = Fixture::new();
        f.edit(|m| m.offset = Some(9));
        f.refuses("manifest offset mismatch");
        let f = Fixture::new();
        f.edit(|m| {
            m.records += 1;
            m.offset = m.records.checked_sub(1);
        });
        f.refuses("journal offset mismatch");
        let f = Fixture::new();
        let mut signed = f.signed();
        let journal = decrypt(
            &f.identity,
            &object_bytes(&f.objects, &signed.manifest.journal).unwrap(),
            MAX_JOURNAL,
        )
        .unwrap();
        let mut prefix: JournalPrefix = serde_json::from_slice(&journal).unwrap();
        prefix.records[0].offset = 9;
        let journal = serde_json::to_vec(&prefix).unwrap();
        signed.manifest.journal = upload(
            &f.objects,
            encrypt(&f.identity.to_public(), &journal).unwrap(),
            "test",
            &mut |_| Ok(()),
        )
        .unwrap();
        signed.manifest.journal_sha256 = digest(&journal);
        signed.manifest.journal_size = journal.len() as u64;
        signed.signature = hex::encode(
            f.signer
                .sign(&signed_bytes(&signed.manifest).unwrap())
                .to_bytes(),
        );
        f.publish(&signed);
        f.refuses("journal offset mismatch");
    }
    #[test]
    fn signed_plaintext_hash_size_and_oid_guards_refuse() {
        let f = Fixture::new();
        f.edit(|m| m.blobs[0].sha256 = "00".repeat(32));
        f.refuses("plaintext blob hash mismatch");
        let f = Fixture::new();
        f.edit(|m| m.blobs[0].size += 1);
        f.refuses("plaintext blob size mismatch");
        let f = Fixture::new();
        // Different valid ciphertext with matching SHA-256 and size, but wrong git oid.
        let replacement = b"different content";
        let object = upload(
            &f.objects,
            encrypt(&f.identity.to_public(), replacement).unwrap(),
            "test",
            &mut |_| Ok(()),
        )
        .unwrap();
        f.edit(|m| {
            m.blobs[0].object = object;
            m.blobs[0].sha256 = digest(replacement);
            m.blobs[0].size = replacement.len() as u64;
        });
        f.refuses("plaintext blob oid mismatch");
    }
    #[test]
    fn missing_truncated_and_hash_corrupted_objects_refuse() {
        let f = Fixture::new();
        let blob = f.signed().manifest.blobs.remove(0);
        fs::remove_file(f.path(&blob.object)).unwrap();
        assert!(
            restore(
                &f.objects,
                &f.dir.path().join("restore"),
                &f.identity,
                &f.signer.verifying_key()
            )
            .is_err()
        );
        let f = Fixture::new();
        let blob = f.signed().manifest.blobs.remove(0);
        let path = f.path(&blob.object);
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::write(path, bytes).unwrap();
        f.refuses("ciphertext size mismatch");
        let f = Fixture::new();
        let blob = f.signed().manifest.blobs.remove(0);
        let path = f.path(&blob.object);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 1;
        fs::write(path, bytes).unwrap();
        f.refuses("ciphertext hash mismatch");
    }
    #[test]
    fn incomplete_and_duplicate_blob_lists_refuse() {
        let f = Fixture::new();
        f.edit(|m| m.blobs.clear());
        f.refuses("blob reachability mismatch");
        let f = Fixture::new();
        f.edit(|m| m.blobs.push(m.blobs[0].clone()));
        f.refuses("blob reachability mismatch");
    }
    #[test]
    fn journal_digest_size_and_unknown_formats_refuse() {
        let f = Fixture::new();
        f.edit(|m| m.journal_sha256 = "00".repeat(32));
        f.refuses("journal integrity mismatch");
        let f = Fixture::new();
        f.edit(|m| m.journal_size += 1);
        f.refuses("journal integrity mismatch");
        let f = Fixture::new();
        f.edit(|m| m.format_version += 1);
        f.refuses("unknown manifest format");
        let f = Fixture::new();
        f.edit(|m| m.signing_key_id = "00".repeat(32));
        f.refuses("wrong signing key id");
    }
    #[test]
    fn envelope_tampering_and_truncation_refuse() {
        let f = Fixture::new();
        let mut e: Envelope = serde_json::from_slice(&fs::read(f.newest()).unwrap()).unwrap();
        e.format_version += 1;
        fs::write(f.newest(), serde_json::to_vec(&e).unwrap()).unwrap();
        f.refuses("unknown envelope format");
        let f = Fixture::new();
        fs::write(f.newest(), b"{").unwrap();
        assert!(
            restore(
                &f.objects,
                &f.dir.path().join("restore"),
                &f.identity,
                &f.signer.verifying_key()
            )
            .is_err()
        );
    }
    #[test]
    fn wrong_decryption_key_refuses_and_public_envelope_hides_metadata() {
        let f = Fixture::new();
        assert!(
            restore(
                &f.objects,
                &f.dir.path().join("restore"),
                &age::x25519::Identity::generate(),
                &f.signer.verifying_key()
            )
            .is_err()
        );
        let public = fs::read_to_string(f.newest()).unwrap();
        for secret in [
            "private-name",
            "fixture",
            "secret test content",
            "journal_sha256",
            "signing_key_id",
        ] {
            assert!(!public.contains(secret));
        }
    }
    #[test]
    fn backup_independently_verifies_source_sha256_and_size() {
        for index in [0, 32] {
            let f = Fixture::new();
            let oid = f.signed().manifest.blobs[0].oid;
            let db = redb::Database::open(f.dir.path().join("source/log.redb")).unwrap();
            let txn = db.begin_write().unwrap();
            {
                let mut blobs = txn
                    .open_table(redb::TableDefinition::<&[u8], &[u8]>::new("blobs"))
                    .unwrap();
                let mut info = blobs
                    .get(oid.as_bytes().as_slice())
                    .unwrap()
                    .unwrap()
                    .value()
                    .to_vec();
                info[index] ^= 1;
                blobs
                    .insert(oid.as_bytes().as_slice(), info.as_slice())
                    .unwrap();
            }
            txn.commit().unwrap();
            drop(db);
            let source = Store::open(f.dir.path().join("source")).unwrap();
            let output = f.dir.path().join("bad");
            let error = backup(&source, &output, &f.identity.to_public(), &f.signer, |_| {
                Ok(())
            })
            .unwrap_err();
            assert!(error.to_string().contains("source blob integrity mismatch"));
            assert_eq!(fs::read_dir(output.join("manifests")).unwrap().count(), 0);
        }
    }
    #[test]
    fn backup_refuses_corrupt_source_and_incomplete_upload_never_commits() {
        let f = Fixture::new();
        let source = Store::open(f.dir.path().join("source")).unwrap();
        let oid = f.signed().manifest.blobs[0].oid.to_hex();
        fs::write(
            source.object_dir().join(&oid[..2]).join(&oid[2..]),
            b"corrupt",
        )
        .unwrap();
        let out = f.dir.path().join("bad");
        assert!(
            backup(
                &source,
                &out,
                &f.identity.to_public(),
                &f.signer,
                |_| Ok(())
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(out.join("manifests")).unwrap().count(), 0);
    }
}
