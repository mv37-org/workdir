# sandboxd — Low-Cost Firecracker microVM Sandbox Provider

A Rust implementation of the *Low-Cost Sandbox Provider* spec: **one configurable
Linux sandbox product**, every sandbox a Firecracker microVM on Hetzner dedicated
servers, optimized for the two public claims — **lowest cost per useful Linux
sandbox-hour** and **fastest create-to-first-command time**.

The cheapest, fastest path is the default:

```
Runtime:    Firecracker microVM
Image:      base (1 vCPU / 2 GB RAM / 8 GB disk)
Startup:    none
Networking: private IP, shared NAT, wildcard preview proxy
Lifecycle:  auto-stop after 120 s idle
Target:     Hetzner dedicated root servers
```

```js
const sandbox = await client.sandboxes.create();   // 1 vCPU / 2 GB / 8 GB, ~$0.009/hr
```

## What this repo contains

| Component | Where | Status |
|---|---|---|
| Control plane API (all of spec §19) | `crates/sandboxd/src/api` | ✅ working |
| Constrained resource knobs, pricing, capacity, lifecycle | `crates/sandboxd/src/{knobs,pricing,capacity,lifecycle}.rs` | ✅ working |
| Placement scheduler (§15 scoring + admission) | `crates/sandboxd/src/scheduler.rs` | ✅ working |
| Hot pools, idle auto-stop, billing, usage | `src/{hotpool,background,usage}.rs` | ✅ working |
| Curated image catalog + async custom image build (§10, §11) | `src/{catalog,images}.rs`, `api/images.rs` | ✅ working |
| Preview / VNC / CDP proxy (§16.2) | `src/api/preview.rs` | ✅ working (HTTP + WS) |
| Node registry, join token, drain, multi-node scheduling (§8) | `src/{nodes,node}.rs`, `api/nodes.rs` | ✅ working |
| **Dev runtime** (mock, runs on any OS, real exec/files/preview) | `src/runtime/mock.rs` | ✅ working |
| **Production runtime** (Firecracker + jailer + vsock guest agent) | `src/runtime/firecracker.rs`, `crates/guest-agent` | ⚙️ implemented; requires a Linux + `/dev/kvm` host to run |
| Installer, systemd, nftables (§7.3, §16, §18) | `deploy/` | ✅ |
| Python + TypeScript SDKs (§20) | `sdk/` | ✅ |
| **Secret management** (AES-GCM at rest, late injection, never snapshotted) | `src/secrets.rs`, `api/secrets.rs` | ✅ working |
| **Docker-in-docker** (dockerd inside the guest VM) | feature, `runtime/firecracker.rs` | ⚙️ runs on Firecracker w/ docker-capable image |
| **S3 bucket mounts** (mountpoint-s3 in-guest) | feature | ⚙️ runs on Firecracker w/ `mount-s3` image |
| **Ephemeral files + images** (inline files; TTL'd auto-GC images) | feature, `background.rs` | ✅ working |

See [docs/FEATURES.md](docs/FEATURES.md) for the four extended features and
[docs/REVIEW.md](docs/REVIEW.md) for the code-review findings and fixes.

> **Why two runtimes?** Firecracker needs `/dev/kvm`, which only exists on a
> Linux KVM host (and on Hetzner that means a *dedicated* server — Hetzner Cloud
> has no nested virtualization, spec §7.2). The `Runtime` trait lets the entire
> control plane, scheduler, pricing, and API run and be tested anywhere via the
> `mock` runtime, then drive real microVMs unchanged on a Hetzner node via the
> `firecracker` runtime. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Quick start (developer machine — no KVM needed)

```bash
cargo build --release
SANDBOXD_DATA_DIR=/tmp/sandboxd \
SANDBOXD_RUNTIME=mock \
SANDBOXD_ALLOW_INSECURE_RUNTIME=1 \
SANDBOXD_PUBLIC_DOMAIN=sandboxes.local \
SANDBOXD_ADMIN_KEY=sk_live_dev \
  ./target/release/sandboxd serve
```

> The `mock` runtime executes code on the host with **no isolation** and refuses
> to start without `SANDBOXD_ALLOW_INSECURE_RUNTIME=1`. It is for local dev only;
> production uses `firecracker` (the default on Linux).

In another shell:

```bash
KEY=sk_live_dev; B=http://127.0.0.1:8080
# default cheap path
curl -s -X POST $B/v1/sandboxes -H "Authorization: Bearer $KEY"
# exec
ID=...   # from the response
curl -s -X POST $B/v1/sandboxes/$ID/exec -H "Authorization: Bearer $KEY" \
  -H 'Content-Type: application/json' -d '{"cmd":"echo ok"}'
```

Or with the Python SDK:

```bash
SANDBOXD_URL=http://127.0.0.1:8080 SANDBOXD_KEY=sk_live_dev \
  python3 sdk/python/sandbox_sdk.py
```

## Deploy to Hetzner

See **[docs/DEPLOY.md](docs/DEPLOY.md)** for the full guide. Short version:

```bash
# on a Hetzner EX44-class dedicated server (Ubuntu 24.04 / Debian 12, /dev/kvm present)
curl -fsSL https://deploy.example.com/install.sh | sudo bash -s -- \
  --role all-in-one --domain sandboxes.example.com
```

Add capacity one node at a time:

```bash
# control plane:  POST /v1/nodes/join-token  -> <token>
curl -fsSL https://deploy.example.com/install.sh | sudo bash -s -- \
  --role worker --control-plane https://api.sandboxes.example.com --join-token <token>
```

## Tests

```bash
cargo test            # unit (domain, scheduler, pricing, auth) + integration (API end-to-end)
```

## Docs

- [SETUP.md](SETUP.md) — build & run (dev / LAN / production), config, secrets
- [docs/DEPLOY.md](docs/DEPLOY.md) — Hetzner deployment, scaling, draining, ops playbooks
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — components, runtime abstraction, multi-node design
- [docs/API.md](docs/API.md) — REST API reference

## License

Apache-2.0.
