//! Real process kills against the directory fixture. TEST keys only.
#[path = "../tests/support/backup_fixture.rs"]
mod fixture;
use cuenca_core::{LocationName, Store};
use cuenca_server::materialise::Volume;
use cuenca_tree::Tree;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(bin: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new(bin).args(args).output()?;
    anyhow::ensure!(
        out.status.success(),
        "command {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8(out.stdout)?)
}
fn copy_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to.join(e.file_name()))?;
        } else {
            fs::copy(e.path(), to.join(e.file_name()))?;
        }
    }
    Ok(())
}
fn main() -> anyhow::Result<()> {
    let start = Instant::now();
    let mut args = std::env::args_os().skip(1);
    let bin = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("binary path required"))?,
    );
    let root = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow::anyhow!("SSD scratch path required"))?,
    );
    fs::create_dir(&root)?;
    let keys = root.join("TEST-keys");
    command(&bin, &["test-keys", "--directory", keys.to_str().unwrap()])?;
    let source_path = root.join("source");
    let source = fixture::fixture(&source_path);
    let before = source.records(0, usize::MAX)?;
    let old_offset = before.last().map(|r| r.offset);
    let count = source
        .export_prefix()?
        .trees
        .values()
        .flat_map(|t| t.entries().map(|(_, e)| e.oid()))
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    drop(source);
    let baseline = root.join("baseline");
    let recipient = keys.join("recipient");
    let signing = keys.join("signer");
    let identity = keys.join("identity");
    let trust = keys.join("trust");
    let backup_args = |objects: &Path| {
        vec![
            "backup".to_string(),
            "--source".into(),
            source_path.display().to_string(),
            "--objects".into(),
            objects.display().to_string(),
            "--recipient".into(),
            recipient.display().to_string(),
            "--signing-key".into(),
            signing.display().to_string(),
        ]
    };
    let run_backup = |objects: &Path| {
        let args = backup_args(objects);
        command(&bin, &args.iter().map(String::as_str).collect::<Vec<_>>())
    };
    print!("{}", run_backup(&baseline)?);
    let source = Store::open(&source_path)?;
    source.add_location(&LocationName::new("after-checkpoint")?, None, &Tree::new())?;
    let after = source.records(0, usize::MAX)?;
    let new_offset = after.last().map(|r| r.offset);
    let forks: Vec<_> = source.forks()?.iter().map(|f| f.name.to_string()).collect();
    drop(source);
    let mut phases = Vec::new();
    for i in 0..count {
        for when in ["before", "after"] {
            phases.push(format!("{when}:blob:{i}"));
        }
    }
    for what in ["journal", "manifest", "commit"] {
        for when in ["before", "after"] {
            phases.push(format!("{when}:{what}"));
        }
    }
    for (i, phase) in phases.iter().enumerate() {
        let case = root.join(format!("kill-{i:02}"));
        fs::create_dir(&case)?;
        let objects = case.join("objects");
        copy_tree(&baseline, &objects)?;
        let marker = case.join("paused");
        let mut child = Command::new(&bin)
            .args(backup_args(&objects))
            .env("CUENCA_BACKUP_PAUSE_AT", phase)
            .env("CUENCA_BACKUP_MARKER", &marker)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !marker.exists() {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("backup exited before {phase}: {status}");
            }
            if Instant::now() >= deadline {
                child.kill()?;
                child.wait()?;
                anyhow::bail!("timeout before {phase}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child.kill()?;
        let status = child.wait()?;
        anyhow::ensure!(!status.success(), "backup was not killed");
        let target = case.join("restored");
        let output = command(
            &bin,
            &[
                "restore",
                "--objects",
                objects.to_str().unwrap(),
                "--destination",
                target.to_str().unwrap(),
                "--identity",
                identity.to_str().unwrap(),
                "--trust-key",
                trust.to_str().unwrap(),
            ],
        )?;
        let receipt: serde_json::Value = serde_json::from_str(&output)?;
        let expected = if phase == "after:commit" {
            &after
        } else {
            &before
        };
        let restored = Store::open(&target)?;
        assert_eq!(&restored.records(0, usize::MAX)?, expected);
        assert_eq!(receipt["verified_blobs"], count);
        restored.replay_check()?;
        let reference = Store::open(&source_path)?;
        assert_eq!(reference.forks()?, restored.forks()?);
        if phase == "after:commit" {
            fixture::assert_same(&reference, &restored);
        }
        for tree in restored.export_prefix()?.trees.values() {
            for (_, entry) in tree.entries() {
                restored.read_blob(entry.oid())?;
            }
        }
        drop(reference);
        fixture::resume(&restored);
        // The real materialiser opens the restored owner's draft as writable files.
        let volume_root = case.join("volume");
        let mut volume = Volume::open(&volume_root, restored.object_dir())?;
        let pass = volume.converge(&restored)?;
        anyhow::ensure!(
            pass.failed.is_empty(),
            "materialisation failed: {:?}",
            pass.failed
        );
        let owner_file = volume_root.join("forks/owner-recovery/receipt.txt");
        assert_eq!(fs::read(&owner_file)?, b"owner draft\n");
        fs::write(&owner_file, b"owner can resume editing\n")?;
        println!(
            "kill={phase} source_offset={new_offset:?} restored_offset={:?} verified_blobs={count} dangling=0 owner_open=ok intent_resume=once",
            expected.last().map(|r| r.offset)
        );
    }
    // Forge a committed point after a complete backup: never silently fall back.
    let forged = root.join("forged");
    copy_tree(&baseline, &forged)?;
    fs::write(
        forged
            .join("manifests")
            .join(format!("{:020}-{}.json", after.len() + 1, "0".repeat(64))),
        b"{\"format_version\":1,\"manifest\":{\"sha256\":\"forged\",\"size\":0}}",
    )?;
    let status = Command::new(&bin)
        .args([
            "restore",
            "--objects",
            forged.to_str().unwrap(),
            "--destination",
            root.join("forged-restore").to_str().unwrap(),
            "--identity",
            identity.to_str().unwrap(),
            "--trust-key",
            trust.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    anyhow::ensure!(
        !status.success() && !root.join("forged-restore").exists(),
        "forged manifest accepted"
    );
    println!(
        "forged_manifest=refused (encrypted authority-signature forgery also covered by unit test)"
    );
    println!(
        "DOGFOOD source_offset={new_offset:?} previous_offset={old_offset:?} restored_offset={new_offset:?} verified_blobs={count} old_fork_ids={} new_fork_ids={} kill_boundaries={} wall_ms={}",
        forks.join(","),
        forks.join(","),
        phases.len(),
        start.elapsed().as_millis()
    );
    println!(
        "node stays incomplete: R2 restore pending the backup-custody owner grant; RPO timer waits on BRO-3461"
    );
    Ok(())
}
