<div align="center">

# workdir

**Run untrusted code in fast, cheap, isolated Linux microVMs.**

Every sandbox is a [Firecracker](https://firecracker-microvm.github.io/) microVM that boots in tens of milliseconds. One small binary gives you the API, scheduler, billing, and host agent — self-host it on a single server, or use the managed cloud at [workdir.dev](https://workdir.dev).

[![License: AGPL v3](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)
[![CI](https://github.com/mv37-org/workdir/actions/workflows/ci.yml/badge.svg)](https://github.com/mv37-org/workdir/actions/workflows/ci.yml)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org/)
[![Cloud](https://img.shields.io/badge/cloud-workdir.dev-black.svg)](https://workdir.dev)
[![npm](https://img.shields.io/npm/v/%40mv37%2Fworkdir?label=npm)](https://www.npmjs.com/package/@mv37/workdir)
[![PyPI](https://img.shields.io/pypi/v/mv37-workdir?label=pypi)](https://pypi.org/project/mv37-workdir/)

</div>

---

```ts
import { Client } from "@mv37/workdir";

const workdir = new Client("https://api.workdir.dev", process.env.WORKDIR_API_KEY);

const box = await workdir.sandboxes.create();      // 1 vCPU / 2 GB, boots in ~40ms
const { stdout } = await box.exec("echo hello");   // → "hello"
await box.delete();
```

That's the whole idea: a real Linux box for an AI agent, a CI job, or an app preview — created in milliseconds, billed by the second, and thrown away when you're done.

## Why workdir

- **Fast.** Curated images are kept in warm pools, so a default sandbox is ready before most APIs finish their TLS handshake (<50ms p50 to first command).
- **Cheap.** It runs on plain dedicated servers (think €44/mo Hetzner boxes), packs sandboxes by memory, and bills per second. The default shape targets ~$0.009/sandbox-hour.
- **Isolated.** Each sandbox is its own Firecracker microVM under the jailer — not a shared container. Run code you don't trust.
- **Honest.** Every create tells you its boot path (`hot_pool` / `snapshot_restore` / `cold_boot`) and a full timing breakdown. No hiding cold starts behind warm-pool numbers.
- **One binary.** Control plane, scheduler, host agent, image builder, and preview proxy ship as a single `workdir` binary. Start with one server; add nodes one command at a time.

## Install

### Use the cloud

The fastest way to try it — no infra. Get a key at **[workdir.dev](https://workdir.dev)** and point the SDK at `https://api.workdir.dev`.

### Self-host (one server)

On a KVM-capable Linux box (a Hetzner **dedicated** server, Ubuntu 24.04 / Debian 12):

```bash
curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
  --role all-in-one --domain sandboxes.example.com
```

The installer runs preflight checks (and fails clearly if `/dev/kvm` is missing), installs Firecracker + the systemd service + firewall rules, and prints your admin key once. Operators still need to build or stage the guest kernel/rootfs artifacts for the image families they want to run. Full guide: **[docs/DEPLOY.md](docs/DEPLOY.md)**.

### Run it locally (no KVM, any OS)

For development you can run the whole product on a Mac or any Linux box using the `mock` runtime — same API, no real VMs:

```bash
git clone git@github.com:mv37-org/workdir.git && cd workdir
cargo build --release
bash examples/serve_lan.sh          # serves on your LAN, prints URL + admin key
```

> The `mock` runtime executes code on the host with **no isolation** and refuses to start without `WORKDIR_ALLOW_INSECURE_RUNTIME=1`. It's for development only — production uses Firecracker. See [SETUP.md](SETUP.md).

## Usage

### Default sandbox

```python
# pip install mv37-workdir
from workdir import Client

wd = Client("https://api.workdir.dev", api_key="...")
box = wd.sandboxes.create()
print(box.exec("python3 -c 'print(2+2)'").stdout)   # "4"
box.delete()
```

### A real workload — clone, install, preview

```ts
const box = await workdir.sandboxes.create({
  resources: { cpu: 2, memoryMb: 4096, diskGb: 16 },
  startup: {
    git: { url: "https://github.com/acme/app.git", ref: "main" },
    commands: [
      { name: "install", run: "pnpm install --frozen-lockfile" },
      { name: "dev", run: "pnpm dev --host 0.0.0.0", background: true },
    ],
    ports: [3000],
    ready: { http: "http://127.0.0.1:3000", timeout_seconds: 30 },
  },
});

console.log(box.urls.ports["3000"]);   // public preview URL, served through an authenticated proxy
```

### Feature examples

All snippets assume a client has already been created:

```ts
import { Client } from "@mv37/workdir";
const wd = new Client("https://api.workdir.dev", process.env.WORKDIR_API_KEY!);
```

```python
from workdir import Client
wd = Client("https://api.workdir.dev", api_key="sk_live_...")
```

#### Browser automation

```ts
const box = await wd.sandboxes.create({
  image: "browser",
  resources: { cpu: 2, memoryMb: 4096, diskGb: 16 },
  browser: { enabled: true, vnc: true, cdp: true },
});
console.log(box.urls.vnc, box.urls.cdp);
console.log(await box.browser());
```

```python
box = wd.sandboxes.create(
    image="browser",
    resources={"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
    browser={"enabled": True, "vnc": True, "cdp": True},
)
print(box.urls["vnc"], box.urls["cdp"])
print(box.browser())
```

#### Secrets and coding agents

```ts
await wd.secrets.set("ANTHROPIC_API_KEY", process.env.ANTHROPIC_API_KEY!);
const box = await wd.sandboxes.create({
  codingAgent: { enabled: true },
  startup: { secrets: ["ANTHROPIC_API_KEY"] },
});
await box.exec("opencode run 'add a regression test'");
```

```python
wd.secrets.set("ANTHROPIC_API_KEY", "sk-ant-...")
box = wd.sandboxes.create(
    coding_agent={"enabled": True},
    startup={"secrets": ["ANTHROPIC_API_KEY"]},
)
box.exec("opencode run 'add a regression test'")
```

#### Network egress policy

```ts
const box = await wd.sandboxes.create({
  startup: {
    network: {
      egress: "allowlist",
      allow: [
        { type: "domain", value: "api.openai.com", protocol: "tcp", ports: [443] },
        "93.184.216.34",
      ],
    },
  },
});
console.log(box.network);
```

```python
box = wd.sandboxes.create(
    startup={
        "network": {
            "egress": "denylist",
            "deny": [{"type": "domain", "value": "*.example.net", "protocol": "tcp", "ports": [443]}],
        }
    }
)
print(box.network)
```

#### Docker-in-Docker

```ts
const box = await wd.sandboxes.create({
  image: "custom/acme/dind",
  resources: { cpu: 2, memoryMb: 4096, diskGb: 16 },
  docker: { enabled: true },
});
console.log((await box.exec("docker run --rm hello-world")).stdout);
```

```python
box = wd.sandboxes.create(
    image="custom/acme/dind",
    resources={"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
    docker={"enabled": True},
)
print(box.exec("docker run --rm hello-world").stdout)
```

#### S3 mounts and inline files

```ts
await wd.secrets.set("AWS_ACCESS_KEY_ID", process.env.AWS_ACCESS_KEY_ID!);
await wd.secrets.set("AWS_SECRET_ACCESS_KEY", process.env.AWS_SECRET_ACCESS_KEY!);
const box = await wd.sandboxes.create({
  startup: { secrets: ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] },
  mounts: [{ type: "s3", bucket: "my-data", prefix: "datasets/", mount_path: "/mnt/data", read_only: true }],
  files: [{ path: "config.json", content: JSON.stringify({ task: "train" }) }],
});
```

```python
wd.secrets.set("AWS_ACCESS_KEY_ID", "...")
wd.secrets.set("AWS_SECRET_ACCESS_KEY", "...")
box = wd.sandboxes.create(
    startup={"secrets": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"]},
    mounts=[{"type": "s3", "bucket": "my-data", "prefix": "datasets/", "mount_path": "/mnt/data", "read_only": True}],
    files=[{"path": "config.json", "content": '{"task":"train"}'}],
)
```

#### Persistent volumes

```ts
const volume = await wd.volumes.create("project-cache", 20);
const box = await wd.sandboxes.create({
  volumes: [{ volume_id: volume.id, mount_path: "/mnt/project" }],
});
await box.exec("echo cached > /mnt/project/state.txt");
await box.delete(); // volume remains
```

```python
volume = wd.volumes.create("project-cache", 20)
box = wd.sandboxes.create(volumes=[{"volume_id": volume["id"], "mount_path": "/mnt/project"}])
box.exec("echo cached > /mnt/project/state.txt")
box.delete()  # volume remains
```

#### Custom and ephemeral images

```ts
const image = await wd.images.create(
  "custom/acme/app",
  { type: "dockerfile", context_url: "https://github.com/acme/app/archive/main.tar.gz", dockerfile: "Dockerfile" },
  { cpu: 2, memory_mb: 4096, disk_gb: 16 },
);
const throwaway = await wd.images.create("custom/acme/one-shot", {
  type: "oci",
  image_ref: "ghcr.io/acme/app:sha",
}, undefined, { ephemeral: true, ttl_seconds: 3600 });
```

```python
image = wd.images.create(
    "custom/acme/app",
    {"type": "dockerfile", "context_url": "https://github.com/acme/app/archive/main.tar.gz", "dockerfile": "Dockerfile"},
    {"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
)
throwaway = wd.images.create(
    "custom/acme/one-shot",
    {"type": "oci", "image_ref": "ghcr.io/acme/app:sha"},
    ephemeral=True,
    ttl_seconds=3600,
)
```

#### Lifecycle, fork, metrics, and PTY

```ts
await box.pause();
await box.resume();
const child = await box.fork();
console.log(await child.metrics());

import WebSocket from "ws";
const pty = new WebSocket(
  `wss://api.workdir.dev/v1/sandboxes/${box.id}/pty`,
  { headers: { Authorization: `Bearer ${process.env.WORKDIR_API_KEY}` } },
);
```

```python
box.pause()
box.resume()
child = box.fork()
print(child.metrics())

# PTY is a WebSocket endpoint:
# wss://api.workdir.dev/v1/sandboxes/<sandbox-id>/pty
# Send Authorization: Bearer sk_live_... from your WebSocket client.
```

Full reference: **[docs/FEATURES.md](docs/FEATURES.md)** and **[docs/API.md](docs/API.md)**.

## SDKs

| Language | Install | Source |
|---|---|---|
| TypeScript / JS | `npm i @mv37/workdir` | [sdk/typescript](sdk/typescript/src/index.ts) |
| Python | `pip install mv37-workdir` | [sdk/python](sdk/python/src/workdir/__init__.py) |

Or just use the REST API directly — it's small and documented in [docs/API.md](docs/API.md).

## How it works

```
   SDK / REST API
        │
   Control plane ── scheduler · billing · image registry · node registry · preview proxy
        │
   Data plane ───── Firecracker microVMs (jailer, vsock guest agent, NAT, hot pools)
```

The control plane is platform-independent and talks to the data plane through a `Runtime` trait. In production that's **Firecracker** on a Linux KVM host; in development it's a **mock** runtime that runs anywhere, so the entire API and SDK surface is testable without a VM. Deep dive: **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

## Documentation

- **[SETUP.md](SETUP.md)** — build & run (dev / LAN / production), config, secrets
- **[docs/DEPLOY.md](docs/DEPLOY.md)** — deploy on Hetzner, scale, drain, ops playbooks
- **[docs/RUNBOOK.md](docs/RUNBOOK.md)** — provision a node, build images, networking, tunnel, disaster recovery
- **[docs/API.md](docs/API.md)** — REST API reference
- **[docs/FEATURES.md](docs/FEATURES.md)** — secrets, docker-in-docker, S3 mounts, ephemeral files/images
- **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — components, runtime abstraction, multi-node design

## Status

The control plane, SDKs, mock runtime, scheduler, lifecycle, billing, secrets, preview proxy, PTY, persistent volumes, and Firecracker runtime are implemented and covered by the test suite. Self-hosted production use requires a KVM Linux host plus built/staged guest kernel and rootfs artifacts for the image families you enable. Before exposing a public multi-tenant deployment, review the deferred hardening items in [docs/REVIEW.md](docs/REVIEW.md). Internally the crate is named `sandboxd`; the shipped binary and CLI are `workdir`.

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks, and contribution expectations. Report security issues privately via [SECURITY.md](SECURITY.md).

## License

workdir is open source under the **[GNU AGPL-3.0](LICENSE)**.

You can self-host it, modify it, and run it for yourself or your company. The AGPL's network-use clause means that if you offer workdir to others as a hosted service, you must release your source. A **commercial license** (for proprietary/embedded use without AGPL obligations) and the **managed cloud** are available at **[workdir.dev](https://workdir.dev)**.

<sub>Built on <a href="https://firecracker-microvm.github.io/">Firecracker</a>. Not affiliated with AWS.</sub>
