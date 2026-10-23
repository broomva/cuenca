//! Kill the writer mid-batch, again and again: a blob batch is stored whole or not at all.
//!
//! `kill_during_batch_ingest` re-runs this test binary as a child that only runs
//! `child_ingester`, which ingests batch after batch of [`PER_BATCH`] blobs, batch `k`
//! being blobs `(k, 0..PER_BATCH)`, and prints an ack with `k` after each commit
//! returns. After a restart it carries on from the first batch the store does not hold.
//! The parent SIGKILLs it at a random moment, reopens the store and checks: every batch
//! is held with all of its blobs or none, the held batches are a prefix of the
//! sequence, every acknowledged batch is held, and every held blob reads back as its
//! bytes.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use cuenca_core::Store;
use cuenca_tree::GitOid;

const DIR_ENV: &str = "CUENCA_BATCH_CRASH_TEST_DIR";
const KILLS: usize = 30;
const PER_BATCH: usize = 16;
const ACKED: &str = "cuenca-batch-acked ";

fn content(batch: u64, i: usize) -> Vec<u8> {
    format!("batch {batch} blob {i}\n").repeat(256).into_bytes()
}

/// How many of batch `k`'s blobs the store holds.
fn held(store: &Store, k: u64) -> usize {
    (0..PER_BATCH)
        .filter(|&i| {
            let oid = GitOid::for_blob(&content(k, i));
            store.blob_info(oid).unwrap().is_some()
        })
        .count()
}

/// The child: ingest batches forever. Not a test on its own.
#[test]
#[ignore = "run by kill_during_batch_ingest as a child process"]
fn child_ingester() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let store = Store::open(&dir).unwrap();
    let mut k = 0;
    while held(&store, k) == PER_BATCH {
        k += 1;
    }
    loop {
        let mut batch = store.blob_batch();
        for i in 0..PER_BATCH {
            let bytes = content(k, i);
            batch
                .add(
                    GitOid::for_blob(&bytes),
                    bytes.len() as u64,
                    bytes.as_slice(),
                )
                .unwrap();
        }
        batch.commit().unwrap();
        // libtest prints "test child_ingester ... " without a newline first.
        println!("{ACKED}{k}");
        k += 1;
    }
}

#[test]
fn kill_during_batch_ingest() {
    let dir = tempfile::tempdir().unwrap();
    drop(Store::open(dir.path()).unwrap());
    let exe = std::env::current_exe().unwrap();
    // A small deterministic generator, so a failure reproduces.
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut acked_total = 0usize;
    let mut torn_seen = 0usize;

    for round in 0..KILLS {
        let mut child = Command::new(&exe)
            .args(["--exact", "child_ingester", "--ignored", "--nocapture"])
            .arg("--test-threads=1")
            .env(DIR_ENV, dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // Wait for this run's first ack, so the kill lands while batches are being
        // written, then let it run for 0-30 ms more.
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut early = Vec::new();
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
                Ok(line) if line.contains(ACKED) => {
                    early.push(line);
                    break;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = child.kill();
                    let out = child.wait_with_output().unwrap();
                    panic!(
                        "round {round}: no batch acknowledged within 60 s: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
            }
        }
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_micros(seed % 30_000));
        child.kill().unwrap();
        let status = child.wait().unwrap();
        assert!(!status.success(), "round {round}: the child was not killed");
        reader.join().unwrap();
        let acked: Vec<u64> = early
            .into_iter()
            .chain(rx.try_iter())
            .filter_map(|l| {
                l.find(ACKED)
                    .map(|at| l[at + ACKED.len()..].trim().parse().unwrap())
            })
            .collect();
        acked_total += acked.len();

        let store = Store::open(dir.path()).unwrap();
        let mut k = 0;
        while held(&store, k) == PER_BATCH {
            for i in 0..PER_BATCH {
                let bytes = content(k, i);
                assert_eq!(
                    store.read_blob(GitOid::for_blob(&bytes)).unwrap(),
                    bytes,
                    "round {round}: batch {k} blob {i}"
                );
            }
            k += 1;
        }
        // Past the held prefix, nothing at all: not part of the interrupted batch, and
        // not a later one. Files the killed batch put in place are not indexed.
        for later in k..k + 3 {
            assert_eq!(
                held(&store, later),
                0,
                "round {round}: batch {later} is torn"
            );
        }
        let blobs = dir.path().join("blobs/objects");
        let on_disk: usize = std::fs::read_dir(&blobs)
            .unwrap()
            .map(|d| std::fs::read_dir(d.unwrap().path()).unwrap().count())
            .sum();
        torn_seen += on_disk.saturating_sub(k as usize * PER_BATCH);
        for a in &acked {
            assert!(*a < k, "round {round}: acknowledged batch {a} is not held");
        }
    }
    eprintln!(
        "{KILLS} kills; {acked_total} batches acknowledged in all; \
         {torn_seen} unindexed blob files seen after kills (never read)"
    );
}
