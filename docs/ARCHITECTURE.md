# Architecture

sandboxd is one binary that can run as the **all-in-one node** (control plane +
data plane) or as a **worker** (data plane). This mirrors spec §6: the first node
runs the entire stack so a customer can start with one cheap dedicated server,
and additional nodes join the same control plane.

```
            Client SDK / REST API
                     │
            ┌────────▼─────────┐
            │  Control Plane   │   axum HTTP, API-key auth
            │  - scheduler     │   (src/api, src/scheduler)
            │  - billing/usage │   (src/usage, src/service)
            │  - image registry│   (src/images, src/store)
            │  - node registry │   (src/nodes)
            │  - preview router│   (src/api/preview)
            │  - SQLite store  │   (src/store, rusqlite)
            └────────┬─────────┘
                     │ NodeClient trait (src/node.rs)
            ┌────────▼─────────┐
            │   Data Plane     │
            │  LocalNode       │   hot pools (src/hotpool)
            │   └─ Runtime     │   ┌─ MockRuntime (dev, any OS)
            │      trait       │   └─ FirecrackerRuntime (prod, Linux+KVM)
            └──────────────────┘        └─ guest-agent over vsock
```

## The runtime abstraction (the key design choice)

Everything above the `Runtime` trait — API, scheduler, pricing, capacity,
lifecycle, hot pools, billing, image registry, preview proxy — is platform
independent and fully unit/integration tested. The trait (`src/runtime/mod.rs`)
has two implementations:

- **`MockRuntime`** (`src/runtime/mock.rs`): runs on any OS with no KVM. It
  simulates the three boot paths and their timings *honestly* (hot pool fast,
  snapshot restore medium, cold boot slow) and backs exec/files/ports with a real
  per-sandbox host workspace, so the acceptance flows genuinely work. It provides
  **no isolation** and is for development and testing only.
- **`FirecrackerRuntime`** (`src/runtime/firecracker.rs`): boots real Firecracker
  microVMs under the jailer, drives the Firecracker API over its Unix control
  socket, and talks to the in-VM `guest-agent` (`crates/guest-agent`) over an
  `AF_VSOCK`-backed Unix socket using a newline-delimited JSON protocol. It
  compiles on any Unix host but requires Linux + `/dev/kvm` to actually boot.

The installer selects `runtime.kind = "firecracker"` only after the KVM preflight
passes; the default on a non-Linux dev machine is `mock`.

### Boot paths (spec §13.2, §3.5)

A create resolves to exactly one boot path, and the API **reports which one**:

1. `hot_pool` — claim a pre-booted warm microVM matching the image+shape. Fastest.
2. `snapshot_restore` — restore a memory+disk snapshot for the shape. Medium.
3. `cold_boot` — boot the image rootfs fresh. Slowest; pays image cache cost.

Hot-pool numbers are never published as the headline without their label
(`GET /v1/benchmarks` keeps the paths separate, spec §21.3).

## Create flow (spec §13.2)

`src/service.rs::create_sandbox` implements:

1. Validate request + org quota/credits.
2. Classify image (curated or `custom/...`; custom must be already published).
3. Validate constrained knobs (§3.2) and enforce per-image minimum resources.
4. Score nodes and select placement (`src/scheduler.rs`).
5. On the chosen node: claim a hot VM, else snapshot restore, else cold boot.
6. Inject env (secrets injected late, never snapshotted).
7. Run the startup recipe (git, package cache, commands, ports, ready check),
   timing each phase **separately**.
8. Open a per-second billing interval; return id, boot path, timing breakdown,
   and preview/VNC/CDP URLs.

## Scheduler (spec §15)

`src/scheduler.rs` is a pure function over node snapshots, so it is deterministic
and unit-tested. It never admits a sandbox that would exceed a node's memory
admission ceiling (practical units × 2 GB), prefers matching hot pools and image
cache hits, spreads one customer's sandboxes across nodes (anti-affinity),
understands draining/unschedulable nodes, and exposes a stable rejection reason.

```
score = matching_hot_pool_available * 100
      + image_cached                 * 60
      + memory_fit_score             * 50
      + low_cpu_pressure             * 20
      + low_io_pressure              * 10
      - custom_image_cache_miss_penalty
      - noisy_customer_penalty
```

## Pricing & capacity (spec §9, §22)

- `resource_units = max(memory_gb/2, cpu/1, disk_gb/8 * 0.25)` — memory is the
  primary constraint; the base shape is exactly 1.0 units.
- `sandbox_price = unit_price * resource_units * image_multiplier`.
- Capacity is shown in default-equivalent units; a 64 GB node = 20 practical
  units (derated from 26 theoretical).
- Hosted price reconciles to cost: `unit_price = (host_pool_cost +
  platform_overhead) / delivered_units` (`GET /v1/admin/overview`).

The base shape stays visibly cheaper than everything else and below the
$0.015/hr exit ceiling — enforced by a unit test (`pricing::tests`).

## Persistence

SQLite via `rusqlite` (`bundled`, no external DB). Rich domain structs are stored
as JSON in a `data` column alongside indexed columns used for filtering. A single
connection behind a mutex is sufficient for a single-node control plane; the
storage layer is small and synchronous (`src/store.rs`). Postgres is a drop-in
later if the control plane is split out.

## Multi-node: what is wired vs scaffolded

**Wired and working across nodes:** node registry, join tokens, capability/KVM
preflight, drain, capacity accounting, hot-pool status, and the **scheduler**,
which scores all registered nodes.

**Single-node execution today:** the `NodeClient` trait (`src/node.rs`) is the
seam for executing the data path on a chosen node. `LocalNode` implements it for
the control plane's own node and is the fully-working path. A `RemoteNodeClient`
(control plane → worker host agent over an internal API) plugs into the same trait
to execute placements on workers; that worker RPC is the remaining increment for
true multi-node *execution*. Until it is enabled, `create_sandbox` returns a
clear `no_capacity / remote_placement_unsupported` error if the scheduler picks a
non-local node, rather than silently mis-routing. Everything else (registry,
join, drain, scheduling decisions, capacity math) already spans the cluster.

## Background loops (`src/background.rs`)

- **Warmer**: reconciles hot pools toward targets (base 2, node-python 1,
  browser 1 by default).
- **Idle reaper**: auto-stops sandboxes idle past their `auto_stop_seconds`
  window; exec and preview activity bump `last_active_at`.
- **Heartbeat**: keeps the local node's registry entry fresh.

## Guest agent (`crates/guest-agent`)

Runs as an init helper inside each microVM and answers a JSON line protocol over
its stdio, which the in-VM init shim bridges onto an `AF_VSOCK` port. The host
`FirecrackerRuntime` performs Firecracker's `CONNECT <port>` handshake and drives
exec, file read/write, directory listing, and HTTP readiness checks — no guest
networking required. Keeping the transport at stdio makes the agent compile and
unit-test on any platform.
