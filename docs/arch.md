# Mill: Architecture

## Overview

Mill is a container orchestrator for small-to-medium deployments (3-20 nodes,
5-50 services). Single binary, single config format, batteries included.

Two primitives: **services** (long-running) and **tasks** (ephemeral).

## Cluster topology

```
                          ┌────────────────────────────────────────┐
                          │               Mill Cluster             │
   Internet               │                                        │
      │                   │   ┌───────────┐  Raft  ┌───────────┐   │
      │     ┌─────────┐   │   │  primary  │◄──────►│  primary  │   │
      ├────►│  proxy  │   │   │+ secondary│        │+ secondary│   │
      │     │ builtin │   │   └─────┬─────┘        └─────┬─────┘   │
      │     └─────────┘   │         │ RPC                │ RPC     │
      │                   │         ▼                    ▼         │
      │                   │   ┌───────────┐        ┌───────────┐   │
      │                   │   │ secondary │        │ secondary │   │
      │                   │   │           │        │           │   │
      │                   │   └───────────┘        └───────────┘   │
      │                   │                                        │
      │                   │ All nodes connected via WireGuard mesh │
      │                   └────────────────────────────────────────┘
      │
  Terraform manages VMs, DNS, firewall rules
```

Every node runs the same `mill` binary. Nodes are both primary (Raft voter)
and secondary (runs containers) by default. For larger clusters, roles can be
separated with `--role`.

## Core design decisions

### Imperative dispatch, not declarative reconciliation

K8s uses level-triggered reconciliation: components watch desired state and
converge independently. This is elegant at scale but creates startup latency,
debugging opacity, and conceptual overhead.

Mill uses imperative dispatch: the Raft leader directly tells secondaries
what to run via RPC. Simpler, faster, easier to reason about. The tradeoff
is that the leader is a single coordination point — acceptable for <20 nodes.

### No plugin system

One networking model (WireGuard), one container runtime (containerd), one
storage model (cloud block + ephemeral). Eliminates CNI/CSI/CRI abstraction layers
and the integration testing surface they create.

### Single resource value

No requests vs limits. The declared `cpu` and `memory` values are both the
scheduling reservation and the cgroup ceiling. Wastes some capacity;
eliminates noisy-neighbor problems and overcommit complexity.

### Embedded everything

Raft consensus, DNS server, reverse proxy, ACME client, secret store — all
run in-process. No external etcd, CoreDNS, nginx, cert-manager, or Vault.
Fewer moving parts, fewer failure modes.

## Crate dependency graph

```
               mill-config
                    │
       ┌────────────┼─────────────┐
       ▼            ▼             ▼
   mill-raft   mill-containerd  mill-net
       │            │             │
       │            │             ▼
       │            │         mill-proxy
       │            │             │
       └────────────┼─────────────┘
                    ▼
                mill-node
                    │
                    ▼
                mill-cli
```

## Component architecture

```
┌─────────────────────────────────────────────────────┐
│                      mill-node                      │
│                                                     │
│  Primary role:              Secondary role:         │
│  ┌──────────────────┐       ┌──────────────────┐    │
│  │ API server       │       │ RPC server       │    │
│  │ Scheduler        │       │ Heartbeat loop   │    │
│  │ Deploy coord     │  RPC  │ Container runner │    │
│  │ Route/DNS comp   │◄─────►│ Log ring buffer  │    │
│  │                  │       │ Metrics collector│    │
│  └────────┬─────────┘       └────────┬─────────┘    │
│           │                          │              │
│  ┌────────┴─────────┐       ┌────────┴─────────┐    │
│  │   mill-raft      │       │  mill-containerd │    │
│  │   (consensus +   │       │  (containerd     │    │
│  │    FSM + secrets)│       │   gRPC client)   │    │
│  └──────────────────┘       └──────────────────┘    │
│           │                          │              │
│  ┌────────┴──────────────────────────┴───────────┐  │
│  │                 mill-net                      │  │
│  │  WireGuard mesh  │  DNS server  │  Port pool  │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │              mill-proxy                       │  │
│  │  Reverse proxy  │  ACME/TLS  │  Route table   │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │              mill-config                      │  │
│  │  Config parser  │  Core types  │  Validation  │  │
│  └───────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────┘
```

