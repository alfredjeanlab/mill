use std::time::Duration;

use mill_raft::fsm::command::Command;

use super::harness::{TestCluster, poll};

#[tokio::test]
async fn minority_cannot_write() {
    let cluster = TestCluster::new(3).await;

    let leader_idx = cluster.find_leader().await;
    // Partition: remove the other 2 nodes from the router so the leader
    // becomes a minority.
    for i in 0..cluster.node_count() {
        if i != leader_idx {
            cluster.raft_router().remove_node(cluster.raft_id(i));
        }
    }

    // Propose on the minority (isolated leader) — should timeout/error.
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        cluster.raft(leader_idx).propose(Command::SecretSet {
            name: "should-fail".into(),
            encrypted_value: vec![1],
            nonce: vec![2],
        }),
    )
    .await;

    // Either the propose times out or returns an error.
    let write_failed = match result {
        Err(_) => true,     // timeout
        Ok(Err(_)) => true, // raft error
        Ok(Ok(_)) => false,
    };
    assert!(write_failed, "minority should not be able to write");

    // Restore network for cleanup.
    for i in 0..cluster.node_count() {
        if i != leader_idx {
            cluster.raft_router().add_node(cluster.raft_id(i), cluster.raft(i).raft().clone());
        }
    }

    cluster.shutdown().await;
}

/// Helper: find a leader among the non-isolated majority nodes and propose.
async fn propose_on_majority(cluster: &TestCluster, isolated_idx: usize, cmd: Command) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        for i in 0..cluster.node_count() {
            if i == isolated_idx {
                continue;
            }
            if cluster.raft(i).propose(cmd.clone()).await.is_ok() {
                return;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("could not propose on majority within 2 seconds");
        }
        // Trigger elections on majority nodes to speed up leader establishment.
        for i in 0..cluster.node_count() {
            if i != isolated_idx {
                let _ = cluster.raft(i).raft().trigger().elect().await;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[tokio::test]
async fn majority_continues() {
    let cluster = TestCluster::new(3).await;

    let leader_idx = cluster.find_leader().await;

    // Partition: remove 1 node (minority). The remaining 2 form a majority.
    let isolated_idx = (0..cluster.node_count()).find(|&i| i != leader_idx).unwrap();
    cluster.raft_router().remove_node(cluster.raft_id(isolated_idx));

    // Write on the majority via Raft — should succeed.
    propose_on_majority(
        &cluster,
        isolated_idx,
        Command::SecretSet {
            name: "majority-write".into(),
            encrypted_value: vec![1, 2, 3],
            nonce: vec![4, 5, 6],
        },
    )
    .await;

    // Verify the secret is in the FSM on a majority node.
    let majority_node = (0..cluster.node_count()).find(|&i| i != isolated_idx).unwrap();
    let has =
        cluster.raft(majority_node).read_state(|fsm| fsm.get_secret("majority-write").is_some());
    assert!(has, "secret should be committed on majority");

    // Restore network.
    cluster
        .raft_router()
        .add_node(cluster.raft_id(isolated_idx), cluster.raft(isolated_idx).raft().clone());

    cluster.shutdown().await;
}

#[tokio::test]
async fn state_converges_on_heal() {
    let cluster = TestCluster::new(3).await;

    let leader_idx = cluster.find_leader().await;

    // Partition: isolate a non-leader node.
    let isolated_idx = (0..cluster.node_count()).find(|&i| i != leader_idx).unwrap();
    cluster.raft_router().remove_node(cluster.raft_id(isolated_idx));

    // Propose on the majority (leader may have changed after partition).
    propose_on_majority(
        &cluster,
        isolated_idx,
        Command::SecretSet {
            name: "converge-test".into(),
            encrypted_value: vec![10, 20],
            nonce: vec![30, 40],
        },
    )
    .await;

    // Heal the partition.
    cluster
        .raft_router()
        .add_node(cluster.raft_id(isolated_idx), cluster.raft(isolated_idx).raft().clone());

    // Wait for replication to catch up.
    poll(|| cluster.raft(isolated_idx).read_state(|fsm| fsm.get_secret("converge-test").is_some()))
        .expect("isolated node did not converge")
        .await;

    cluster.shutdown().await;
}
