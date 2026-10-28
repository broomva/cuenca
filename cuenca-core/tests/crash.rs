//! Kill the writer mid-append, again and again, and check the log after each kill.
//!
//! `kill_during_append` re-runs this test binary as a child that only runs
//! `child_writer`, which in a tight loop records a landing intent for the fork's head,
//! moves main to it, and publishes the next draft, printing an ack with the draft's id
//! after each publish returns. After a restart the intent and the move are retries.
//! The parent SIGKILLs it at a random moment, reopens the store and checks: the log
//! replays cleanly (no torn, missing or duplicated record of any kind), every
//! acknowledged draft is in it, the drafts are exactly the chain the fork's head counter
//! says, and intents and moves of main trail the drafts by at most the interrupted step.
//!
//! `kill_during_fenced_append` does the same for the fenced path: its child takes the
//! fork over once (a retry after a restart sees the new epoch and skips it), then keeps
//! publishing as the fenced-out epoch-1 writer, so every draft is a `DraftFenced` that
//! the first one creates the sibling with. After each kill the sibling's drafts must be
//! exactly its head counter's chain, every acknowledged one in the log, and the fork
//! itself untouched.

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::*;
use cuenca_core::{ChangesetId, Event, Store, sibling_name};
use cuenca_tree::Tree;

const DIR_ENV: &str = "CUENCA_CRASH_TEST_DIR";
const KILLS: usize = 100;
const ACKED: &str = "cuenca-crash-acked ";

/// Who takes the fork over in the fenced run.
fn taker() -> cuenca_core::Holder {
    holder("taker", "nTakerCNTRL")
}

/// The sibling the fenced-out [`w1`] writes to.
fn sibling() -> cuenca_core::ForkName {
    sibling_name(&fork("f"), 1, &agent().node)
}

fn counter(store: &Store, head: &Tree) -> u64 {
    let oid = head.get("counter").expect("counter file").oid();
    String::from_utf8(store.read_blob(oid).unwrap())
        .unwrap()
        .parse()
        .unwrap()
}

/// The child: publish drafts forever. Not a test on its own.
#[test]
#[ignore = "run by kill_during_append as a child process"]
fn child_writer() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let store = Store::open(&dir).unwrap();
    // libtest prints "test child_writer ... " without a newline first, so the parent
    // finds acks within a line rather than matching whole lines.
    loop {
        // Land the head, every time: after a restart these are retries of what the
        // killed child may already have recorded, and must append nothing.
        let state = store.fork(&fork("f")).unwrap().unwrap();
        if let Some((head, _)) = state.head_changeset {
            // A verify after a restart is a new record, not a retry: only before the
            // request does it go in.
            if state.landing.map(|l| l.changeset) != Some(head) {
                verify(&store, &fork("f"), &w1(), head);
            }
            ask_landing(&store, &fork("f"), &w1(), head).unwrap();
            store.advance_main(&fork("f"), head).unwrap();
        }
        let head = store.fork_head(&fork("f")).unwrap();
        let next = counter(&store, &head) + 1;
        let body = format!("{next}\n").repeat(512);
        let cs = write(
            &store,
            &head,
            &[
                ("counter", next.to_string().as_bytes()),
                (&format!("files/{}", next % 50), body.as_bytes()),
            ],
        );
        let published = store.publish_draft(&fork("f"), &w1(), &cs).unwrap();
        println!("{ACKED}{}", published.id);
    }
}

/// The fenced child: take the fork over once, then publish fenced drafts forever as
/// the epoch-1 writer, chaining from the sibling's head. Not a test on its own.
#[test]
#[ignore = "run by kill_during_fenced_append as a child process"]
fn child_fenced_writer() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let store = Store::open(&dir).unwrap();
    let state = store.fork(&fork("f")).unwrap().unwrap();
    if state.epoch() == 1 {
        assert_eq!(store.take_over(&fork("f"), 1, &taker()).unwrap(), 2);
    }
    loop {
        // The sibling's first draft starts where epoch 1 left the fork.
        let head = match store.fork(&sibling()).unwrap() {
            Some(_) => store.fork_head(&sibling()).unwrap(),
            None => {
                let f = store.fork(&fork("f")).unwrap().unwrap();
                store.tree(f.released_at(1).unwrap()).unwrap()
            }
        };
        let next = counter(&store, &head) + 1;
        let body = format!("{next}\n").repeat(512);
        let cs = write(
            &store,
            &head,
            &[
                ("counter", next.to_string().as_bytes()),
                (&format!("files/{}", next % 50), body.as_bytes()),
            ],
        );
        let published = store.publish_draft(&fork("f"), &w1(), &cs).unwrap();
        assert!(published.fenced && published.fork == sibling() && published.epoch == 1);
        println!("{ACKED}{}", published.id);
    }
}

