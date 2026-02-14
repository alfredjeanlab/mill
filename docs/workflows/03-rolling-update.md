# Workflow: Rolling Update

Zero-downtime deploy when a service's image changes.

## Diagram

```mermaid
sequenceDiagram
    participant CLI as mill deploy
    participant P as Primary
    participant S as Secondary
    participant DNS as DNS / Proxy

    CLI->>P: POST /v1/deploy (new config)
    P->>P: Diff: "office" image changed

    Note over P,S: Per-replica rolling update

    P->>S: RPC: schedule new alloc (v2)
    P-->>CLI: SSE: scheduled (office, node-2)
    S->>S: Pull image + start container
    P-->>CLI: SSE: starting
    S-->>P: Heartbeat: alloc running
    P->>S: Health check poll
    S-->>P: HTTP 200 OK
    P-->>CLI: SSE: healthy (office, 2.1s)

    P->>DNS: Add v2 to route table + DNS
    P->>S: RPC: stop old alloc (v1)
    P-->>CLI: SSE: stopped (office)

    Note over DNS: Traffic now routes to v2 only

    P-->>CLI: SSE: done (4.2s)
```

## Steps

### 1. Trigger

```bash
mill deploy -f production.mill   # image tag changed for "office"
```

The primary diffs the new config against FSM state. Only the changed service
enters the rolling update path. Unchanged services are skipped.

### 2. Per-replica cycle

For each replica, sequentially:

1. **Schedule new alloc** — primary commits `AllocScheduled` to Raft.
2. **Pull image** — secondary pulls the new image (or uses cache).
3. **Start container** — containerd create + start. Status moves to `running`.
4. **Health check** — primary polls the health endpoint until `healthy`.
5. **Route traffic** — DNS records and proxy route table updated to include new alloc.
6. **Stop old alloc** — old container stopped. Status moves to `stopped`.

The next replica does not begin until the current one is healthy and receiving
traffic. This guarantees `healthy >= 1` throughout.

### 3. Completion

```
$ mill deploy -f production.mill
  office: scheduled on node-2
  office: healthy (2.1s)
  office: old instance stopped
  deployed (4.2s)
```

## Key Points

- **Zero-downtime:** At least one healthy replica exists at every point.
- **Atomic route update:** DNS and proxy updated before the old alloc stops.
- **Sequential cycling:** N replicas = N sequential cycles.
- **Rollback on failure:** A single replica health check failure aborts the
  update (see [04-deploy-rollback](./04-deploy-rollback.md)).
