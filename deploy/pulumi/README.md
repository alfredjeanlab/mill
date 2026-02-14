# Deploy Mill with Pulumi

## DigitalOcean

### Prerequisites

Create an API token at https://cloud.digitalocean.com/account/api/tokens

```bash
doctl auth init                # authenticate with your token
doctl compute ssh-key list     # get your SSH key fingerprint

# If you don't have an SSH key on DO yet:
doctl compute ssh-key import mill --public-key-file ~/.ssh/id_ed25519.pub
```

### Deploy

```bash
cd digitalocean
npm install

pulumi config set digitalocean:token --secret
pulumi config set mill-digitalocean:sshKeyFingerprint <fingerprint>

pulumi up
```

The cluster bootstraps automatically via cloud-init (~2 min).
Node 0 runs `mill init`, the rest wait for it and run `mill join`.

Once cloud-init finishes:

```bash
mill status --address <millApi output>
mill deploy -f ../../examples/hello.mill --address <millApi output>
```

### Tear down

```bash
pulumi destroy
```
