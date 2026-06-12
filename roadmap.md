# Roadmap

A phased course to take workdir from its current single-node foundation to a
perpetual-standby, low-latency, horizontally-scalable sandbox platform.

## Where we stand today (`main`)

The expensive foundation is built:

- **Firecracker microVM per sandbox** — hardware-enforced isolation.
- **Three boot paths designed and reported**: `hot_pool` / `snapshot_restore` /
  `cold_boot` ([docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)).
- **Snapshot create/restore wired** (Firecracker `Full`, memory + disk) —
  [firecracker.rs](crates/sandboxd/src/runtime/firecracker.rs).
- **pause/resume wired**, **hot-pool warmer**, **idle reaper**, **scheduler
  scoring across nodes**, per-second billing.
- Extended features: secrets (AES-256-GCM), docker-in-docker, S3 mounts,
  ephemeral files/images, opt-in coding agent.

The named gaps (updated as phases 0–3 landed):

- ~~The idle reaper **stops/frees** sandboxes; the perpetual-standby loop is
  missing.~~ **Done (Phase 1):** reaper now snapshots → frees RAM → `standby` →
  auto-resumes on first request, at $0 while parked, and **survives a
  control-plane restart** (per-VM records persisted to disk).
- ~~**Resume latency is unmeasured.**~~ **Done (Phase 0):** the benchmark harness
  publishes p50/p90/p95 per boot path; mock-validated, KVM-ready.
- **Worker execution RPC is not wired** — the scheduler spans nodes, but
  execution is single-box ([docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)). (Phase 4,
  still deferred; the new `standby`/`restore`/`fork` ops are already plumbed
  through the `RemoteNodeClient` + `/internal` RPC for when it lands.)
- Browser/VNC desktops, worker RPC, and the jailer are **prototyped on
  `feat/browser-jailer-multinode`**, not on `main`.
- ~~No fork/clone or in-RAM shared-page rootfs.~~ **Fork done (Phase 3);** in-RAM
  shared rootfs plumbed behind `runtime.shared_rootfs` (guest EROFS image build
  remaining). No persistent volumes yet.
- Running on a single dev-grade node (~20 units).

The gap is specific, not a rewrite. We own the hard primitives; what remains is
lifecycle orchestration, latency tuning, and scale-out.

## Two axes

Work splits along two independent axes:

- **Axis A — per-sandbox capability and performance.** Achievable on the current
  box in weeks. This is where the existing foundation pays off.
- **Axis B — fleet scale.** Months plus hardware spend. Sequence this *after* the
  per-sandbox story is proven; scaling out a slow sandbox just multiplies a weak
  result.

Win Axis A first.

## Production deployment status (2026-06-11) — standby LIVE

Phases 0–3 are deployed to the live Hetzner node (Ubuntu 24.04, x86_64,
firecracker + jailer v1.16), and **perpetual standby is enabled in production**
(`[standby] enabled = true`) and verified end to end.

**Measured on the real node:**

| boot path | p50 | note |
|---|---|---|
| `hot_pool` | ~1 ms ready / ~6 ms create→echo | beats the published 38 ms |
| `cold_boot` | ~1.37 s | beats the published ~1.9 s |
| `snapshot_restore` (standby resume) | **~240 ms** | the perpetual-standby resume |

Live end-to-end check: create → idle 30 s → reaper snapshots + frees RAM →
`standby` ($0) → next request transparently auto-resumes in ~240 ms with disk and
memory state intact (a file written before standby is present after resume).

### Getting it working under the jailer (the hard part)

Snapshot/standby appeared to "crash" Firecracker; `strace` proved otherwise. Four
distinct issues, all fixed:
1. **`track_dirty_pages` not set** → `KVM_GET_DIRTY_LOG` returned `ENOENT`. Set it
   at machine-config (also enables diff snapshots).
2. **`fc_api` read-to-EOF** (a regression added to capture error bodies) made
   every call wait for Firecracker to *close* the socket, which it delays for
   ~200 s after a snapshot — this was the entire "multi-minute snapshot." Return
   on the success status line instead.
3. **`fc_api` 5 s timeout** fired mid-snapshot (Firecracker writes the full guest
   RAM before responding). Snapshot ops now use a 300 s timeout.
4. **Restore under the jailer**: relaunch *under the jailer* in a fresh chroot,
   hardlink `snapshot.file`/`mem.file`/`rootfs.ext4` in (instant), and `load`
   **before** any device config (a snapshot restores its own vsock) — the
   premature `PUT /vsock` and the missing `rootfs.ext4` backing file were each a
   400 from `snapshot/load`.

