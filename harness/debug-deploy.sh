#!/bin/bash
# Temporary debug script to diagnose container startup failures
set -uo pipefail

MILL_BIN=/workspace/Developer/mill/target-linux/debug/mill
MILL_DATA=/tmp/mill-debug-test
NS=mill-0

echo "=== Cleaning up stale state ==="
pkill -9 -f "mill.*init" 2>/dev/null || true
sleep 0.3
ip netns exec $NS ip link del mill0 2>/dev/null || true
rm -rf $MILL_DATA
mkdir -p $MILL_DATA

# Clean up any stale containerd containers/tasks
for task in $(ctr -n mill tasks list -q 2>/dev/null || true); do
    ctr -n mill tasks kill "$task" --signal SIGKILL 2>/dev/null || true
    ctr -n mill tasks delete "$task" 2>/dev/null || true
done
for container in $(ctr -n mill containers list -q 2>/dev/null || true); do
    ctr -n mill containers delete "$container" 2>/dev/null || true
done
# Remove stale shim directories
rm -rf /run/containerd/io.containerd.runtime.v2.task/mill/ 2>/dev/null || true
# Remove stale mill-managed snapshots (mill-web-* etc.), preserving image layer snapshots
for snap in $(ctr -n mill snapshots list 2>/dev/null | awk 'NR>1 && /^mill-/ {print $1}' || true); do
    ctr -n mill snapshots remove "$snap" 2>/dev/null || true
done
echo "Containerd cleaned."

echo "=== Starting mill in $NS namespace ==="
ip netns exec $NS \
    env MILL_NETNS_PATH=/var/run/netns/$NS \
    MILL_DEPLOY_TIMEOUT_MS=20000 \
    MILL_HEALTH_TIMEOUT_MS=10000 \
    MILL_SNAPSHOT_THRESHOLD=1 \
    MILL_SNAPSHOT_LOG_KEEP=0 \
    $MILL_BIN --data-dir $MILL_DATA \
    init --token test123 --advertise 172.18.0.10 \
    2> /tmp/mill-stderr.txt &

MILL_PID=$!
echo "Mill PID: $MILL_PID"

# Wait for "To join another node" marker in stderr
for i in $(seq 1 30); do
    if grep -q "To join another node" /tmp/mill-stderr.txt 2>/dev/null; then
        echo "Mill ready after ${i}s"
        break
    fi
    if ! kill -0 $MILL_PID 2>/dev/null; then
        echo "Mill exited early! stderr:"
        cat /tmp/mill-stderr.txt
        exit 1
    fi
    sleep 1
done

# Extract API address: "API:        http://10.99.0.1:4400"
API_ADDR=$(grep -oE 'http://[0-9.]+:[0-9]+' /tmp/mill-stderr.txt | head -1 | sed 's|http://||')
if [ -z "$API_ADDR" ]; then
    echo "Could not extract API address. Stderr:"
    cat /tmp/mill-stderr.txt
    kill $MILL_PID 2>/dev/null
    exit 1
fi
echo "API address: $API_ADDR"

echo ""
echo "=== Testing API connectivity (/v1/status) ==="
ip netns exec $NS curl --max-time 5 "http://$API_ADDR/v1/status" \
    -H "Authorization: Bearer test123" 2>&1 || echo "(status curl error: $?)"
echo ""

echo "=== Starting bundle watcher in background ==="
(
    for i in $(seq 1 50); do
        sleep 0.1
        BUNDLE=/run/containerd/io.containerd.runtime.v2.task/mill/mill-web-0
        if [ -d "$BUNDLE" ]; then
            echo "BUNDLE FOUND at iteration $i"
            ls -la "$BUNDLE/" 2>&1
            echo "rootfs contents:"
            ls -la "$BUNDLE/rootfs/" 2>&1 | head -20
            echo "rootfs/bin exists:"
            ls "$BUNDLE/rootfs/bin/" 2>&1 | head -10
            break
        fi
    done
) &
WATCHER_PID=$!

echo "=== Deploying service to $API_ADDR ==="
cat > /tmp/mill-config.hcl << 'CONF'
service "web" {
  image    = "docker.io/library/busybox:latest"
  command  = ["/bin/sh", "-c", "httpd -f -p $PORT"]
  port     = 8080
  replicas = 1
  cpu      = 0.25
  memory   = "64M"
}
CONF

ip netns exec $NS curl --max-time 30 -X POST "http://$API_ADDR/v1/deploy" \
    -H "Authorization: Bearer test123" \
    -H "Content-Type: text/plain" \
    --data-binary "@/tmp/mill-config.hcl" 2>&1 || echo "(deploy curl error: $?)"
echo ""
echo "(deploy done)"

echo "=== Waiting 5s more ==="
sleep 5

echo "=== Services status ==="
ip netns exec $NS curl --max-time 5 "http://$API_ADDR/v1/services" \
    -H "Authorization: Bearer test123" 2>&1 || echo "(services curl error)"
echo ""

echo "=== Mill stderr (last 100 lines) ==="
tail -100 /tmp/mill-stderr.txt 2>/dev/null

echo ""
echo "=== Containerd tasks ==="
ctr -n mill tasks list 2>/dev/null || echo "(none or error)"

echo ""
echo "=== Containerd containers ==="
ctr -n mill containers list 2>/dev/null || echo "(none)"

kill $MILL_PID 2>/dev/null
echo "Done."
