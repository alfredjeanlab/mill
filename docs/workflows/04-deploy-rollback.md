# Workflow: Deploy Rollback

Automatic rollback when a new version fails health checks.

## Diagram

```mermaid
flowchart TD
    A[Deploy: new config received] --> B[Diff: detect changed service]
    B --> C[Schedule new alloc]
    C --> D[Pull image + start container]
    D --> E{Alloc status?}

    E -- running --> F[Poll health endpoint]
    E -- "failed (crash/exit)" --> H

    F --> G{Health check?}
    G -- "200 within timeout" --> OK[Healthy]
    G -- "timeout / failure" --> H[Rollback]

    H --> I[Kill new alloc]
    I --> J[Keep old alloc running]
    J --> K["SSE: event: failed"]

    OK --> L[Update DNS + proxy routes]
    L --> M[Stop old alloc]
    M --> N["SSE: event: done"]

    style H fill:#d32,stroke:#a00,color:#fff
    style OK fill:#2a2,stroke:#070,color:#fff
```

## Steps

### 1. Trigger

```bash
mill deploy -f production.mill   # "office" image updated to a bad version
```

Failure causes: crash loop, misconfigured health endpoint, image that exits
immediately, health check timeout.

### 2. New alloc starts but never becomes healthy

Primary diffs config, begins rolling update. New alloc reaches `running` on the
secondary, but the health endpoint never returns 200. After the health check
timeout (default 30s), the primary declares failure.

### 3. Rollback

Primary kills the new alloc and aborts the deploy. The old alloc was never
stopped and continues serving traffic. DNS and proxy routes were never modified.

### 4. SSE stream

The stream never emits `healthy` or `stopped`. The terminal event is `failed`:

```
event: progress
data: {"phase": "started"}

event: progress
data: {"service": "office", "phase": "scheduled", "node": "node-2"}

event: progress
data: {"service": "office", "phase": "starting"}

event: failed
data: {"service": "office", "reason": "health check timeout after 30s"}
```

```
$ mill deploy -f production.mill
  office: scheduled on node-2
  office: starting...
  office: failed: health check timeout after 30s
  deploy failed
```

## Key Points

- **No downtime:** The old alloc is never stopped during a failed deploy.
- **No route change:** DNS and proxy routes are never updated until healthy.
- **Cleanup:** The new alloc is always killed on failure.
- **Per-service:** If service A succeeds but B fails, A keeps its new version
  and B keeps its old version.
