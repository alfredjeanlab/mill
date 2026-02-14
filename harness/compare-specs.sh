#!/bin/bash
# Compare OCI specs between ctr --with-ns and mill container
ctr -n mill run \
    --snapshotter overlayfs \
    --with-ns "network:/var/run/netns/mill-0" \
    docker.io/library/busybox:latest \
    test-ctr-spec \
    sleep 60 &
sleep 2

echo "=== CTR spec (full) ==="
cat /run/containerd/io.containerd.runtime.v2.task/mill/test-ctr-spec/config.json | python3 -m json.tool 2>/dev/null

echo ""
echo "=== CTR rootfs contents ==="
ls -la /run/containerd/io.containerd.runtime.v2.task/mill/test-ctr-spec/rootfs/ 2>/dev/null

echo ""
echo "=== MILL spec (full) ==="
cat /run/containerd/io.containerd.runtime.v2.task/mill/mill-web-0/config.json | python3 -m json.tool 2>/dev/null || echo "(mill spec not found)"

echo ""
echo "=== MILL rootfs contents ==="
ls -la /run/containerd/io.containerd.runtime.v2.task/mill/mill-web-0/rootfs/ 2>/dev/null || echo "(mill rootfs not found)"

ctr -n mill tasks kill test-ctr-spec --signal SIGKILL 2>/dev/null
wait
ctr -n mill tasks delete test-ctr-spec 2>/dev/null
ctr -n mill containers delete test-ctr-spec 2>/dev/null
echo "Done."
