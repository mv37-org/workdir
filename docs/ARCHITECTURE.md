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
2. `snapshot_restore` — restore the **golden image snapshot** for the
   image+shape (produced once per shape by the warmer; restores launch like
   fork children: fresh chroot, own tap, `network_overrides`, guest re-IP).
   Warm-pool VMs restore from the same artifact, so all siblings' clean guest
   pages share ONE `mem.file` host page cache. Medium latency.
3. `cold_boot` — boot the image rootfs fresh. Slowest; pays image cache cost.
   Volume-attached sandboxes always cold-boot (drives are configured pre-boot).
4. `fork` — clone a sibling from a parent's live snapshot artifact (roadmap
   Phase 3, `POST /v1/sandboxes/:id/fork`). Tracks resume latency, not cold boot.

Hot-pool numbers are never published as the headline without their label
(`GET /v1/benchmarks` keeps the paths separate, spec §21.3).

### Benchmark harness (roadmap Phase 0)

`src/bench.rs` actively measures each boot path on the local node by driving the
runtime with throwaway VMs (no billable sandbox records), persists samples to the
`benchmarks` table, and serves a p50/p90/p95 table at `GET /v1/benchmarks`.
`POST /v1/benchmarks/run` sweeps **every curated image** at its own shape by
default (`{"image":"all"}`) or a single named one. The `snapshot_restore` series
is the perpetual-standby resume path and carries the Phase 2 targets, so every
later phase is judged against a measured, path-separated baseline.

### Perpetual standby (roadmap Phase 1)

The lifecycle adds a `standby` state. The idle reaper no longer merely *stops* an
idle sandbox — it **snapshots it, frees its RAM, parks it in `standby` at $0**,
and the next request **transparently auto-resumes** it (`service::ensure_running`
in the request path). This is what reframes workdir from a sandbox API into a
perpetual-sandbox platform: an idle sandbox stays logically alive but stops
costing anything. The Firecracker runtime implements `standby` (snapshot + kill
to reclaim guest RAM, tear down the tap) and `restore` (recreate the tap — the
networking a naive restore would drop — relaunch, load the snapshot with the
mem file page-cache-prewarmed). `stopped` remains the explicit, user-initiated
pause that requires an explicit resume.

Standby **survives a control-plane restart**: each runtime persists its per-VM
record to disk (mock: `vm.json` beside the workspace; Firecracker: `record.json`
in the jail dir) on boot/standby/restore/fork, and lazily rehydrates it when a
handle isn't resident. After sandboxd restarts, `reconcile_interrupted` fails the
*active* sandboxes (their VMs are gone) but leaves `standby`/`stopped` rows alone;
the first request to a standby sandbox then drives `restore`, which reloads the
record from disk and brings the VM back. Without this, "perpetual" would only
hold until the next daemon restart.

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
preflight, drain, capacity accounting, hot-pool status, the **scheduler**, and
the `RemoteNodeClient` data path. When `node.rpc_token` is set on the control
plane and worker, the control plane forwards placement, exec, file, port expose,
ready-check, lifecycle, snapshot, standby/restore, fork, delete, and hot-pool
queries over the worker's token-authenticated `/internal` API.

**Still local-only / needs production validation:** PTY and host-routed preview
traffic are served by the node that receives the public request today, so remote
PTY/preview proxying is the next multi-node increment. Image distribution is
also operator-managed: curated/custom artifacts must be staged on nodes before
they accept placements. Treat horizontal execution as wired but requiring a
two-box validation pass before relying on it for production capacity.

## Background loops (`src/background.rs`)

- **Warmer**: reconciles hot pools toward targets (base 2, node-python 1,
  browser 1 by default), produces missing golden image snapshots, and drives
  runtime maintenance (jailer-pool refill).
- **Idle reaper**: parks sandboxes idle past their `auto_stop_seconds` window in
  perpetual standby (snapshot + free RAM + $0, auto-resume on next request);
  secret-resident sandboxes fall back to a plain stop. Exec and preview activity
  bump `last_active_at`.
- **Balloon reaper** (soft standby, opt-in): after `standby.balloon_idle_seconds`
  of idleness, inflates the guest balloon so free guest memory returns to the
  host at zero resume latency; deflates when activity returns.
- **Pressure reaper** (opt-in): when memory PSI exceeds
  `capacity.psi_standby_threshold`, parks the least-recently-active running
  sandbox ahead of its idle window — the backpressure that makes
  measured-memory overcommit (`capacity.overcommit`) safe.
- **Heartbeat**: keeps the local node's registry entry fresh.

## Guest agent (`crates/guest-agent`)

Runs as an init helper inside each microVM and answers a JSON line protocol over
its stdio, which the in-VM init shim bridges onto an `AF_VSOCK` port. The host
`FirecrackerRuntime` performs Firecracker's `CONNECT <port>` handshake and drives
exec, file read/write, directory listing, and HTTP readiness checks — no guest
networking required. Keeping the transport at stdio makes the agent compile and
unit-test on any platform.

The `pty` op upgrades its connection to a REAL TTY: the agent openpty()s, runs
the shell on the slave (setsid + TIOCSCTTY, so ^C and job control work), sends
one Ok line, and then the connection IS the terminal byte stream. One agent
process serves one connection (socat `fork`), so the host closing the stream
ends the session and SIGHUPs the shell — no extra channel management.

## Persistent volumes (Phase 5)

A volume is an org-scoped, labelled ext4 image under `volumes_dir`, attached to
at most one sandbox at a time (`POST /v1/sandboxes` with `volumes`) and
surviving that sandbox's deletion. The runtime hardlinks the backing image into
the jailer chroot (same inode — guest writes persist), attaches it as an extra
virtio drive, and the guest mounts it by ext4 LABEL with a positional
`/dev/vd{b+i}` fallback. Standby restores re-stage the backing files; fork is
refused while volumes are attached (they are exclusive, and the parent's
snapshot carries the drives). The dev runtime simulates all of this with host
directories + symlinks, so the acceptance flow (write → delete sandbox →
re-attach → read) genuinely round-trips.