#[test]
fn kill_during_append() {
    kill_loop("child_writer", |round, store, changesets| {
        let drafts = store
            .records(0, usize::MAX)
            .unwrap()
            .into_iter()
            .filter(|r| matches!(r.event, Event::DraftPublished { .. }))
            .count() as u64;
        let head = store.fork_head(&fork("f")).unwrap();
        assert_eq!(
            drafts,
            counter(store, &head),
            "round {round}: drafts in the log are not the head's chain"
        );
        assert_eq!(changesets, drafts);
        // Each draft is followed by its intent and its move of main, so each lags the
        // one before by at most the step the kill interrupted.
        let count = |want: fn(&Event) -> bool| {
            store
                .records(0, usize::MAX)
                .unwrap()
                .iter()
                .filter(|r| want(&r.event))
                .count() as u64
        };
        let intents = count(|e| matches!(e, Event::LandingIntent { .. }));
        let moves = count(|e| matches!(e, Event::MainAdvanced { .. }));
        assert!(
            intents <= drafts && drafts <= intents + 1 && moves <= intents && intents <= moves + 1,
            "round {round}: {drafts} drafts, {intents} intents, {moves} moves of main"
        );
        assert_eq!(counter(store, &store.main_tree(&loc("l")).unwrap()), moves);
    });
}

#[test]
fn kill_during_fenced_append() {
    kill_loop("child_fenced_writer", |round, store, changesets| {
        let records = store.records(0, usize::MAX).unwrap();
        let fenced = records
            .iter()
            .filter(
                |r| matches!(&r.event, Event::DraftFenced { sibling: s, .. } if *s == sibling()),
            )
            .count() as u64;
        let published = records
            .iter()
            .filter(|r| matches!(r.event, Event::DraftPublished { .. }))
            .count();
        let takeovers = records
            .iter()
            .filter(|r| matches!(r.event, Event::TakenOver { .. }))
            .count();
        assert_eq!(
            takeovers, 1,
            "round {round}: the fork was taken over {takeovers} times"
        );
        assert_eq!(
            published, 0,
            "round {round}: a fenced writer published to the fork"
        );
        let f = store.fork(&fork("f")).unwrap().unwrap();
        assert_eq!((f.epoch(), f.holder()), (2, Some(&taker())));
        assert_eq!(counter(store, &store.fork_head(&fork("f")).unwrap()), 0);
        let s = store
            .fork(&sibling())
            .unwrap()
            .unwrap_or_else(|| panic!("round {round}: no sibling after acknowledged drafts"));
        assert_eq!(s.parent, Some(fork("f")));
        assert_eq!(s.holder_at(1), Some(&agent()));
        assert_eq!(
            fenced,
            counter(store, &store.fork_head(&sibling()).unwrap()),
            "round {round}: fenced drafts in the log are not the sibling head's chain"
        );
        assert_eq!(changesets, fenced);
    });
}

/// Run `child_name` (a test of this binary) [`KILLS`] times against one store, SIGKILL it
/// at a random moment after its first acknowledged draft each time, and after each kill
/// check that the log replays, every acknowledged draft is in it, and `check(round,
/// store, changesets)` holds, `changesets` being the count the replay found.
fn kill_loop(child_name: &str, check: impl Fn(usize, &Store, u64)) {
    let dir = tempfile::tempdir().unwrap();
    {
        let store = Store::open(dir.path()).unwrap();
        let main = tree(&store, &[("counter", b"0")]);
        store.add_location(&loc("l"), None, &main).unwrap();
        store.create_fork(&fork("f"), &loc("l"), &agent()).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    // A small deterministic generator, so a failure reproduces.
    let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
    let mut acked_total = 0usize;

    for round in 0..KILLS {
        let mut child = Command::new(&exe)
            .args(["--exact", child_name, "--ignored", "--nocapture"])
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
        // Wait until the child has acknowledged its first draft of this run, so the
        // kill lands while it is appending, then let it run for 0-30 ms more.
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
                        "round {round}: no draft acknowledged within 60 s: {}",
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
        let acked: Vec<ChangesetId> = early
            .into_iter()
            .chain(rx.try_iter())
            .filter_map(|l| {
                l.find(ACKED)
                    .map(|at| ChangesetId::from_hex(&l[at + ACKED.len()..]).unwrap())
            })
            .collect();
        acked_total += acked.len();

        let store = Store::open(dir.path()).unwrap();
        let summary = store
            .replay_check()
            .unwrap_or_else(|e| panic!("round {round}: replay failed: {e}"));
        for id in &acked {
            assert!(
                store.changeset(*id).unwrap().is_some(),
                "round {round}: acknowledged draft {id} is not in the log"
            );
        }
        check(round, &store, summary.changesets as u64);
    }
    eprintln!(
        "{child_name}: {KILLS} kills, each after at least one acknowledged draft; \
         {acked_total} acknowledged in all"
    );
}
