#!/bin/bash
# netns-teardown.sh — Destroy network namespaces and clean up mill test state.
#
# Usage: netns-teardown.sh [num_nodes]  (default: 3)
#
# Cleans:
#   - Kill any mill processes inside each namespace
#   - Delete WireGuard interfaces (mill0) inside each namespace
#   - Delete namespaces (auto-cleans veth pairs)
#   - Remove host routes for tunnel IPs
#   - Remove per-node data directories
#   - Kill stale containerd containers in the mill namespace
#   - Remove the bridge
set -euo pipefail

N="${1:-3}"

# --- Per-node cleanup ---
for i in $(seq 0 $((N - 1))); do
    NS="mill-$i"
    TUNNEL_IP="10.99.0.$((1 + i))"

    if ip netns list | grep -qw "$NS"; then
        # Kill any mill processes inside the namespace.
        ip netns exec "$NS" pkill -9 -f mill 2>/dev/null || true

        # Delete WireGuard interface inside the namespace.
        ip netns exec "$NS" ip link del mill0 2>/dev/null || true

        # Delete the namespace (auto-deletes veths).
        ip netns del "$NS" 2>/dev/null || true
        echo "  deleted namespace $NS"
    fi

    # Remove host route for tunnel IP.
    ip route del "$TUNNEL_IP/32" 2>/dev/null || true

    # Remove data directory.
    rm -rf "/var/lib/mill-test/node-$i"
done

# --- Containerd cleanup ---
# Kill and remove any mill-* containers left by crashed tests.
if command -v ctr &>/dev/null; then
    for task in $(ctr -n mill tasks list -q 2>/dev/null || true); do
        ctr -n mill tasks kill "$task" --signal SIGKILL 2>/dev/null || true
        ctr -n mill tasks delete "$task" 2>/dev/null || true
    done
    for container in $(ctr -n mill containers list -q 2>/dev/null || true); do
        ctr -n mill containers delete "$container" 2>/dev/null || true
    done
fi

# --- Bridge cleanup ---
if ip link show mill-br0 &>/dev/null; then
    ip link del mill-br0 2>/dev/null || true
    echo "  deleted bridge mill-br0"
fi

echo "netns-teardown: cleanup complete"
