# Workflow: Secret Injection

Store an encrypted secret, reference it in config, inject it as an env var.

## Diagram

```mermaid
sequenceDiagram
    participant CLI as mill CLI
    participant API as Primary (API)
    participant Raft as Raft FSM
    participant Node as Secondary
    participant C as Container

    CLI->>API: PUT /v1/secrets/DB_URL
    API->>Raft: SecretSet(DB_URL, encrypted, nonce)
    Raft-->>API: committed + replicated

    CLI->>API: POST /v1/deploy (config with ${secret(DB_URL)})
    API->>API: validate: all referenced secrets exist
    API->>Raft: Deploy(config)
    API->>Node: schedule alloc (RPC)
    Node->>API: resolve ${secret(DB_URL)}
    API-->>Node: plaintext value
    Node->>C: start container with DATABASE_URL=postgres://...
```

## Steps

### 1. Store a secret

```bash
mill secret set DB_URL "postgres://user:pass@db.example.com:5432/myapp"
```

The primary encrypts the value and commits `SecretSet` to Raft. Replicated
encrypted to all nodes.

### 2. Reference in config

```hcl
service "myapp" {
  image = "registry.example.com/myapp:v4"
  port  = 3000
  env {
    DATABASE_URL = "${secret(DB_URL)}"
  }
}
```

On `mill deploy`, every `${secret(...)}` reference is validated before
scheduling. Missing references fail immediately:

```
error: secret "DB_URL" not found (referenced by service "myapp")
```

### 3. Resolution at container start

The secondary resolves secret references and injects plaintext values as env
vars. Secrets are resolved **once** at start — running containers keep old
values if the secret changes.

### 4. Update and redeploy

```bash
mill secret set DB_URL "postgres://user:newpass@db.example.com:5432/myapp"
mill restart myapp
```

New containers get the new value. During a rolling restart, old containers keep
the old value until replaced.

### 5. List, inspect, delete

```bash
mill secret list              # names only, values not shown
mill secret get DB_URL        # retrieve value
mill secret delete myapp.key  # SecretDelete committed to Raft
```

Deleting a referenced secret causes the next `mill deploy` to fail validation.
Running containers are unaffected.

## Key Points

- **Encrypted at rest:** Encrypted before Raft commit; replicated encrypted.
- **Resolved at start:** Plaintext injected at container start, not before.
- **Immutable to running containers:** Containers keep their original values.
- **Validated on deploy:** Deploy fails if any `${secret(...)}` reference is missing.
- **Cluster-wide scope:** Any service or task can reference any secret.
