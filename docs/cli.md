# Mill: CLI

Single binary, two modes: cluster management and client operations.

## Cluster bootstrap

```bash
# Initialize a new cluster (first node)
mill init [--token <token>] [--advertise <ip>] [--provider <name>] [--provider-token <token>]
          [--acme-email <email>] [--acme-staging]

# Join an existing cluster
mill join <address> [--advertise <ip>]

# Leave the cluster gracefully (drains allocations first)
mill leave
```

## Deploy

```bash
# Deploy or update from config file
mill deploy [-f <path>]            # default: ./mill.conf

# Restart a service (same image, new container)
mill restart <service>
```

`mill deploy` is synchronous. It prints progress and blocks until complete
or failed:

```
$ mill deploy -f production.mill
  office: pulling image... done (0.3s)
  office: starting v2... running (0.8s)
  office: health check... healthy (2.1s)
  office: routing traffic... done
  office: stopping v1... done
  ojd: image cached, skipping pull
  ojd: starting v2... running (0.6s)
  ojd: health check... healthy (1.4s)
  ojd: routing traffic... done
  ojd: stopping v1... done
  deployed (4.2s)
```

## Tasks

```bash
# Spawn an ephemeral task
mill spawn <task> [--env <KEY>=<VALUE>]...
mill spawn --image <image> [--cpu <n>] [--memory <size>] [--timeout <dur>] [--env <KEY>=<VALUE>]...

# List running tasks
mill ps

# Kill a running task
mill kill <task-id>
```

Spawn returns immediately with the task ID and address:

```
$ mill spawn coop-agent --env SESSION_ID=abc123
  id:      coop-agent-x7k2
  address: coop-agent-x7k2.mill:8080
  status:  starting
```

## Status

```bash
# Cluster overview
mill status
```

```example
$ mill status
CLUSTER: 3 nodes, all healthy

SERVICES
  NAME              REPLICAS  CPU      MEM       REQ/s  P99
  office            1/1       0.3/0.5  340M/512M  42    23ms
  terminal-proxy    1/1       0.1/0.5  80M/256M   12    8ms
  ojd               1/1       0.8/1    620M/1G    --    --

TASKS
  NAME         RUNNING  SPAWNED/24h  AVG DURATION
  coop-agent   3        47           34m

NODES
  NAME    STATUS  CPU     MEM      ALLOCS
  node-1  ready   1.2/4   2.1G/8G  4
  node-2  ready   0.8/4   1.8G/8G  3
  node-3  ready   2.1/2   3.2G/4G  2
```

## Logs

```bash
# Stream logs (follows)
mill logs <name> [-f]

# Last N lines
mill logs <name> -n <count>

# Task instance logs
mill logs <task-id>

# All services, interleaved
mill logs --all
```

## Secrets

```bash
mill secret set <name> <value>
mill secret get <name>
mill secret list
mill secret delete <name>
```

## Volumes

```bash
# List volumes and their locations
mill volume list

# Destroy a persistent volume (fails if in use)
mill volume delete <name>
```

```
$ mill volume list
NAME       STATE     NODE    CLOUD ID
ojd-data   attached  node-2  vol-abc123
```

## Node management

```bash
# List nodes
mill nodes

# Drain a node (reschedule allocs, then mark unschedulable)
mill drain <node-id>

# Remove a drained node
mill remove <node-id>
```

## Global flags

| Flag | Description |
|------|-------------|
| `--address <url>` | API endpoint (default: `http://127.0.0.1:4400`) |
| `--token <token>` | Auth token (or `MILL_TOKEN` env var) |
| `--json` | Output as JSON instead of tables |
