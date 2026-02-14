# Mill: HTTP API

All cluster operations go through a single HTTP API served by the primary
on port 4400. Other nodes proxy requests to the current leader.

Auth: bearer token in the `Authorization` header. Single token, set at
`mill init`.

## Cluster

### `GET /v1/status`

Cluster overview: nodes, services, tasks, resource utilization.

### `GET /v1/nodes`

List all nodes with status, resources, and allocation counts.

### `POST /v1/nodes/{id}/drain`

Mark a node as draining. Migrates allocations to other nodes and marks the
node unschedulable.

### `DELETE /v1/nodes/{id}`

Remove a drained node from the cluster. Fails if the node still has active
allocations.

## Deploy

### `POST /v1/deploy`

Push a new config. Triggers rolling deploy for changed services.

**Body:** Config file content (`Content-Type: text/plain`).

**Response:** Streamed deploy progress (SSE). Final event is `done` or
`failed`.

```
event: progress
data: {"phase": "started"}

event: progress
data: {"phase": "pulling"}

event: progress
data: {"service": "office", "phase": "scheduled", "node": "node-2"}

event: progress
data: {"service": "office", "phase": "starting"}

event: progress
data: {"service": "office", "phase": "healthy", "elapsed": 2.1}

event: progress
data: {"service": "office", "phase": "stopped"}

event: done
data: {"elapsed": 4.2, "success": true}
```

Phases:

| Phase | Fields | Description |
|-------|--------|-------------|
| `started` | — | Deploy begins |
| `pulling` | — | Image pull broadcast (cluster-wide) |
| `scheduled` | `service`, `node` | Alloc placed on a node |
| `starting` | `service` | Container create + start |
| `healthy` | `service`, `elapsed` | Health check passed |
| `stopped` | `service` | Old alloc stopped (rolling update) |

Terminal events (sent as `event: done` or `event: failed`, not `event: progress`):

| Event | Fields | Description |
|-------|--------|-------------|
| `done` | `elapsed`, `success` | Deploy complete |
| `failed` | `service`, `reason` | Service-level error |

### `POST /v1/restart/:service`

Restart a service with the current image (new container).

## Services

### `GET /v1/services`

List all services with allocations, health, and addresses.

```json
[{
  "name": "office",
  "image": "registry.example.com/office:v2",
  "replicas": {"desired": 1, "healthy": 1},
  "allocations": [{
    "id": "alloc-a1b2",
    "node": "node-2",
    "status": "healthy",
    "address": "10.99.0.2:49200",
    "cpu": 0.3,
    "memory": 348127232,
    "started_at": "2026-02-14T10:00:00Z"
  }]
}]
```

## Tasks

### `POST /v1/tasks/spawn`

Spawn an ephemeral task.

**Body (template):**
```json
{
  "task": "coop-agent",
  "env": {"SESSION_ID": "abc123"}
}
```

**Body (inline):**
```json
{
  "image": "registry.example.com/myapp:v3",
  "cpu": 1,
  "memory": "2G",
  "timeout": "1h",
  "env": {"KEY": "value"}
}
```

**Response:**
```json
{
  "id": "coop-agent-x7k2",
  "address": "coop-agent-x7k2.mill:8080",
  "node": "node-3",
  "status": "running"
}
```

### `GET /v1/tasks`

List running task instances.

### `GET /v1/tasks/:id`

Task status, metadata, node, elapsed time.

### `DELETE /v1/tasks/:id`

Kill a running task.

## Logs

### `GET /v1/logs/:name`

Stream logs for a service or task instance.

**Query params:**
- `follow=true` — keep connection open, stream new lines (SSE)
- `tail=100` — last N lines only

**Response:** SSE stream of log lines.

```
data: {"ts": "2026-02-14T10:00:01Z", "stream": "stdout", "line": "listening on :3000"}
data: {"ts": "2026-02-14T10:00:02Z", "stream": "stderr", "line": "connected to database"}
```

## Secrets

### `GET /v1/secrets`

List secret names (not values).

### `PUT /v1/secrets/:name`

Set or update a secret.

**Body:** `{"value": "postgres://..."}`

### `GET /v1/secrets/:name`

Get a secret value.

### `DELETE /v1/secrets/:name`

Delete a secret.

## Volumes

### `GET /v1/volumes`

List persistent volumes with node, size, and bound service.

### `DELETE /v1/volumes/:name`

Destroy a persistent volume. Fails if a service is currently using it.

## Metrics

### `GET /v1/metrics`

Prometheus exposition format. Includes:

- `mill_alloc_cpu_usage{service, node}` — CPU usage per allocation
- `mill_alloc_memory_bytes{service, node}` — memory per allocation
- `mill_alloc_restarts_total{service, node}` — restart count
- `mill_proxy_requests_total{route}` — request count per route
- `mill_proxy_latency_seconds{route}` — request latency histogram
- `mill_node_cpu_available{node}` — free CPU per node
- `mill_node_memory_available_bytes{node}` — free memory per node
- `mill_tasks_spawned_total{task}` — task spawn count
- `mill_tasks_duration_seconds{task}` — task duration histogram
