//! Raft over an in-process mesh with fault injection (spec/14-clustering.md §15).

use glidex_raftstore::store::{StoreError, definition};
use glidex_raftstore::testing::{Test, TestCluster, TestOrigin, TestTable};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_member_commits_writes() {
    let c = TestCluster::new(1).await;
    c.put(1, "k", "v").unwrap();
    assert!(c.by_id(1).db.revision() > 0);
    let dump = c.by_id(1).db.dump().unwrap();
    assert!(
        dump.iter()
            .any(|(_, rows)| rows.iter().any(|(k, v)| k == "k" && v == b"v"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_replicate_to_every_member() {
    let c = TestCluster::new(3).await;
    let leader = c.leader(&c.ids()).await;
    for i in 0..20 {
        c.put(leader, &format!("k{i}"), "v").unwrap();
    }
    c.converge(&c.ids(), leader).await;
    assert!(c.same_data(&c.ids()));
    // A follower refuses, and says who the leader is.
    let follower = c.ids().into_iter().find(|i| *i != leader).unwrap();
    match c.put(follower, "x", "y") {
        Err(StoreError::NotLeader { .. }) => {}
        other => panic!("expected NotLeader, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killing_the_leader_keeps_writes_going() {
    let c = TestCluster::new(3).await;
    let old = c.leader(&c.ids()).await;
    c.put(old, "before", "1").unwrap();
    c.kill(old).await;
    let rest: Vec<_> = c.ids().into_iter().filter(|i| *i != old).collect();
    let new = c.leader(&rest).await;
    assert_ne!(new, old);
    c.put(new, "after", "2").unwrap();
    c.converge(&rest, new).await;
    assert!(c.same_data(&rest));
    let dump = c.by_id(rest[0]).db.dump().unwrap();
    for k in ["before", "after"] {
        assert!(
            dump.iter()
                .any(|(_, rows)| rows.iter().any(|(kk, _)| kk == k)),
            "{k} is missing"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_partitioned_leaders_writes_are_rejected() {
    let c = TestCluster::new(3).await;
    let old = c.leader(&c.ids()).await;
    c.put(old, "committed", "1").unwrap();
    c.converge(&c.ids(), old).await;
    c.mesh.isolate(old, &c.ids());
    // The majority elects a new leader; the old one still thinks it leads.
    let rest: Vec<_> = c.ids().into_iter().filter(|i| *i != old).collect();
    let new = c.leader(&rest).await;
    // The isolated leader's write can't reach a majority.
    let stale = {
        let db = c.by_id(old).db.clone();
        tokio::task::spawn_blocking(move || {
            db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
                tx.open_table(definition::<Test>(TestTable::Data))?
                    .insert("stale", b"x".as_slice())?;
                Ok(())
            })
        })
    };
    c.put(new, "fresh", "2").unwrap();
    c.mesh.heal();
    let res = tokio::time::timeout(Duration::from_secs(20), stale)
        .await
        .expect("the stale write returned")
        .unwrap();
    assert!(
        res.is_err(),
        "a write by a deposed leader must fail, got {res:?}"
    );
    c.converge(&c.ids(), new).await;
    assert!(c.same_data(&c.ids()));
    for i in c.ids() {
        let dump = c.by_id(i).db.dump().unwrap();
        assert!(
            !dump
                .iter()
                .any(|(_, rows)| rows.iter().any(|(k, _)| k == "stale")),
            "the stale write reached {i}"
        );
        assert!(
            dump.iter()
                .any(|(_, rows)| rows.iter().any(|(k, _)| k == "fresh"))
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lagging_learner_catches_up_from_a_snapshot() {
    let mut c = TestCluster::new(3).await;
    let leader = c.leader(&c.ids()).await;
    // Enough writes that the log has been purged (the test keeps ~100).
    for i in 0..400 {
        c.put(leader, &format!("k{i:04}"), "v").unwrap();
    }
    c.converge(&c.ids(), leader).await;
    // Wait for Raft to purge, so a new member can't be fed from the log.
    let node = c.by_id(leader).node.clone();
    node.raft.trigger().snapshot().await.unwrap();
    node.raft
        .wait(Some(Duration::from_secs(20)))
        .metrics(|m| m.snapshot.is_some(), "snapshot")
        .await
        .unwrap();
    node.raft.trigger().purge_log(300).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idx = c.add_started(4).await;
    let id = c.nodes[idx].id;
    node.add_learner(id, "n4").await.unwrap();
    c.converge(&[id], leader).await;
    assert!(
        node.raft.metrics().borrow().purged.is_some(),
        "the log was never purged, so no snapshot was needed"
    );
    assert!(
        c.by_id(id).node.raft.metrics().borrow().snapshot.is_some(),
        "the new member installed no snapshot"
    );
    assert!(c.same_data(&[leader, id]), "the new member's data differs");
    assert!(
        c.by_id(id)
            .db
            .dump()
            .unwrap()
            .iter()
            .any(|(_, rows)| rows.iter().any(|(k, _)| k == "k0399"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn membership_changes_under_load() {
    let mut c = TestCluster::new(3).await;
    let leader = c.leader(&c.ids()).await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let db = c.by_id(leader).db.clone();
        let stop = stop.clone();
        tokio::task::spawn_blocking(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let r = db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
                    tx.open_table(definition::<Test>(TestTable::Data))?
                        .insert(&format!("w{n}"), b"1".as_slice())?;
                    Ok(())
                });
                if r.is_ok() {
                    n += 1;
                }
            }
            n
        })
    };
    // 3 → 5 voters, then back to 3, while the writer runs.
    for id in [4u64, 5] {
        let idx = c.add_started(id).await;
        assert_eq!(c.nodes[idx].id, id);
        c.by_id(leader)
            .node
            .add_learner(id, &format!("n{id}"))
            .await
            .unwrap();
    }
    c.by_id(leader)
        .node
        .set_voters((1..=5).collect())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.by_id(leader)
        .node
        .set_voters(
            [leader]
                .into_iter()
                .chain([1, 2, 3].into_iter().filter(|i| *i != leader).take(2))
                .collect(),
        )
        .await
        .unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let written = writer.await.unwrap();
    assert!(written > 10, "the writer made only {written} writes");
    let voters: Vec<u64> = c.by_id(leader).node.voters().into_iter().collect();
    assert_eq!(voters.len(), 3);
    c.converge(&voters, leader).await;
    assert!(c.same_data(&voters));
}

/// A write set over the schema's limit is refused before it is proposed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_sets_over_the_schemas_limit_are_refused() {
    let c = TestCluster::new(1).await;
    c.put(1, "small", "v").unwrap();
    let big = "x".repeat(100 * 1024);
    assert!(matches!(
        c.put(1, "big", &big),
        Err(StoreError::TooLarge(_))
    ));
}

/// A member that has stopped refuses writes, and lets go of its store: the
/// store can be opened again once the member is dropped (the store holds
/// the member through its replicator, which would otherwise keep both).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopped_member_lets_go_of_its_store() {
    let c = TestCluster::new(1).await;
    c.put(1, "k", "v").unwrap();
    let path = c.by_id(1).dir.join("store.db");
    c.kill(1).await;
    assert!(matches!(
        c.put(1, "k2", "v"),
        Err(StoreError::NotLeader { .. })
    ));
    let glidex_raftstore::testing::TestCluster {
        mesh, nodes, root, ..
    } = c;
    drop(nodes);
    drop(mesh);
    // The member's tasks wind down on the runtime after the drop, so the
    // store closes shortly after, not at once: wait for it, but not forever.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match glidex_raftstore::store::Db::<Test>::create(&path) {
            Ok(_) => break,
            Err(StoreError::Database(redb::DatabaseError::DatabaseAlreadyOpen))
                if tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(e) => panic!("the store was not closed within 10s: {e}"),
        }
    }
    drop(root);
}
