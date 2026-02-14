## Overview

Mill is a container orchestrator for small-to-medium deployments.
Single binary, single config format, batteries included.

Two primitives: **services** (long-running) and **tasks** (ephemeral).

## Directory Structure

```toc
crates/
  config/       → mill-config       Config parser, core types, validation
  raft/         → mill-raft         Raft consensus, FSM, secret storage
  containerd/   → mill-containerd   containerd gRPC client, container lifecycle
  net/          → mill-net          WireGuard mesh, DNS server, port allocator
  proxy/        → mill-proxy        Reverse proxy, ACME/TLS, route table
  node/         → mill-node         Primary + secondary runtime, RPC, API server
  cli/          → mill              CLI client
```

```toc
docs/
  arch.md      Architecture, component diagram, design decisions
  config.md    .mill config format reference
  cli.md       CLI commands and flags
  api.md       HTTP API endpoints
harness/       → mill-harness   Lima VM test environment for e2e smoke tests
tests/smoke/   → smoke          E2e smoke tests (real binaries, real containers)
```

## Development

```sh
make check      # fmt + clippy + build + test
make ci         # full pre-release checks
make fmt        # format code
make smoke      # e2e smoke tests (fully automated: VM, build, namespaces, teardown)
```

- `make smoke` is self-contained — just run it. The harness manages the Lima VM, cross-compiles the binary, sets up network namespaces, runs the tests, and tears down automatically.
- Use `MILL_TEST_FILTER=test_name make smoke` to run a single smoke test.

## Shell Commands

- Always set a `timeout` on Bash tool calls; never run a command without one unless it is explicitly backgrounded
- Long-running diagnostics (e.g. `limactl shell`, `cargo test`, harness scripts) must use `run_in_background` or a conservative timeout (≤ 120 s) so they cannot hang the session

## Conventions

- Rust, async with tokio
- Functional core / imperative shell where possible (FSM is pure, no I/O)
- One implementation per subsystem (no plugin abstractions)
- Use native async traits, not the `async_trait` crate
- Types shared via `mill-config`; all crates import from it
- All timeout and sleep durations in `mill-node` must be defined in `crates/node/src/env.rs`; never hardcode durations inline
- Use conventional commit format: `type(scope): description`
  Types: feat, fix, chore, docs, test, refactor
- Never use `sleep` in tests; use `mill_support::poll` to wait on conditions
- Use `yare::parameterized` for table-driven / parameterized tests

## Test Helpers (`crates/support/` → `mill-support`)

- `poll(|| cond)` — async poll-wait with `.secs(n)` timeout and `.expect(msg)` builders
- `Api::new(addr, token)` — HTTP client for all Mill API endpoints (status, nodes, services, tasks, volumes, deploy, drain, secrets, join)
- `TestCluster::new(n)` — N-node harness with real Raft, RPC servers, simulated secondaries, and a Primary on the leader
- `TestSecondary::start(rx, tx, id, res)` — channel-driven fake secondary; supports `stop_heartbeats()`, `resume_heartbeats()`, `fail_next_run(reason)`
- `TestVolumes::new()` — in-memory `Volumes` impl with `attached_to(name)` and `exists(name)` assertions

## Landing the Plane

Before committing changes:

- [ ] Run `make check`
  - `cargo fmt --all`
  - `cargo clippy --all -- -D warnings`
  - `quench check --fix`
  - `cargo build --all`
  - `cargo test --all`
