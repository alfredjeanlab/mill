#!/bin/bash
# Mill node bootstrap script.
# Used as cloud-init userdata by Terraform and Pulumi templates.
#
# Installs dependencies, then initializes or joins the cluster automatically.
#
# Template variables (injected by Terraform/Pulumi):
#   mill_version      - Mill binary version
#   node_index        - 0 for first node, 1+ for others
#   advertise_ip      - This node's private IP
#   leader_ip         - First node's private IP (used by join nodes)
#   provider          - Cloud provider name (e.g. "digitalocean")
#   provider_token    - Cloud provider API token

set -euo pipefail

MILL_VERSION="${mill_version}"
NODE_INDEX="${node_index}"
ADVERTISE_IP="${advertise_ip}"
LEADER_IP="${leader_ip}"
PROVIDER="${provider}"
PROVIDER_TOKEN="${provider_token}"

export DEBIAN_FRONTEND=noninteractive

# --- Install dependencies ---

echo "==> Installing system dependencies"
apt-get update -qq
apt-get install -y -qq \
  containerd \
  wireguard-tools \
  ca-certificates \
  curl \
  jq

echo "==> Configuring containerd"
mkdir -p /etc/containerd
containerd config default > /etc/containerd/config.toml
systemctl enable containerd
systemctl restart containerd

echo "==> Loading WireGuard kernel module"
modprobe wireguard
echo wireguard >> /etc/modules-load.d/wireguard.conf

echo "==> Installing Mill"
# TODO: replace with real release URL once published
# if [ "$MILL_VERSION" = "latest" ]; then
#   curl -fsSL "https://github.com/example/mill/releases/latest/download/mill-linux-amd64" \
#     -o /usr/local/bin/mill
# else
#   curl -fsSL "https://github.com/example/mill/releases/download/v${MILL_VERSION}/mill-linux-amd64" \
#     -o /usr/local/bin/mill
# fi
# chmod +x /usr/local/bin/mill

echo "==> Creating Mill systemd unit"
cat > /etc/systemd/system/mill.service <<'UNIT'
[Unit]
Description=Mill container orchestrator
After=network-online.target containerd.service
Wants=network-online.target containerd.service

[Service]
ExecStart=/usr/local/bin/mill agent
Restart=on-failure
RestartSec=5
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload

# --- Bootstrap cluster ---

if [ "$NODE_INDEX" = "0" ]; then
  echo "==> Initializing cluster (node 0)"
  mill init \
    --advertise "$ADVERTISE_IP" \
    --provider "$PROVIDER" \
    --provider-token "$PROVIDER_TOKEN"
else
  echo "==> Waiting for leader at $LEADER_IP:4400"
  until curl -sf "http://$LEADER_IP:4400/v1/status" > /dev/null 2>&1; do
    sleep 2
  done

  echo "==> Joining cluster (node $NODE_INDEX)"
  mill join "$LEADER_IP" --advertise "$ADVERTISE_IP"
fi

systemctl enable mill
echo "==> Done."
