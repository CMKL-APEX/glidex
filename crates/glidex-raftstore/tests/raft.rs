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

/// A write made from a plain thread (a control plane's startup, on its main
/// thread) the moment a restarted member is back: it finishes, or is
/// refused, within `secs`; it never waits for ever.
fn write_returns_within(c: &TestCluster, id: u64, key: &str, secs: u64) -> Result<(), StoreError> {
    let db = c.by_id(id).db.clone();
    let key = key.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = db.write(TestOrigin::Api, |t| -> Result<(), StoreError> {
            t.open_table(definition::<Test>(TestTable::Data))?
                .insert(&key, b"v")?;
            Ok(())
        });
        let _ = tx.send(r);
    });
    rx.recv_timeout(Duration::from_secs(secs))
        .unwrap_or_else(|_| panic!("a write on {id} still waits after {secs}s"))
}

/// A rolling restart, as an upgrade does it: the leader first, then another
/// member shortly after, each writing at once when it is back. Writes keep
/// finishing, and the cluster keeps a leader that commits.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn writes_at_startup_survive_a_rolling_restart() {
    let mut c = TestCluster::new(3).await;
    // Every member writes all the time (liveness, status), as servers do;
    // a write on a follower is refused at once.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let busy = |c: &TestCluster| {
        for id in c.ids() {
            // Weak: a restart reopens the database file.
            let db = std::sync::Arc::downgrade(&c.by_id(id).db);
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let Some(db) = db.upgrade() else { return };
                    let _ = db.write(TestOrigin::Api, |t| -> Result<(), StoreError> {
                        t.open_table(definition::<Test>(TestTable::Other))?
                            .insert(&format!("live{id}"), &n.to_le_bytes())?;
                        Ok(())
                    });
                    drop(db);
                    n += 1;
                    std::thread::sleep(Duration::from_millis(20));
                }
            });
        }
    };
    busy(&c);
    for round in 0..5 {
        let leader = c.leader(&c.ids()).await;
        c.put(leader, &format!("before{round}"), "v").unwrap();
        c.restart(leader).await;
        busy(&c);
        let _ = tokio::task::block_in_place(|| {
            write_returns_within(&c, leader, &format!("first{round}"), 30)
        });
        let other = c.ids().into_iter().find(|i| *i != leader).unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        c.restart(other).await;
        busy(&c);
        let _ = tokio::task::block_in_place(|| {
            write_returns_within(&c, other, &format!("second{round}"), 30)
        });
        let now = c.leader(&c.ids()).await;
        tokio::task::block_in_place(|| write_returns_within(&c, now, &format!("after{round}"), 30))
            .unwrap_or_else(|e| panic!("round {round}: the leader {now} refused a write: {e}"));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A leader cut off from every other member, with nothing healing it: its
/// write gives up within the bound instead of waiting for a commit that no
/// leader makes, so the writes queued behind it on the write lane don't
/// wait for ever either (prism1 and prism2, 2026-11-03).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_write_that_no_leader_commits_gives_up() {
    let c = TestCluster::new(3).await;
    let old = c.leader(&c.ids()).await;
    c.put(old, "committed", "1").unwrap();
    c.converge(&c.ids(), old).await;
    c.mesh.isolate(old, &c.ids());
    let write = |key: &'static str| {
        let db = c.by_id(old).db.clone();
        tokio::task::spawn_blocking(move || {
            db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
                tx.open_table(definition::<Test>(TestTable::Data))?
                    .insert(key, b"x".as_slice())?;
                Ok(())
            })
        })
    };
    let (first, queued) = (write("first"), write("queued"));
    // 10 election timeouts of the test's 1.6 s, and a margin.
    for (name, w) in [("first", first), ("queued", queued)] {
        let res = tokio::time::timeout(Duration::from_secs(40), w)
            .await
            .unwrap_or_else(|_| panic!("the {name} write still waits"))
            .unwrap();
        assert!(res.is_err(), "the {name} write can't commit, got {res:?}");
    }
    // The majority carries on.
    let rest: Vec<_> = c.ids().into_iter().filter(|i| *i != old).collect();
    let new = c.leader(&rest).await;
    c.put(new, "fresh", "2").unwrap();
}