## State machine

All cluster state lives in the Raft FSM. Commands are applied to produce
state transitions. No external database.

```diagram
Commands:                                        FSM State:
  Deploy(config)                                   config:  ClusterConfig
  NodeRegister(id, mill_id, addr, resources)       nodes:   Map<u64, FsmNode>
  NodeHeartbeat(id, statuses)                      allocs:  Map<AllocId, Alloc>
  NodeDown(id)                                     secrets: Map<String, Encrypted>
  NodeDrain(id)                                    volumes: Map<String, FsmVolume>
  NodeRemove(id)
  AllocScheduled(id, node, name, kind, addr, res)
  AllocStatus(id, status)
  AllocRunning(id, addr, started_at)
  SecretSet(name, encrypted_value, nonce)
  SecretDelete(name)
  VolumeCreated(name, cloud_id, node)
  VolumeAttached(name, node)
  VolumeDetached(name)
  VolumeDestroyed(name)
```

The FSM is a pure data structure with no I/O. Testable without infrastructure.

## Allocation lifecycle

```
                ┌──────────┐
Spawn/deploy →  │ pulling  │  (if image not cached on target node)
                └────┬─────┘
                     ▼
                ┌──────────┐
                │ starting │  (containerd create + start)
                └────┬─────┘
                     ▼
                ┌──────────┐   health check passes
                │ running  │ ───────────────────→ healthy  (services only)
                └────┬─────┘
                     │
            exit / timeout / kill
                ┌────┴─────┐
                ▼          ▼
           ┌─────────┐ ┌────────┐
           │ stopped │ │ failed │  (container error, OOM, image pull fail)
           └─────────┘ └────────┘
                └──┬───────┘
                   ▼
         logs in memory ring buffer (lost on node restart)

Services: restarted on stopped/failed (backoff: 1s, 5s, 30s, 60s cap).
Tasks: not restarted. Caller decides.
```

## Failure handling

| Failure | Response |
|---------|----------|
| Secondary dies | Detected via missed heartbeats (15s). Service allocs rescheduled. Task allocs marked failed. |
| Primary dies | Raft elects new leader (<5s). New primary rebuilds from log. |
| Container crashes | Services: restart with backoff. Tasks: report exit code, no retry. |
| Network partition | Minority partition can't commit writes. Existing containers keep running. Converges on heal. |
| Volume-backed service on dead node | Detach volume, attach to healthy node, reschedule (persistent). Restart fresh elsewhere (ephemeral). |

## Networking model

- **Node-to-node:** WireGuard mesh, configured at join time. Always encrypted.
- **Container networking:** Host networking with mapped ports from per-node port pool.
- **Service discovery:** Built-in DNS on every node. `*.mill` names resolve to node_ip:mapped_port. Updated synchronously with allocation changes.
- **External ingress:** Built-in reverse proxy with automatic Let's Encrypt TLS.

## Storage model

Mill owns the full storage lifecycle via a cloud provider driver. Terraform
provisions VMs and networking; Mill provisions storage.

- **Persistent volumes** (default): cloud block devices (e.g., DO Volumes).
  Mill creates, attaches, detaches, and migrates them via a cloud provider
  driver (~60 lines per provider). On node failure, Mill detaches the volume
  and reattaches it to a healthy node.
- **Ephemeral volumes** (`ephemeral = true`): scratch space, no cloud API.
  Lost on reschedule or node failure. Suitable for caches and temp files.

The scheduler places volume-bound services on the node where the volume
is attached.

Cloud provider set at `mill init --provider <name>`. One driver per
provider, each implementing create/destroy/attach/detach/list. Stored
in `mill-node/src/storage/<provider>/`.

## Scope boundary

Mill handles everything between "VMs exist" and "application code runs."
Terraform provisions infrastructure. Mill deploys and operates containers.

| Terraform           | Mill |
|---------------------|------|
| Provision VMs       | Schedule containers |
| VPC / firewall      | WireGuard mesh |
| DNS zone + records  | Internal service discovery |
| Install mill binary | Ingress + TLS |
|                     | Volumes (create, attach, migrate) |
|                     | Secrets, logs, metrics, rolling deploys |
