# Mill: Overview

Mill is a container orchestrator for small-to-medium deployments — 3 to 20 nodes, 5 to 50
services. It ships as a single binary with networking, ingress, TLS, DNS, secrets, storage, logs,
and metrics built in. The leader tells nodes what to run directly rather than relying on declarative
reconciliation.

## Cluster Model

Every node runs the same `mill` binary as both primary and secondary by default. Primaries form a
Raft consensus group; the elected leader schedules work and dispatches commands to secondaries.
Non-leaders forward writes and serve reads. All cluster state lives in a replicated state machine.

Secondaries send periodic heartbeats. A silent node is marked down and its services rescheduled.

## Deploy Lifecycle

Mill has two workload types:
- Services are long-running, health-checked, and rolling-updated.
- Tasks are ephemeral, run to completion or timeout, and never restarted.

`mill deploy` validates the config, diffs it against the current state, and redeploys only what changed.
Images are pulled in parallel, then the scheduler places replicas based on volume affinity,
available resources, and spread.

Existing services get a rolling update: start new, health-check, re-route, stop old — with
automatic rollback on failure. The CLI streams progress and blocks until complete.

## Network Layer

A WireGuard mesh encrypts all node-to-node traffic, maintained automatically as nodes come and go.
Containers use host networking — no overlay network.

### Service Discovery

Built-in DNS resolves `*.mill` names to service addresses, updated synchronously with scheduling
changes. Services reference each other through `${service.name.address}` interpolation in config.

### Ingress

A built-in reverse proxy routes external traffic by hostname and path.
TLS certificates are acquired automatically via Let's Encrypt and renewed in the background.
Individual routes can opt into WebSocket support.

## Storage and Secrets

### Volumes

Persistent volumes are cloud block devices whose lifecycle Mill manages: create, attach, migrate
on failure, destroy on deletion. The scheduler pins services to the node where their volume is
attached. Ephemeral volumes provide host-local scratch space, lost on reschedule.

### Secrets

Secrets are encrypted and replicated with the cluster state. Config files reference them via
`${secret(name)}`; they are injected as environment variables at container start.

## Operational Properties

### Failure Recovery

A node that misses heartbeats is marked down.

Its services are rescheduled with backoff; its tasks are marked failed.

A crashed secondary recovers automatically by reattaching to still-running containers. A crashed
leader is replaced by Raft election, typically within five seconds.

### Observability

Log streaming with tail and live-follow is built in across all services.
Prometheus metrics cover resource usage, restart counts, proxy latencies, and task durations.

## Scope Boundary

Terraform provisions infrastructure (VMs, VPC, DNS zones, firewall) and installs the `mill` binary.

Mill handles everything from first boot: cluster formation, scheduling, networking, ingress, TLS,
service discovery, storage, secrets, logs, metrics, and zero-downtime deploys.