/// A leader cut off with a write it can't commit says its log stalled,
/// once 10 election timeouts pass, and stops saying so when it is healed;
/// the majority, which elects a leader and commits, never stalls
/// (spec/14-clustering.md D30).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_whose_log_stops_says_so() {
    let c = TestCluster::new(3).await;
    let old = c.leader(&c.ids()).await;
    c.put(old, "committed", "1").unwrap();
    c.converge(&c.ids(), old).await;
    c.mesh.isolate(old, &c.ids());
    let db = c.by_id(old).db.clone();
    let write = tokio::task::spawn_blocking(move || {
        db.write(TestOrigin::Api, |tx| -> Result<(), StoreError> {
            tx.open_table(definition::<Test>(TestTable::Data))?
                .insert("stuck", b"x".as_slice())?;
            Ok(())
        })
    });
    let rest: Vec<_> = c.ids().into_iter().filter(|i| *i != old).collect();
    let new = c.leader(&rest).await;
    c.put(new, "fresh", "2").unwrap();
    // 10 election timeouts of the test's 1.6 s, and a margin.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while c.by_id(old).node.stalled_since().is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "n{old} doesn't say its log stalled"
        );
        for id in &rest {
            assert_eq!(c.by_id(*id).node.stalled_since(), None, "n{id} stalled");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(write.await.unwrap().is_err());
    c.mesh.heal();
    c.converge(&c.ids(), new).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while c.by_id(old).node.stalled_since().is_some() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "n{old} still says its log stalled once healed"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A member restarted more often than its snapshot policy fires still
/// trims its log to `log_keep_entries` (a restart makes the live database
/// the snapshot and starts the policy's count again; the log grew without
/// end on servers restarted every few hours).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_is_trimmed_across_restarts() {
    let mut c = TestCluster::new(3).await;
    let keep = c.settings.log_keep_entries;
    for round in 0..6 {
        let leader = c.leader(&c.ids()).await;
        for i in 0..60 {
            c.put(leader, &format!("r{round}-{i}"), "v").unwrap();
        }
        c.converge(&c.ids(), leader).await;
        for id in c.ids() {
            c.restart(id).await;
        }
    }
    let leader = c.leader(&c.ids()).await;
    c.put(leader, "last", "v").unwrap();
    c.converge(&c.ids(), leader).await;
    for id in c.ids() {
        let raft = c.by_id(id).node.raft.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let m = raft.metrics().borrow().clone();
            let last = m.last_log_index.unwrap_or(0);
            let purged = m.purged.map(|l| l.index).unwrap_or(0);
            if last - purged <= keep * 2 {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "n{id} keeps {} entries (purged up to {purged} of {last}), not about {keep}",
                last - purged
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// Members crashed between checkpoints (D31), under writes: a follower,
/// then the leader. Each comes back at its last checkpoint, applies the
/// rest again from its log, and ends with the same data as the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_crashed_between_checkpoints_catch_up() {
    let mut c = TestCluster::new(3).await;
    // Writes all along, through whichever member leads. Weak: a crashed
    // member's database goes, and the writer carries on with the others.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let dbs: Vec<_> = c
        .ids()
        .iter()
        .map(|id| std::sync::Arc::downgrade(&c.by_id(*id).db))
        .collect();
    let writer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                for db in &dbs {
                    let Some(db) = db.upgrade() else { continue };
                    let _ = db.write(TestOrigin::Api, |t| -> Result<(), StoreError> {
                        t.open_table(definition::<Test>(TestTable::Data))?
                            .insert(&format!("w{n}"), b"v")?;
                        Ok(())
                    });
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };
    let leader = c.leader(&c.ids()).await;
    let follower = c.ids().into_iter().find(|i| *i != leader).unwrap();
    for (role, id) in [("follower", follower), ("leader", leader)] {
        // A crash rolls back only what came since the last checkpoint: up
        // to a second's writes. Crash again until one did.
        let mut rolled_back = false;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let (applied, durable) = c.crash(id).await;
            if durable < Some(applied) {
                rolled_back = true;
                break;
            }
        }
        assert!(rolled_back, "no crash of the {role} lost an apply");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::task::spawn_blocking(move || writer.join().unwrap())
        .await
        .unwrap();
    let now = c.leader(&c.ids()).await;
    // One more write commits in the current term, so every member learns
    // everything before it is committed.
    tokio::task::block_in_place(|| write_returns_within(&c, now, "last", 30)).unwrap();
    c.converge(&c.ids(), now).await;
    assert!(c.same_data(&c.ids()), "the members' data differ");
}

/// `snapshot_now` makes the state durable as far as the position it
/// reports, which openraft may purge the log to (§6.2b, I4).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reported_snapshot_position_is_durable() {
    let c = TestCluster::new(1).await;
    let n = c.by_id(1);
    let mut i = 0;
    while n.db.unflushed() == 0 {
        assert!(i < 50, "every write was flushed");
        c.put(1, &format!("k{i}"), "v").unwrap();
        i += 1;
    }
    let applied = n.db.revision();
    n.node.raft.trigger().snapshot().await.unwrap();
    let m = n
        .node
        .raft
        .wait(Some(Duration::from_secs(10)))
        .metrics(
            |m| m.snapshot.is_some_and(|s| s.index >= applied),
            "the snapshot",
        )
        .await
        .unwrap();
    let reported = m.snapshot.unwrap().index;
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("crash.db");
    n.db.crash_image(&n.dir.join("store.db"), &image).unwrap();
    let durable = glidex_raftstore::testing::durable_applied(&image).unwrap();
    assert!(
        durable >= reported,
        "reported {reported}, durable only to {durable}"
    );
}
