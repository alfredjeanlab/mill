terraform {
  required_providers {
    digitalocean = {
      source  = "digitalocean/digitalocean"
      version = "~> 2.0"
    }
  }
}

provider "digitalocean" {
  token = var.do_token
}

# --- VPC ---

resource "digitalocean_vpc" "mill" {
  name     = "${var.cluster_name}-vpc"
  region   = var.region
  ip_range = "10.10.0.0/16"
}

# --- Droplets ---

# Node 0 is created first so its private IP is available for join nodes.
resource "digitalocean_droplet" "leader" {
  name     = "${var.cluster_name}-node-1"
  region   = var.region
  size     = var.node_size
  image    = "ubuntu-24-04-x64"
  vpc_uuid = digitalocean_vpc.mill.id
  ssh_keys = [var.ssh_key_fingerprint]

  user_data = templatefile("${path.module}/../../scripts/install.sh", {
    mill_version   = var.mill_version
    node_index     = 0
    advertise_ip   = self.ipv4_address_private
    leader_ip      = ""  # not used by node 0
    provider       = "digitalocean"
    provider_token = var.do_token
  })

  tags = ["${var.cluster_name}-node"]
}

resource "digitalocean_droplet" "node" {
  count    = var.node_count - 1
  name     = "${var.cluster_name}-node-${count.index + 2}"
  region   = var.region
  size     = var.node_size
  image    = "ubuntu-24-04-x64"
  vpc_uuid = digitalocean_vpc.mill.id
  ssh_keys = [var.ssh_key_fingerprint]

  user_data = templatefile("${path.module}/../../scripts/install.sh", {
    mill_version   = var.mill_version
    node_index     = count.index + 1
    advertise_ip   = self.ipv4_address_private
    leader_ip      = digitalocean_droplet.leader.ipv4_address_private
    provider       = "digitalocean"
    provider_token = var.do_token
  })

  tags = ["${var.cluster_name}-node"]
}

# --- Firewall ---

resource "digitalocean_firewall" "mill" {
  name = "${var.cluster_name}-fw"
  droplet_ids = concat(
    [digitalocean_droplet.leader.id],
    digitalocean_droplet.node[*].id,
  )

  # SSH
  inbound_rule {
    protocol         = "tcp"
    port_range       = "22"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  # Mill API
  inbound_rule {
    protocol         = "tcp"
    port_range       = "4400"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  # HTTP + HTTPS (ingress proxy)
  inbound_rule {
    protocol         = "tcp"
    port_range       = "80"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  inbound_rule {
    protocol         = "tcp"
    port_range       = "443"
    source_addresses = ["0.0.0.0/0", "::/0"]
  }

  # WireGuard (node-to-node, VPC only)
  inbound_rule {
    protocol         = "udp"
    port_range       = "51820"
    source_tags      = ["${var.cluster_name}-node"]
  }

  # All outbound
  outbound_rule {
    protocol              = "tcp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }

  outbound_rule {
    protocol              = "udp"
    port_range            = "1-65535"
    destination_addresses = ["0.0.0.0/0", "::/0"]
  }
}
