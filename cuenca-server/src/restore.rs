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
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
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
    let name = format!("{:020}-{}.json", manifest.records, encrypted.sha256);
    let envelope = serde_json::to_vec(&Envelope {
        format_version: FORMAT_VERSION,
        manifest: encrypted,
    })?;
    transition("before:commit")?;
    durable_write(&root.join("manifests"), &name, &envelope)?;
    transition("after:commit")?;
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
    let mut commits = Vec::new();
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
            name.len() == 90
                && name.ends_with(".json")
                && name.as_bytes()[20] == b'-'
                && name[..20].bytes().all(|b| b.is_ascii_digit())
                && hex_digest(&name[21..85]),
            "invalid committed manifest name"
        );
        commits.push(name);
    }
    commits.sort();
    let name = commits
        .last()
        .ok_or_else(|| anyhow::anyhow!("no complete restore point"))?;
    let envelope: Envelope = serde_json::from_slice(&read_bounded(
        &root.join("manifests").join(name),
        MAX_MANIFEST,
    )?)?;
    anyhow::ensure!(
        envelope.format_version == FORMAT_VERSION,
        "unknown envelope format"
    );
    anyhow::ensure!(
        envelope.manifest.sha256 == name[21..85],
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
    anyhow::ensure!(
        format!("{:020}", manifest.records) == name[..20],
        "commit offset mismatch"
    );
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
