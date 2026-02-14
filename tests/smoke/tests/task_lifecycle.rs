//! E2E smoke test: Task Lifecycle (docs/workflows/07-task-lifecycle.md)
//!
//! Spawn an ephemeral task, observe it run, retrieve its status, kill it:
//! 1. `POST /v1/tasks/spawn` with an inline image
//! 2. Returns immediately with task ID and address
//! 3. Monitor via `GET /v1/tasks` — status transitions: pulling -> starting -> running
//! 4. Kill via `DELETE /v1/tasks/:id`
//! 5. Verify: task status becomes stopped, not restarted
//!
//! Tasks are NEVER restarted by Mill. Exit 0 -> stopped, non-zero -> failed.

use std::collections::HashMap;

use smoke::{Api, MillNode, SpawnTaskRequest, poll_async, require_harness};

const TOKEN: &str = "mill_t_smoke_task_lifecycle";

/// Spawn an ephemeral task, poll until it appears in the task list,
/// kill it, and verify it transitions to a terminal state (stopped/failed).
#[tokio::test]
async fn spawn_poll_kill_task() {
    require_harness!();
    smoke::reset_node_counter();

    // Step 1: Bootstrap a single-node cluster.
    let node = MillNode::init(TOKEN).await;
    let api = node.api();

    // Verify the cluster starts with zero tasks.
    let status = api.status().await.expect("GET /v1/status");
    assert_eq!(status.task_count, 0, "fresh cluster should have no tasks");

    // Step 2: Spawn a task with an inline image.
    let spawn_req = SpawnTaskRequest {
        task: None,
        image: Some("docker.io/library/busybox:latest".into()),
        cpu: 0.25,
        memory: "128M".into(),
        env: HashMap::from([("SMOKE_TEST".into(), "task_lifecycle".into())]),
        timeout: Some("5m".into()),
    };

    let spawn_resp = api.spawn_task(spawn_req).await.expect("POST /v1/tasks/spawn");
    let task_id = spawn_resp.id.clone();
    assert!(!task_id.is_empty(), "spawn should return a non-empty task ID");
    assert!(!spawn_resp.node.is_empty(), "spawn should assign a node");
    // Initial status should be one of the early lifecycle states.
    assert!(
        ["pulling", "starting", "running"].contains(&spawn_resp.status.as_str()),
        "initial status should be pulling, starting, or running; got: {}",
        spawn_resp.status,
    );

    // Step 3: Poll until the task appears in the task list.
    poll_async(|| {
        let api = Api::new(node.api_addr, TOKEN);
        let task_id = task_id.clone();
        async move {
            let Ok(tasks) = api.tasks().await else { return false };
            tasks.iter().any(|t| t.id == task_id)
        }
    })
    .secs(15)
    .expect("task should appear in task list")
    .await;

    // Verify task is listed and has expected fields.
    let tasks = api.tasks().await.expect("GET /v1/tasks");
    let task = tasks.iter().find(|t| t.id == task_id).expect("task should be in list");
    assert_eq!(task.cpu, 0.25, "task CPU should match requested value");

    // Step 4: Verify the cluster status reflects the spawned task.
    let status = api.status().await.expect("GET /v1/status");
    assert!(status.task_count >= 1, "cluster should have at least 1 task");

    // Step 5: Kill the task.
    let kill_status = api.kill_task(&task_id).await.expect("DELETE /v1/tasks/:id");
    assert!(
        kill_status.is_success(),
        "kill_task should return a success status code; got: {kill_status}",
    );

    // Step 6: Verify the task transitions to a terminal state (stopped or failed).
    // "stopped" is the expected state for a killed task.
    // "failed" is also acceptable (e.g. if the kill races with a container error).
    // The task must NOT be restarted.
    poll_async(|| {
        let api = Api::new(node.api_addr, TOKEN);
        let task_id = task_id.clone();
        async move {
            let Ok(tasks) = api.tasks().await else { return false };
            match tasks.iter().find(|t| t.id == task_id) {
                Some(t) => t.status == "stopped" || t.status == "failed",
                // Task removed from list also counts as terminal.
                None => true,
            }
        }
    })
    .secs(15)
    .expect("task should reach terminal state after kill")
    .await;

    // Final assertion: if the task is still listed, confirm it is not "running"
    // and was not restarted (Mill never restarts tasks).
    let tasks = api.tasks().await.expect("GET /v1/tasks after kill");
    if let Some(task) = tasks.iter().find(|t| t.id == task_id) {
        assert!(
            task.status == "stopped" || task.status == "failed",
            "killed task must be stopped or failed, not restarted; got: {}",
            task.status,
        );
    }
}
