# Mill: Config Format

Mill uses a single config file per project. No templates, no Helm, no
overlays. Variables are limited to secret references and service addresses.

File extension: `.mill` (e.g., `dev.mill`, `prod.mill`).

## Example

```hcl
service "office" {
  image    = "registry.example.com/office:latest"
  port     = 3000
  replicas = 1
  cpu      = 0.5
  memory   = "512M"

  env {
    DATABASE_URL  = "${secret(office.database-url)}"
    OJ_AUTH_TOKEN = "${secret(ojd.auth-token)}"
    OJ_DAEMON_URL = "tcp://${service.ojd.address}"
  }

  health {
    path     = "/health"
    interval = "10s"
  }

  route "office.example.com" {
    path = "/"
  }
}

service "terminal-proxy" {
  image  = "registry.example.com/terminal-proxy:latest"
  port   = 8080
  cpu    = 0.5
  memory = "256M"

  env {
    OJ_DAEMON_URL = "tcp://${service.ojd.address}"
    OJ_AUTH_TOKEN = "${secret(ojd.auth-token)}"
  }

  route "office.example.com" {
    path      = "/ws"
    websocket = true
  }
}

service "ojd" {
  image  = "registry.example.com/ojd:latest"
  port   = 7777
  cpu    = 1
  memory = "1G"

  volume "ojd-data" {
    path = "/data"
    size = "5Gi"
  }
}

task "coop-agent" {
  image   = "registry.example.com/coop:claude"
  cpu     = 2
  memory  = "4G"
  timeout = "2h"
}

```

## Primitives

### service

Long-running process. Restarted on failure. Has a stable network identity.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image` | string | required | Container image |
| `port` | int | required | Service port |
| `replicas` | int | `1` | Number of instances |
| `cpu` | float | `0.25` | CPU cores (reservation + limit) |
| `memory` | string (size) | `"256M"` | Memory (reservation + limit) |
| `env` | block | `{}` | Environment variables |
| `health` | block | none | HTTP health check |
| `route` | block(s) | none | External ingress routes |
| `volume` | block(s) | none | Named volume mounts (see below) |

### task

Ephemeral process. Spawned via API, runs to completion or timeout.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image` | string | required | Container image |
| `cpu` | float | `0.25` | CPU cores |
| `memory` | string (size) | `"256M"` | Memory |
| `timeout` | string (duration) | `"1h"` | Max runtime before kill |
| `env` | block | `{}` | Default environment (overridable at spawn) |

### volume

Named volume mount on a service. Persistent volumes are created and
managed by Mill via the cloud provider API. Ephemeral volumes are
scratch space on the host.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| (name) | string | required | Volume name (used as cloud volume identifier) |
| `path` | string | required | Mount path inside container |
| `size` | string (size) | required | Volume size (persistent volumes only) |
| `ephemeral` | bool | `false` | If true, scratch space — no cloud provisioning, lost on reschedule |

## Interpolation

Two forms of dynamic reference in string values:

| Syntax | Resolves to | Example |
|--------|-------------|---------|
| `${service.NAME.address}` | Internal address (`name.mill:port`) | `"tcp://${service.ojd.address}"` |
| `${secret(NAME)}` | Secret value from mill's secret store | `"${secret(db.password)}"` |

Both forms can appear inside larger strings and can be mixed.

No conditionals, no loops, no template logic. For environment-specific
config, use separate files: `mill deploy -f staging.mill`.

## Validation

`mill deploy` validates before applying:

- All referenced secrets exist
- All `${service.X.address}` references point to defined services
- Persistent volumes have a `size` specified
- Resource values are within node capacity
- Port numbers are valid
- Health check paths start with `/`
