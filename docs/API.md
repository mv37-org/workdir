# REST API Reference

Base URL: `https://api.<domain>` (dev: `http://127.0.0.1:8080`).
Auth: `Authorization: Bearer sk_live_...` on every `/v1` route.
All bodies are JSON. Errors are `{"error": {"code", "message", "reason?"}}`.

The default create is one call with no body and yields the cheapest, fastest path.

## Sandboxes

| Method | Path | Notes |
|---|---|---|
| `POST` | `/v1/sandboxes` | Create. Empty body = default cheap path. |
| `GET` | `/v1/sandboxes` | List the caller's sandboxes. |
| `GET` | `/v1/sandboxes/:id` | Get one (with timings, urls, price, uptime, cost). |
| `POST` | `/v1/sandboxes/:id/exec` | `{cmd, cwd?, env?, background?}` → `{exit_code, stdout, stderr}`. |
| `GET` | `/v1/sandboxes/:id/pty` | WebSocket interactive shell. |
| `GET` | `/v1/sandboxes/:id/files?path=…` | Read a file → `{content, encoding}`. |
| `PUT` | `/v1/sandboxes/:id/files` | `{path, content, encoding?}` → write. |
| `POST` | `/v1/sandboxes/:id/ports/:port/expose` | → `{port, url}` preview route. |
| `GET`/`POST` | `/v1/sandboxes/:id/browser` | Browser readiness + VNC/CDP urls. |
| `POST` | `/v1/sandboxes/:id/snapshot` | Snapshot (billed separately). |
| `POST` | `/v1/sandboxes/:id/pause` | Stop (release CPU/mem; keep billing correct). |
| `POST` | `/v1/sandboxes/:id/resume` | Resume from stopped disk/snapshot. |
| `DELETE` | `/v1/sandboxes/:id` | Stop, delete ephemeral disk, remove routes. |

### Create request

```jsonc
// default cheap path
{}

// or "startup": "none" explicitly
{ "startup": "none" }

// heavier path — explicit options required (spec §3.4)
{
  "image": "browser",
  "resources": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 },
  "browser": { "enabled": true, "vnc": true, "cdp": true },
  "auto_stop_seconds": 300,
  "snapshot": false,
  "startup": {
    "git": { "url": "https://github.com/acme/app.git", "ref": "main", "depth": 1 },
    "env": { "NODE_ENV": "development" },
    "secrets": ["OPENAI_API_KEY"],
    "cache": { "package_managers": ["npm", "pnpm", "pip", "uv"] },
    "commands": [
      { "name": "install", "run": "pnpm install --frozen-lockfile", "cache_key": "pnpm-lock.yaml" },
      { "name": "dev", "run": "pnpm dev --host 0.0.0.0", "background": true }
    ],
    "ports": [3000, 6080],
    "ready": { "http": "http://127.0.0.1:3000", "timeout_seconds": 30 },
    "network": { "egress": "default" }
  }
}
```

**Constrained knobs (spec §3.2)** — arbitrary values like 13 GB or 250 GB are
rejected with `400 bad_request`:

| Knob | Allowed | Default |
|---|---|---|
| `cpu` | 0.5, 1, 2, 4 | 1 |
| `memory_mb` | 1024, 2048, 4096, 8192, 16384 | 2048 |
| `disk_gb` | 8, 16, 32, 64 | 8 |
| `image` | base, node-python, browser, heavy-build, custom/… | base |
| `auto_stop_seconds` | 30–3600 | 120 |

### Create / get response

```jsonc
{
  "id": "sbx_…",
  "runtime": "firecracker",
  "image": "base",
  "state": "running",
  "resources": { "cpu": "1 shared vCPU", "memory_mb": 2048, "disk_gb": 8 },
  "node_id": "node_…",
  "boot_path": "hot_pool",          // hot_pool | snapshot_restore | cold_boot
  "boot_ms": 42,
  "browser_ready_ms": 1280,          // present only for browser sandboxes
  "coding_agent": "opencode",        // present only when the coding agent is opted in
  "auto_stop_seconds": 120,
  "timings": { "boot_ms": 42, "image_cache_ms": 0, "git_ms": 0,
               "install_ms": 0, "ready_ms": 0, "total_ms": 43 },
  "urls": { "ports": { "3000": "https://sbx_…-3000.<domain>" },
            "vnc": "https://sbx_…-6080.<domain>",
            "cdp": "https://sbx_…-9222.<domain>" },
  "price": { "resource_units": 1.0, "image_multiplier": 1.0,
             "unit_price_usd_hr": 0.009, "price_usd_hr": 0.009,
             "price_usd_second": 0.0000025 },
  "uptime_seconds": 0,
  "cost_estimate_usd": 0.0
}
```

