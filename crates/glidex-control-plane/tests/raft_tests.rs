//! Raft over an in-process mesh with fault injection (spec/clustering.md §15).

use glidex_control_plane::cluster::raft::testing::TestCluster;
use glidex_control_plane::store::StoreError;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_single_member_commits_writes() {
    let c = TestCluster::new(1).await;
    c.put(1, "k", "v").unwrap();
    assert_eq!(c.by_id(1).db.revision() > 0, true);
    let dump = c.by_id(1).db.dump().unwrap();
    assert!(dump.iter().any(|(_, rows)| rows.iter().any(|(k, v)| k == "k" && v == b"v")));
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
        assert!(dump.iter().any(|(_, rows)| rows.iter().any(|(kk, _)| kk == k)), "{k} is missing");
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
            db.write(glidex_control_plane::store::Origin::Api, |tx| -> Result<(), StoreError> {
                tx.open_table(glidex_control_plane::store::TableId::Meta.definition())?.insert("stale", b"x".as_slice())?;
                Ok(())
            })
        })
    };
    c.put(new, "fresh", "2").unwrap();
    c.mesh.heal();
    let res = tokio::time::timeout(Duration::from_secs(20), stale).await.expect("the stale write returned").unwrap();
    assert!(res.is_err(), "a write by a deposed leader must fail, got {res:?}");
    c.converge(&c.ids(), new).await;
    assert!(c.same_data(&c.ids()));
    for i in c.ids() {
        let dump = c.by_id(i).db.dump().unwrap();
        assert!(!dump.iter().any(|(_, rows)| rows.iter().any(|(k, _)| k == "stale")), "the stale write reached {i}");
        assert!(dump.iter().any(|(_, rows)| rows.iter().any(|(k, _)| k == "fresh")));
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
    node.raft.wait(Some(Duration::from_secs(20))).metrics(|m| m.snapshot.is_some(), "snapshot").await.unwrap();
    node.raft.trigger().purge_log(300).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let idx = c.add_started(4).await;
    let id = c.nodes[idx].id;
    node.add_learner(id, "n4").await.unwrap();
    c.converge(&[id], leader).await;
    assert!(node.raft.metrics().borrow().purged.is_some(), "the log was never purged, so no snapshot was needed");
    assert!(c.by_id(id).node.raft.metrics().borrow().snapshot.is_some(), "the new member installed no snapshot");
    assert!(c.same_data(&[leader, id]), "the new member's data differs");
    assert!(c.by_id(id).db.dump().unwrap().iter().any(|(_, rows)| rows.iter().any(|(k, _)| k == "k0399")));
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
                let r = db.write(glidex_control_plane::store::Origin::Api, |tx| -> Result<(), StoreError> {
                    tx.open_table(glidex_control_plane::store::TableId::Meta.definition())?.insert(&format!("w{n}"), b"1".as_slice())?;
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
        c.by_id(leader).node.add_learner(id, &format!("n{id}")).await.unwrap();
    }
    c.by_id(leader).node.set_voters((1..=5).collect()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    c.by_id(leader).node.set_voters([leader].into_iter().chain([1, 2, 3].into_iter().filter(|i| *i != leader).take(2)).collect()).await.unwrap();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let written = writer.await.unwrap();
    assert!(written > 10, "the writer made only {written} writes");
    let voters: Vec<u64> = c.by_id(leader).node.voters().into_iter().collect();
    assert_eq!(voters.len(), 3);
    c.converge(&voters, leader).await;
    assert!(c.same_data(&voters));
}
