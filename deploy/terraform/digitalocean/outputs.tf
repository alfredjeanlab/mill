output "node_ips" {
  description = "Public IP addresses of all nodes"
  value = concat(
    [digitalocean_droplet.leader.ipv4_address],
    digitalocean_droplet.node[*].ipv4_address,
  )
}

output "node_private_ips" {
  description = "Private (VPC) IP addresses of all nodes"
  value = concat(
    [digitalocean_droplet.leader.ipv4_address_private],
    digitalocean_droplet.node[*].ipv4_address_private,
  )
}

output "mill_api" {
  description = "Mill API endpoint"
  value       = "http://${digitalocean_droplet.leader.ipv4_address}:4400"
}

output "next_steps" {
  description = "What to do after apply"
  value       = <<-EOT

    Cluster is bootstrapping via cloud-init (~2 min).

    Wait for it:
      ssh root@${digitalocean_droplet.leader.ipv4_address} "cloud-init status --wait"

    Verify:
      mill status --address http://${digitalocean_droplet.leader.ipv4_address}:4400

    Deploy:
      mill deploy -f examples/hello.mill --address http://${digitalocean_droplet.leader.ipv4_address}:4400

  EOT
}