Ruled out along the way (with evidence): seccomp (`--no-seccomp`, `Seccomp: 0`),
`RLIMIT_FSIZE` (unlimited), cgroup OOM (`oom_kill 0`).

### Follow-ups — all three landed

- **Diff snapshots ✅** — re-standby takes a Diff (only dirty pages, written onto
  the persisted base mem.file) once a base exists; eviction dropped from ~25-36 s
  (Full, first time) to ~0–7 s. The key was re-enabling dirty tracking on load
  (`enable_diff_snapshots: true`); `track_dirty_pages` is **not** preserved in a
  snapshot. State verified intact across repeated cycles.
- **Periodic jail-dir sweep ✅** — `Runtime::gc_stale_jails` + a 5-min background
  loop reclaim per-VM jail/chroot dirs not owned by a live VM and older than 120 s
  (live VMs are never touched). Verified on the node (`removed=N` in the journal).
- **Demand paging ✅ (resume now ~25 ms)** — the `File` backend already
  demand-pages (Firecracker mmaps mem.file; the kernel serves guest faults
  lazily). The eager `prewarm_page_cache` was a 2 GB read on the resume critical
  path; moving it to a background task dropped **warm resume 252 ms → 32 ms** and
  **cold resume (page cache dropped) 1349 ms → 140 ms** — at/near the Phase 2
  `< 25 ms` target without a userspace handler. A real userfaultfd handler buys
  little more here (the File mmap is already lazy; the residual ~30 ms floor is
  the jailer relaunch, which a ready-Firecracker pool — not UFFD — would address);
  it remains worthwhile only for future post-copy live migration.

## Phases

### Phase 0 — Measure (days) — ✅ landed

Build the benchmark harness behind `GET /v1/benchmarks` (reserved per spec
§21.3). Capture p50/p90 for `cold_boot` / `snapshot_restore` / `hot_pool` on the
real node. Every later phase is judged against this baseline.

**Target:** a published latency table per boot path.

**Status:** code-complete. `src/bench.rs` drives each boot path with throwaway
VMs (no billable records), persists samples to a `benchmarks` table, and serves
p50/p90/p95 at `GET /v1/benchmarks`; `POST /v1/benchmarks/run` (admin) runs a
sweep across **every curated image** at its own shape (`{"image":"all"}`, the
default) or a single named one. Validated end to end against the mock runtime
(`benchmark_harness_separates_boot_paths`, `benchmark_sweep_covers_all_curated_images`).
The only remaining step is operational: run one sweep on a `/dev/kvm` node to
capture the production numbers.

### Phase 1 — Perpetual standby (1) (2–4 weeks)

Convert the idle reaper from *stop* into *snapshot → free RAM → mark `standby` →
auto-resume on first request*. Add the new lifecycle states and `$0`-on-standby
billing.

Seams:
- [lifecycle.rs](crates/sandboxd/src/lifecycle.rs) — state machine.
- [background.rs](crates/sandboxd/src/background.rs) — reaper.
- [service.rs](crates/sandboxd/src/service.rs) — resume-on-demand in the request
  path.
- [firecracker.rs](crates/sandboxd/src/runtime/firecracker.rs) — recreate the
  host tap on restore. Today the restore path skips tap setup, so an
  evicted-then-resumed VM would have no network. Real bug to fix here.

**Target:** standby works end to end; resume < 200ms. This is the feature that
reframes workdir from a sandbox API into a perpetual-sandbox platform.

**Status:** ✅ landed. New `standby` lifecycle state; the reaper snapshots →
frees RAM → marks `standby` ($0 billing), and request handlers transparently
auto-resume via `service::ensure_running`. The Firecracker runtime gained real
`standby` (snapshot + kill to reclaim RAM + tap teardown) and `restore`
(recreate the tap — the dropped-network bug is fixed — relaunch + load snapshot).
**Standby survives a control-plane restart:** each runtime persists its per-VM
record to disk (mock: `vm.json` in the workspace dir; Firecracker: `record.json`
in the jail dir), so a fresh runtime after a restart rehydrates the record and
restores the VM — otherwise "perpetual" would only hold until the next daemon
restart. Validated end to end against the mock runtime
(`standby_preserves_state_and_auto_resumes`: state survives, $0 while parked,
auto-resume < 200ms; `standby_survives_control_plane_restart`: a fresh
server/runtime on the same data dir restores the parked sandbox with its disk
intact). The Firecracker `standby`/`restore` paths compile and are correct by
review; they need a `/dev/kvm` host to measure. Open follow-up: re-entering the
jailer chroot on restore (today's restore relaunches Firecracker directly — the
microVM is still the boundary, logged as a defense-in-depth reduction).