## Images (spec §10, §11)

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/images` | Curated catalog + the org's custom images. |
| `POST` | `/v1/images` | Build/import **asynchronously** (`202 Accepted`). |
| `GET` | `/v1/images/:id` | Status + `build_log` + cache-miss time. |
| `DELETE` | `/v1/images/:id` | Soft delete: blocks new creates, keeps running sandboxes. |

```jsonc
// POST /v1/images
{
  "source": { "type": "dockerfile",
              "context_url": "https://github.com/acme/app/archive/main.tar.gz",
              "dockerfile": "Dockerfile" },
  "name": "custom/acme/app",
  "resources_hint": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 }
}
// or  "source": { "type": "oci", "image_ref": "ghcr.io/acme/app:1.2.3" }
```

Use a published custom image: `{"image": "custom/acme/app", "image_version": "2026-06-10-ab12cd"}`.

## Secrets (feature)

Org-scoped, encrypted at rest, never returned over the API. See
[FEATURES.md](FEATURES.md#1-secret-management).

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/secrets` | List secret names + timestamps (no values). |
| `PUT` | `/v1/secrets/:name` | `{value}` → store/replace (encrypted). |
| `DELETE` | `/v1/secrets/:name` | Remove a secret. |

Reference secrets in `startup.secrets: ["NAME", ...]`; values are injected into
the sandbox env after assignment. A sandbox with resident secrets cannot be
snapshotted (`409`).

## Extended create options (features)

Added to `POST /v1/sandboxes` (all optional, default-off):

```jsonc
{
  "docker": { "enabled": true },                 // dockerd inside the guest VM (heavy-build/custom image)
  "coding_agent": { "enabled": true },           // install opencode CLI into the guest (opt-in; any image)
  "mounts": [ { "type": "s3", "bucket": "my-data", "mount_path": "/mnt/data",
                "read_only": true, "prefix": "p/", "region": "us-east-1" } ],
  "files":  [ { "path": "config.json", "content": "{}", "encoding": "utf8" } ]
}
```

Custom images accept `"ephemeral": true` + `"ttl_seconds": N` for auto-GC'd
one-off images. Full reference: [FEATURES.md](FEATURES.md).

## Nodes (spec §8)

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/nodes` | Nodes + capacity in default-equivalent units + add-node command. |
| `POST` | `/v1/nodes/join-token` | Admin: mint/rotate a worker join token. |
| `POST` | `/v1/nodes/:id/drain` | Admin: mark unschedulable + draining. |

## Usage, billing, benchmarks

| Method | Path | Notes |
|---|---|---|
| `GET` | `/v1/usage` | Org cost, delivered unit-seconds, prepaid balance, per-sandbox. |
| `GET` | `/v1/admin/overview` | Admin: nodes, hot pools, reconciled at-cost price, abuse alerts. |
| `GET` | `/v1/benchmarks` | Boot timings p50/p95 by image **and boot path** (labeled). |
| `GET` | `/healthz` | Liveness (no auth). |

## Preview proxy (spec §16.2)

Host-routed: `https://<sandbox-id>-<port>.<domain>/…`. HTTP is forwarded;
WebSocket/CDP/VNC upgrades are bridged. Requires a valid API key (header or
`?key=`) belonging to the sandbox's org. A path-based form
`/_preview/<id>/<port>/<rest>` exists for environments without wildcard DNS.

## Error codes

`bad_request` (400), `unauthorized` (401), `forbidden` (403), `not_found` (404),
`conflict` (409), `rejected` (422), `no_capacity` (503, with `reason`),
`internal` (500). `no_capacity.reason` is one of `no_nodes`,
`no_schedulable_nodes`, `no_browser_capable_node`, `memory_admission`,
`remote_placement_unsupported`.
