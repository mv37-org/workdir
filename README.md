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

### More capabilities

- **Browser automation** — `image: "browser"` gives you Chromium + Playwright with noVNC and CDP URLs once the browser rootfs is built on the node.
- **Secrets** — store org secrets (encrypted at rest, AES-256-GCM), reference them by name; they're injected at runtime and never land in a snapshot.
- **Docker-in-Docker** — `docker: { enabled: true }` runs `dockerd` *inside* a docker-capable microVM image (never the host socket).
- **S3 mounts** — mount a bucket into the sandbox via `mountpoint-s3` when the guest image includes the mount helper.
- **Ephemeral files & images** — drop inline files into a sandbox at boot, or build throwaway images that auto-expire.

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

The control plane, SDKs, mock runtime, scheduler, lifecycle, billing, secrets, preview proxy, and Firecracker runtime are implemented and covered by the test suite. Self-hosted production use requires a KVM Linux host plus built/staged guest kernel and rootfs artifacts for the image families you enable. Before exposing a public multi-tenant deployment, review the deferred hardening items in [docs/REVIEW.md](docs/REVIEW.md). Internally the crate is named `sandboxd`; the shipped binary and CLI are `workdir`.

## Contributing

Issues and PRs welcome. See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, checks, and contribution expectations. Report security issues privately via [SECURITY.md](SECURITY.md).

## License

workdir is open source under the **[GNU AGPL-3.0](LICENSE)**.

You can self-host it, modify it, and run it for yourself or your company. The AGPL's network-use clause means that if you offer workdir to others as a hosted service, you must release your source. A **commercial license** (for proprietary/embedded use without AGPL obligations) and the **managed cloud** are available at **[workdir.dev](https://workdir.dev)**.

<sub>Built on <a href="https://firecracker-microvm.github.io/">Firecracker</a>. Not affiliated with AWS.</sub>
