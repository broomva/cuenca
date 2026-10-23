//! A blob batch holds no open file per blob: one batch of [`BLOBS`] blobs is ingested
//! by a child process whose soft open-file limit is [`FD_LIMIT`], far fewer.
//!
//! `a_batch_larger_than_the_fd_limit_is_stored` re-runs this test binary under
//! `ulimit -Sn` as a child that only runs `child_batch`.

use std::process::Command;

use cuenca_core::Store;
use cuenca_tree::GitOid;

const DIR_ENV: &str = "CUENCA_BATCH_FDS_TEST_DIR";
const BLOBS: usize = 1000;
const FD_LIMIT: u32 = 64;

fn content(i: usize) -> Vec<u8> {
    format!("blob {i}\n").into_bytes()
}

/// The child: one batch of every blob. Not a test on its own.
#[test]
#[ignore = "run by a_batch_larger_than_the_fd_limit_is_stored as a child process"]
fn child_batch() {
    let Ok(dir) = std::env::var(DIR_ENV) else {
        return;
    };
    let store = Store::open(&dir).unwrap();
    let mut batch = store.blob_batch();
    for i in 0..BLOBS {
        let bytes = content(i);
        batch
            .add(
                GitOid::for_blob(&bytes),
                bytes.len() as u64,
                bytes.as_slice(),
            )
            .unwrap();
    }
    batch.commit().unwrap();
}

#[test]
fn a_batch_larger_than_the_fd_limit_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let out = Command::new("/bin/sh")
        .args([
            "-c",
            &format!("ulimit -Sn {FD_LIMIT} && exec \"$0\" \"$@\""),
        ])
        .arg(&exe)
        .args(["--exact", "child_batch", "--ignored", "--nocapture"])
        .env(DIR_ENV, dir.path())
        .output()
        .unwrap();
    assert!(out.status.success(), "child: {out:?}");
    let store = Store::open(dir.path()).unwrap();
    for i in 0..BLOBS {
        let bytes = content(i);
        assert_eq!(store.read_blob(GitOid::for_blob(&bytes)).unwrap(), bytes);
    }
}
