//! Smoke test: Alloc Leak — FSM Allocs Never Pruned (docs/workflows/13-alloc-leak.md)
//!
//! Verifies that terminal task allocations (stopped/failed) are never removed
//! from the FSM alloc map, causing the task list to grow without bound:
//!   1. Spawn N short-lived tasks (killed by timeout)
//!   2. Wait for all N to reach a terminal state
//!   3. Assert that GET /v1/tasks still returns all N tasks
//!      (terminal allocs are never garbage-collected)

use smoke::{Api, MillNode, SpawnTaskRequest, poll_async, require_harness};
use std::collections::HashMap;

const TOKEN: &str = "mill_t_smoke_alloc_leak";

/// Number of tasks to spawn. Large enough to show unbounded accumulation but
/// small enough for the test to complete in a reasonable time.
const TASK_COUNT: usize = 10;

#[tokio::test]
async fn terminal_task_allocs_are_never_pruned() {
    require_harness!();
    smoke::reset_node_counter();

    let node = MillNode::init(TOKEN).await;
    let api = node.api();

    // Confirm we start with zero tasks.
    let initial = api.tasks().await.expect("GET /v1/tasks (initial)");
    assert_eq!(initial.len(), 0, "fresh cluster should have no tasks");

    // ---------------------------------------------------------------
    // Step 1: Spawn N tasks with a very short timeout so they reach a
    // terminal state (failed) quickly without us having to kill them.
    // ---------------------------------------------------------------
    let mut task_ids = Vec::with_capacity(TASK_COUNT);

    for _ in 0..TASK_COUNT {
        let req = SpawnTaskRequest {
            task: None,
            image: Some("docker.io/library/busybox:latest".into()),
            cpu: 0.1,
            memory: "64M".into(),
            env: HashMap::new(),
            // 1-second timeout — task is killed and transitions to "failed"
            // almost immediately, exercising the terminal-alloc code path.
            timeout: Some("1s".into()),
        };

        let resp = api.spawn_task(req).await.expect("POST /v1/tasks/spawn");
        task_ids.push(resp.id);
    }

    // ---------------------------------------------------------------
    // Step 2: Wait for every spawned task to reach a terminal state.
    // ---------------------------------------------------------------
    let api_addr = node.api_addr;
    let ids_for_poll = task_ids.clone();
    poll_async(move || {
        let ids = ids_for_poll.clone();
        async move {
            let Ok(tasks) = Api::new(api_addr, TOKEN).tasks().await else {
                return false;
            };
            ids.iter().all(|id| {
                tasks
                    .iter()
                    .find(|t| &t.id == id)
                    .map(|t| t.status == "stopped" || t.status == "failed")
                    .unwrap_or(false)
            })
        }
    })
    .secs(30)
    .expect("all spawned tasks should reach a terminal state within 30s")
    .await;

    // ---------------------------------------------------------------
    // Step 3: All N terminal allocs must still appear in the task list.
    //
    // Bug: terminal allocs are never garbage-collected from fsm.allocs.
    // A correct implementation might prune them; Mill never does.
    // ---------------------------------------------------------------
    let tasks_after = api.tasks().await.expect("GET /v1/tasks (after completion)");

    // Every task we spawned must still be present in the list.
    for id in &task_ids {
        let found = tasks_after.iter().find(|t| &t.id == id);
        assert!(
            found.is_some(),
            "task {id} should still appear in the task list after reaching terminal state — \
             terminal allocs are never pruned from the FSM"
        );
        let task = found.unwrap();
        assert!(
            task.status == "stopped" || task.status == "failed",
            "task {id} should be in a terminal state, got: {}",
            task.status
        );
    }

    // The total count must be at least TASK_COUNT (the alloc map only grows).
    assert!(
        tasks_after.len() >= TASK_COUNT,
        "expected at least {TASK_COUNT} tasks in the list, got {}",
        tasks_after.len()
    );
}
