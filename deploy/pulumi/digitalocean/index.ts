import * as fs from "node:fs";
import * as path from "node:path";
import * as digitalocean from "@pulumi/digitalocean";
import * as pulumi from "@pulumi/pulumi";

const config = new pulumi.Config();

const clusterName = config.get("clusterName") || "mill";
const region = (config.get("region") || "nyc1") as digitalocean.Region;
const nodeCount = config.getNumber("nodeCount") || 3;
const nodeSize = config.get("nodeSize") || "s-2vcpu-4gb";
const sshKeyFingerprint = config.require("sshKeyFingerprint");
const millVersion = config.get("millVersion") || "latest";
const doToken = new pulumi.Config("digitalocean").requireSecret("token");

// --- Install script ---

const installScriptTemplate = fs.readFileSync(
  path.join(__dirname, "../../scripts/install.sh"),
  "utf-8",
);

function renderInstallScript(vars: Record<string, pulumi.Input<string>>): pulumi.Output<string> {
  return pulumi.all(Object.values(vars)).apply((vals) => {
    let script = installScriptTemplate;
    const keys = Object.keys(vars);
    for (let i = 0; i < keys.length; i++) {
      script = script.replaceAll(`\${${keys[i]}}`, vals[i]);
    }
    return script;
  });
}

// --- VPC ---

const vpc = new digitalocean.Vpc(`${clusterName}-vpc`, {
  name: `${clusterName}-vpc`,
  region,
  ipRange: "10.10.0.0/16",
});

// --- Droplets ---

// Node 0 is created first so its private IP is available for join nodes.
const leader = new digitalocean.Droplet(`${clusterName}-node-1`, {
  name: `${clusterName}-node-1`,
  region,
  size: nodeSize as digitalocean.DropletSlug,
  image: "ubuntu-24-04-x64",
  vpcUuid: vpc.id,
  sshKeys: [sshKeyFingerprint],
  userData: renderInstallScript({
    mill_version: millVersion,
    node_index: "0",
    advertise_ip: "$(hostname -I | awk '{print $1}')",
    leader_ip: "",
    provider: "digitalocean",
    provider_token: doToken,
  }),
  tags: [`${clusterName}-node`],
});

const joinNodes: digitalocean.Droplet[] = [];
for (let i = 1; i < nodeCount; i++) {
  const node = new digitalocean.Droplet(`${clusterName}-node-${i + 1}`, {
    name: `${clusterName}-node-${i + 1}`,
    region,
    size: nodeSize as digitalocean.DropletSlug,
    image: "ubuntu-24-04-x64",
    vpcUuid: vpc.id,
    sshKeys: [sshKeyFingerprint],
    userData: renderInstallScript({
      mill_version: millVersion,
      node_index: String(i),
      advertise_ip: "$(hostname -I | awk '{print $1}')",
      leader_ip: leader.ipv4AddressPrivate,
      provider: "digitalocean",
      provider_token: doToken,
    }),
    tags: [`${clusterName}-node`],
  });
  joinNodes.push(node);
}

// --- Firewall ---

const allNodes = [leader, ...joinNodes];

new digitalocean.Firewall(`${clusterName}-fw`, {
  name: `${clusterName}-fw`,
  dropletIds: allNodes.map((n) => n.id.apply(parseInt)),
  inboundRules: [
    { protocol: "tcp", portRange: "22", sourceAddresses: ["0.0.0.0/0", "::/0"] },
    { protocol: "tcp", portRange: "4400", sourceAddresses: ["0.0.0.0/0", "::/0"] },
    { protocol: "tcp", portRange: "80", sourceAddresses: ["0.0.0.0/0", "::/0"] },
    { protocol: "tcp", portRange: "443", sourceAddresses: ["0.0.0.0/0", "::/0"] },
    { protocol: "udp", portRange: "51820", sourceTags: [`${clusterName}-node`] },
  ],
  outboundRules: [
    { protocol: "tcp", portRange: "1-65535", destinationAddresses: ["0.0.0.0/0", "::/0"] },
    { protocol: "udp", portRange: "1-65535", destinationAddresses: ["0.0.0.0/0", "::/0"] },
  ],
});

// --- Outputs ---

export const nodeIps = allNodes.map((n) => n.ipv4Address);
export const nodePrivateIps = allNodes.map((n) => n.ipv4AddressPrivate);
export const millApi = pulumi.interpolate`http://${leader.ipv4Address}:4400`;

export const nextSteps = pulumi.interpolate`
Cluster is bootstrapping via cloud-init (~2 min).

Wait for it:
  ssh root@${leader.ipv4Address} "cloud-init status --wait"

Verify:
  mill status --address http://${leader.ipv4Address}:4400

Deploy:
  mill deploy -f examples/hello.mill --address http://${leader.ipv4Address}:4400
`;