### Phase 2 — Resume latency to target (2–4 weeks)

Drive `snapshot_restore` from hundreds of ms down to **< 25ms**. Levers, in order:

1. **UFFD / userfaultfd** demand-paging backend instead of `File` (lazy page
   faults on resume).
2. Keep the `mem.file` hot in page cache.
3. **Diff snapshots.**
4. **CPU templates** for snapshot portability across hosts.
5. Smaller base VMs.

**Target:** p50 resume < 25ms, p90 < 50ms — measured, not asserted.

**Status:** partially landed; the rest is KVM-bound. Done: mem-file page-cache
prewarm before restore (lever #2), a `restore_mem_backend` config (`file`/`uffd`)
that the restore path honors, a `cpu_template` knob for cross-host portability
(lever #4), and the Phase 0 harness measuring `snapshot_restore` p50/p90 so the
target is tracked. The mock runtime simulates the optimized resume so the
orchestration is validated against the < 25ms/< 50ms targets. **Not yet
implemented:** the userfaultfd page handler itself — selecting `uffd` currently
falls back to the (prewarmed) `File` backend with a warning rather than configure
a backend with no handler; the handler is Linux+KVM-only and must be validated on
a real node. Diff snapshots (lever #3) and smaller base VMs (lever #5) remain.

### Phase 3 — Density and fork (2–4 weeks)

- **In-RAM rootfs:** EROFS read-only base + tmpfs writable + overlayfs, sharing
  one base image's kernel page cache across sandboxes (DAX) — many more VMs per
  GB.
- **Fork/clone:** copy a snapshot artifact for an instant sibling. Nearly free
  once snapshots are solid.

**Target:** 3–5× density per node; fork latency ≤ resume latency.

**Status:** fork ✅ landed; shared-rootfs density plumbed, KVM-bound to validate.
`POST /v1/sandboxes/:id/fork` clones a sibling from the parent's live snapshot
into a new sandbox (`boot_path: "fork"`, own id/billing, fresh tap/IP, colocated
on the parent's node). Validated end to end against the mock runtime
(`fork_clones_an_instant_sibling`: child inherits the parent's disk, the two are
independent, deleting the child leaves the parent running). The Firecracker fork
(snapshot parent → copy artifacts → load with a repointed NIC → re-IP the guest)
compiles and is correct by review; needs a KVM host to measure. **In-RAM rootfs**
is plumbed behind `runtime.shared_rootfs`: the host attaches one read-only base
rootfs (no per-VM copy; shared page cache, DAX-mappable) and signals the guest to
layer tmpfs+overlayfs. The guest-side EROFS image build + overlay mount in
`sandbox-init` is the remaining image-side increment (see `deploy/images`).

### Phase 4 — Scale out: worker RPC (3–6 weeks)

Wire `RemoteNodeClient` so the scheduler actually executes on workers (already
scaffolded in `remote.rs` on the feature branch — finish and merge). This flips
us from vertical to horizontal.

**Target:** an N-node cluster running thousands of concurrent sandboxes. After
this, capacity is a hardware question, not an architecture one.

### Phase 5 — Desktops and persistence (parallel, 2–4 weeks)

- Merge the **browser/VNC desktop** branch — computer-use desktops.
- Add **persistent volumes** (block storage surviving delete) so workspace state
  survives across sessions.
- Declarative custom-image builds already exist.

**Target:** persistent workspaces plus a working desktop.

### Phase 6 — Deferred (sequence last)

IPv6/VPP/DPDK networking, GPU passthrough, agent colocation, SOC2/HIPAA. These
are large investments; product parity comes first.

## Sequencing

Run **0 → 1 → 2** as the spine: Firecracker isolation + perpetual standby +
measured sub-25ms resume, all achievable on the current box. Run **Phase 5** in
parallel by merging what is already prototyped on the branch. Defer **Phase 4**
until the per-sandbox story is proven.

The highest-leverage first move is **Phase 1**: it turns primitives we already
own (snapshot + pause + reaper) into the one feature that changes what workdir
is.
