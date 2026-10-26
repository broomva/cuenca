//! Offline local backup operator: redb refuses opening a running authority store.
use clap::{Parser, Subcommand};
use cuenca_server::restore;
use ed25519_dalek::{SigningKey, VerifyingKey};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[derive(Parser)]
#[command(about = "Signed age-encrypted journal prefixes; local fixture backend only")]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
enum Action {
    Backup {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        objects: PathBuf,
        #[arg(long)]
        recipient: PathBuf,
        #[arg(long)]
        signing_key: PathBuf,
    },
    Restore {
        #[arg(long)]
        objects: PathBuf,
        #[arg(long)]
        destination: PathBuf,
        #[arg(long)]
        identity: PathBuf,
        #[arg(long)]
        trust_key: PathBuf,
    },
    /// Generate TEST ONLY keys, never production custody.
    TestKeys {
        #[arg(long)]
        directory: PathBuf,
    },
}
fn key_bytes(path: &Path) -> anyhow::Result<[u8; 32]> {
    hex::decode(fs::read_to_string(path)?.trim())?
        .try_into()
        .map_err(|_| anyhow::anyhow!("key must have 32 bytes"))
}
fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Action::TestKeys { directory } => restore::test_keys(&directory)?,
        Action::Backup {
            source,
            objects,
            recipient,
            signing_key,
        } => {
            let recipient = fs::read_to_string(recipient)?
                .trim()
                .parse()
                .map_err(anyhow::Error::msg)?;
            let signer = SigningKey::from_bytes(&key_bytes(&signing_key)?);
            let store = cuenca_core::Store::open(source)?;
            let manifest = restore::backup(&store, &objects, &recipient, &signer, |phase| {
                if std::env::var("CUENCA_BACKUP_PAUSE_AT").is_ok_and(|p| p == phase) {
                    fs::write(std::env::var("CUENCA_BACKUP_MARKER")?, phase)?;
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
                Ok(())
            })?;
            println!(
                "backup records={} source_offset={:?} verified_blobs={}",
                manifest.records,
                manifest.offset,
                manifest.blobs.len()
            );
        }
        Action::Restore {
            objects,
            destination,
            identity,
            trust_key,
        } => {
            let identity = fs::read_to_string(identity)?
                .trim()
                .parse()
                .map_err(anyhow::Error::msg)?;
            let trust = VerifyingKey::from_bytes(&key_bytes(&trust_key)?)?;
            println!(
                "{}",
                serde_json::to_string(&restore::restore(
                    &objects,
                    &destination,
                    &identity,
                    &trust
                )?)?
            );
        }
    }
    Ok(())
}
