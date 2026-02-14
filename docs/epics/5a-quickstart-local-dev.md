# 5a: Quickstart & Local Dev

**Crate:** `mill-cli`, `mill-node`

Mill works, but trying it requires cloud VMs, WireGuard, and
containerd. This epic adds a zero-infrastructure local mode for
kicking the tires, plus diagnostic and reset commands.

## Context

`mill init` requires `--advertise` and assumes WireGuard, DNS, and
a reverse proxy. `PrimaryConfig` already accepts these as `Option`
fields (`dns: Option<Arc<DnsServer>>`, `proxy: Option<Arc<ProxyServer>>`,
`wireguard: Option<Arc<WireGuard>>`), and `SecondaryConfig` has
`mesh_ip: Option<IpAddr>`. The plumbing for "skip network subsystems"
exists — nothing invokes it yet.

The daemon module (`daemon/mod.rs`) has `init`, `join`, `leave`, and
`boot`. Local mode adds a path through `init` that passes `None` for
all network subsystems and skips `start_subsystems` entirely.

## Requirements

### `mill init --local`

Single-command quickstart. No provider, no WireGuard, no multi-node.
Runs a single-node cluster bound to `127.0.0.1`.

Pseudo-code:

```example
fn init_local(data_dir):
    // Persist identity — no provider, no wireguard
    node_id = generate_node_id()       // 8 random bytes → 16 hex chars
    raft_id = 1
    cluster_token = generate_token()   // 16 random bytes → 32 hex chars
    write node.json {
        node_id, raft_id, cluster_token,
        rpc_port: 4400,
        advertise_addr: "127.0.0.1",
        local: true,                   // NEW field — signal for local mode
        provider: null,
        provider_token: null,
        wireguard_subnet: "",          // empty — no mesh
        peers: []
    }

    // Skip WireGuard entirely — bind everything to localhost
    cluster_key = ClusterKey::generate()
    cluster_key.save(data_dir.raft_dir())
    raft = MillRaft::open(raft_id, raft_config, network, cluster_key, data_dir.raft_dir())
    raft.initialize()                  // single-node bootstrap

    // No DNS, no proxy, no port allocator
    // Register this node in Raft state
    raft.propose(Command::NodeRegister { node_id, raft_id, addr: "127.0.0.1", resources })

    // Secondary — mesh_ip: None, containerd on default socket
    secondary, handle = Secondary::new(
        SecondaryConfig { containerd_socket: "/run/containerd/containerd.sock",
                          data_dir, expected_allocs: [], node_id, mesh_ip: None },
        PortAllocator::new()
    )
    spawn secondary.run()

    // Primary — dns: None, proxy: None, wireguard: None
    primary = Primary::start(
        PrimaryConfig { api_addr: "127.0.0.1:4400", auth_token: cluster_token,
                        dns: None, proxy: None, wireguard: None, ... },
        raft, Some(handle), cancel
    )

    print "Local cluster ready."
    print "  API:    http://127.0.0.1:4400"
    print "  Token:  {cluster_token}"
    print "  Deploy: mill deploy -f example.mill"

    wait_for_signal(SIGTERM, SIGINT)
    // Shutdown — no dns/proxy to stop
    primary.stop()
    raft.shutdown()
```

Differences from full `init`:
- No `--advertise`, `--provider`, or `--provider-token` flags
- WireGuard skipped — no tunnel, no `wg` command needed
- DNS server skipped — services resolve to `127.0.0.1`
- Proxy skipped — container ports mapped to host directly
- `node.json` has `local: true` to mark local mode

### Restart in local mode

`boot()` already checks for `node.json` and calls `start_from_existing`.
Add a branch for local mode:

```example
fn start_from_existing(data_dir):
    identity = read node.json

    if identity.local:
        // Skip WireGuard, DNS, proxy — same as init_local
        cluster_key = ClusterKey::load(data_dir.raft_dir())
        raft = MillRaft::open(identity.raft_id, ...)

        secondary, handle = Secondary::new(
            SecondaryConfig { ..., mesh_ip: None,
                              expected_allocs: read_allocs_from(raft) },
            PortAllocator::new()
        )
        spawn secondary.run()   // reconcile runs inside

        primary = Primary::start(
            PrimaryConfig { dns: None, proxy: None, wireguard: None, ... },
            raft, Some(handle), cancel
        )
        wait_for_signal()
        primary.stop()
        raft.shutdown()
        return

    // ... existing full-stack restart path
```

