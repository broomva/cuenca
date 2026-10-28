use cuenca_core::gates::{AuthorKind, LandingRequest};
use cuenca_core::{ForkName, Holder, LocationName, NodeId, PrincipalName, Store, Writer};
use cuenca_tree::{Changeset, Entry, GitOid, Op, RepoPath, Tree};
use std::path::Path;

pub fn fork(name: &str) -> ForkName {
    ForkName::new(name).unwrap()
}
pub fn holder(name: &str) -> Holder {
    Holder::new(
        PrincipalName::new(name).unwrap(),
        NodeId::new("testNode").unwrap(),
    )
}
fn write(store: &Store, base: &Tree, bytes: &[u8]) -> Changeset {
    let oid = GitOid::for_blob(bytes);
    store.ingest_blob(oid, bytes.len() as u64, bytes).unwrap();
    Changeset::new(
        base,
        vec![Op::put(
            RepoPath::new("receipt.txt").unwrap(),
            Entry::File(oid),
        )],
    )
    .unwrap()
}
/// Empty main, historical draft, replacement draft, stale-epoch sibling, writable
/// owner fork with a draft, revoked principal and a verified pending landing intent.
pub fn fixture(path: &Path) -> Store {
    let store = Store::open(path).unwrap();
    let location = LocationName::new("drill").unwrap();
    store.add_location(&location, None, &Tree::new()).unwrap();
    let agent = fork("agent");
    store
        .create_fork(&agent, &location, &holder("builder"))
        .unwrap();
    let first = write(&store, &Tree::new(), b"historical draft\n");
    store
        .publish_draft(&agent, &Writer::new(holder("builder"), 1), &first)
        .unwrap();
    let base = store.fork_head(&agent).unwrap();
    let second = write(&store, &base, b"current draft\n");
    store
        .publish_draft(&agent, &Writer::new(holder("builder"), 1), &second)
        .unwrap();
    store.take_over(&agent, 1, &holder("successor")).unwrap();
    let fenced = write(
        &store,
        &store.fork_head(&agent).unwrap(),
        b"offline fenced draft\n",
    );
    store
        .publish_draft(&agent, &Writer::new(holder("builder"), 1), &fenced)
        .unwrap();
    let owner = fork("owner-recovery");
    store
        .create_fork(&owner, &location, &Holder::volume())
        .unwrap();
    let draft = write(&store, &Tree::new(), b"owner draft\n");
    store
        .publish_draft(&owner, &Writer::new(Holder::volume(), 1), &draft)
        .unwrap();
    store
        .revoke(&PrincipalName::new("builder").unwrap())
        .unwrap();
    let writer = Writer::new(holder("successor"), 2);
    let id = store
        .fork(&agent)
        .unwrap()
        .unwrap()
        .head_changeset
        .unwrap()
        .0;
    store
        .record_verify(&agent, &writer, id, Tree::new().git_tree_id(), vec![])
        .unwrap();
    store
        .record_landing_intent(&agent, &writer, id, &LandingRequest::new(AuthorKind::Agent))
        .unwrap();
    store
}

pub fn assert_same(source: &Store, restored: &Store) {
    assert_eq!(
        source.records(0, usize::MAX).unwrap(),
        restored.records(0, usize::MAX).unwrap()
    );
    assert_eq!(source.forks().unwrap(), restored.forks().unwrap());
    assert_eq!(source.locations().unwrap(), restored.locations().unwrap());
    assert_eq!(
        source.replay_check().unwrap(),
        restored.replay_check().unwrap()
    );
    for tree in source.export_prefix().unwrap().trees.values() {
        for (_, entry) in tree.entries() {
            assert_eq!(
                source.read_blob(entry.oid()).unwrap(),
                restored.read_blob(entry.oid()).unwrap()
            );
        }
    }
}

pub fn resume(restored: &Store) {
    assert!(
        restored
            .revoked(&PrincipalName::new("builder").unwrap())
            .unwrap()
            .is_some()
    );
    let agent = fork("agent");
    let state = restored.fork(&agent).unwrap().unwrap();
    let id = state.head_changeset.unwrap().0;
    let before = restored.len().unwrap();
    restored
        .record_landing_intent(
            &agent,
            &Writer::new(holder("successor"), 2),
            id,
            &LandingRequest::new(AuthorKind::Agent),
        )
        .unwrap();
    assert_eq!(before, restored.len().unwrap());
    let offset = restored.advance_main(&agent, id).unwrap();
    assert_eq!(offset, restored.advance_main(&agent, id).unwrap());
    assert_eq!(before + 1, restored.len().unwrap());
    let owner = restored.fork_head(&fork("owner-recovery")).unwrap();
    assert_eq!(
        restored
            .read_blob(owner.get("receipt.txt").unwrap().oid())
            .unwrap(),
        b"owner draft\n"
    );
}
