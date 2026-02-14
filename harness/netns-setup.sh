#!/bin/bash
# netns-setup.sh — Create network namespaces for mill smoke tests.
#
# Usage: netns-setup.sh [num_nodes]  (default: 3)
#
# Creates:
#   - mill-br0 bridge at 172.18.0.1/24
#   - N namespaces (mill-0 .. mill-{N-1}) with veth pairs to the bridge
#   - Host routes for tunnel IPs: 10.99.0.{1+i}/32 via 172.18.0.{10+i}
#
# Idempotent: safe to run multiple times.
set -euo pipefail

N="${1:-3}"

# --- Bridge ---
if ! ip link show mill-br0 &>/dev/null; then
    ip link add mill-br0 type bridge
    ip addr add 172.18.0.1/24 dev mill-br0
    ip link set mill-br0 up
fi

# --- Per-node namespaces ---
for i in $(seq 0 $((N - 1))); do
    NS="mill-$i"
    VETH="veth-mill-$i"
    IP="172.18.0.$((10 + i))"
    TUNNEL_IP="10.99.0.$((1 + i))"

    # Create namespace if it doesn't exist.
    if ! ip netns list | grep -qw "$NS"; then
        ip netns add "$NS"
    fi

    # Create veth pair if it doesn't exist.
    if ! ip link show "$VETH" &>/dev/null; then
        ip link add "$VETH" type veth peer name eth0 netns "$NS"
        ip link set "$VETH" master mill-br0
        ip link set "$VETH" up
    fi

    # Configure the namespace side.
    ip netns exec "$NS" ip addr replace "$IP/24" dev eth0
    ip netns exec "$NS" ip link set eth0 up
    ip netns exec "$NS" ip link set lo up
    ip netns exec "$NS" ip route replace default via 172.18.0.1

    # Disable reverse-path filtering (weak host model needed for tunnel IPs).
    ip netns exec "$NS" sysctl -qw net.ipv4.conf.all.rp_filter=0
    ip netns exec "$NS" sysctl -qw net.ipv4.conf.eth0.rp_filter=0

    # Host route so test binary can reach tunnel IPs through the bridge.
    ip route replace "$TUNNEL_IP/32" via "$IP" dev mill-br0

    # Create per-node data directory.
    mkdir -p "/var/lib/mill-test/node-$i"

    echo "  namespace $NS: eth0=$IP, tunnel route $TUNNEL_IP -> $IP"
done

echo "netns-setup: $N namespaces ready on mill-br0"