### `mill doctor`

Pre-flight check that validates the host environment. Useful for
debugging failed inits or broken clusters.

Pseudo-code:

```example
fn doctor(data_dir):
    identity = read node.json (if exists)
    is_local = identity and identity.local
    has_cluster = identity is Some

    checks = [
        // Always check
        ("containerd socket",   check_socket_exists("/run/containerd/containerd.sock")),
        ("containerd reachable", check_containerd_ping()),
        ("data dir writable",   check_writable(data_dir)),
        ("port 4400 available", check_port_available(4400)),
        ("disk space > 1GB",    check_disk_space(data_dir)),
    ]

    // Only check WireGuard for non-local clusters
    if not is_local:
        checks += [
            ("wg command exists",    check_command_exists("wg")),
            ("port 51820 available", check_port_available(51820)),
            ("port 80 available",    check_port_available(80)),
            ("port 443 available",   check_port_available(443)),
        ]

    // Only check cluster state if initialized
    if has_cluster:
        checks += [
            ("node.json valid",      check_node_json_parseable(data_dir)),
            ("cluster.key exists",   check_file_exists(data_dir.raft_dir() + "/cluster.key")),
        ]

    passed = 0
    failed = 0
    for (name, result) in checks:
        if result.ok:
            print "  ok  {name}"
            passed += 1
        else:
            print "  FAIL {name}: {result.message}"
            print "       {result.hint}"
            failed += 1

    print ""
    print "{passed} passed, {failed} failed"
    exit(1 if failed > 0)
```

### `mill clean`

Wipe all local state and start fresh. Opposite of init.

Pseudo-code:

```example
fn clean(force, data_dir):
    if not force:
        print "This will delete all Mill state:"
        print "  - Node identity (node.json)"
        print "  - Raft log, snapshots, cluster key"
        print "  - WireGuard keys"
        print "  - TLS certificates"
        print "  - Container logs"
        print "  Running containers will NOT be stopped."
        confirm = prompt "Continue? [y/N]"
        if confirm != "y":
            return

    identity = read node.json (if exists)

    // Tear down WireGuard if not local mode
    if identity and not identity.local:
        wg = WireGuard::from_existing("mill0")
        wg.teardown()

    // Stop daemon if running
    if process_listening_on(4400):
        signal_shutdown()
        wait_for_exit(timeout: 10s)

    // Delete entire state directory
    // Covers: node.json, raft/, wireguard/, logs/, volumes/,
    //         ephemeral/, certs/
    remove_dir_all(data_dir)

    print "All Mill state removed."
    print "  Running containers were left untouched."
    print "  Run 'mill init' or 'mill init --local' to start fresh."
```

## Acceptance criteria

- `mill init --local` starts a cluster with no flags and no infra
- `mill deploy` works immediately after local init
- `curl http://127.0.0.1:<port>` reaches the deployed service
- Kill and restart the process — node recovers in local mode
- `mill doctor` passes on a healthy local cluster
- `mill doctor` fails with actionable hints on a broken environment
- `mill doctor` skips WireGuard/DNS/proxy checks in local mode
- `mill clean` wipes all state; `mill init --local` works again after
- `mill clean` tears down WireGuard in non-local mode, skips in local
- Local mode skips WireGuard, DNS, and proxy entirely

## Schema change

Add one field to `NodeIdentity`:

```rs
NodeIdentity {
    ...existing fields...
    local: bool,         // NEW — default false, true for --local
}
```

Existing `node.json` files without `local` deserialize as `false`
(serde default).

## Implementation

```example
crates/cli/src/
  args.rs            ← add --local flag to InitArgs, add Doctor + Reset subcommands
  commands/
    cluster.rs       ← add init_local() path
    doctor.rs        ← NEW: health checks
    reset.rs         ← NEW: state wipe

crates/node/src/
  daemon/
    mod.rs           ← add local-mode branch in init() and start_from_existing()
    support.rs       ← add NodeIdentity.local field
```

No separate `boot_local()` — the existing `init()` gains an `if local { ... }` branch
that passes `None` for WireGuard, DNS, and proxy.

`PrimaryConfig` and `SecondaryConfig` already accept optional network subsystems.
