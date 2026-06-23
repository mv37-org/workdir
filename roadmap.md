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
  ephemeral files/images, opt-in coding agent, browser/VNC/CDP, PTY, and
  persistent volumes.

The named gaps (updated as phases 0–3 landed):

- ~~The idle reaper **stops/frees** sandboxes; the perpetual-standby loop is
  missing.~~ **Done (Phase 1):** reaper now snapshots → frees RAM → `standby` →
  auto-resumes on first request, at $0 while parked, and **survives a
  control-plane restart** (per-VM records persisted to disk).
- ~~**Resume latency is unmeasured.**~~ **Done (Phase 0):** the benchmark harness
  publishes p50/p90/p95 per boot path; mock-validated, KVM-ready.
- **Worker RPC is wired for the data path** through `RemoteNodeClient` +
  `/internal` when `node.rpc_token` is configured. Remaining multi-node work is
  remote PTY/preview proxying, image distribution, and two-box production
  validation.
- Browser/VNC desktops, the jailer path, fork/shared rootfs, PTY, and persistent
  volumes are on `main`; production use still depends on staging the right
  guest kernel/rootfs artifacts per node.
- Running on a single dev-grade node (~20 units).

The gap is specific, not a rewrite. We own the hard primitives; what remains is
operational hardening, latency tuning, and scale-out validation.

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

## Landed 2026-06-12 — density, speed, volumes, PTY

A coordinated batch driving Phases 2/3/5 forward plus two restart-hardening
bug fixes. The mock integration suite covers the orchestration paths; the
feature notes below call out the KVM/node validation that has landed and any
remaining operational caveats.

**Bug fixes (restart hardening):**
- **Jail GC no longer destroys parked standby VMs after a daemon restart.**
  Per-VM records now rehydrate EAGERLY at runtime construction (they were
  lazy-loaded only on first restore), so `gc_stale_jails` sees parked VMs as
  live. Before: ~5 minutes after a restart the sweeper deleted every standby
  VM's record.json + snapshot artifacts — "perpetual" standby silently became
  "standby until the next deploy + 5 min". Unit-tested without KVM.
- **Tap/IP/uid collisions after a restart.** The tap/CID/restore-chroot
  counters now resume above anything a persisted record (or surviving `-rN`
  chroot) still holds; fresh boots could previously reuse `wdtapN` names and
  guest IPs owned by parked VMs (`setup_tap`'s `link del` yanks a live NIC).
  Rehydrated pids are verified against `/proc/<pid>/comm` before any kill.

**Speed:**
- **Golden image snapshots** (per image+shape, produced by the warmer): an
  empty-pool create now takes `snapshot_restore` (~hundreds of ms) instead of a
  ~1.4s cold boot. The old half-wired `boot(snapshot)` path (no tap, no
  guest IP) is gone; golden restores launch like fork children
  (`network_overrides` + guest re-IP), and `snapshot_available` is finally real
  at both create and scheduler level.
- **Pre-spawned jailer pool** (`runtime.jailer_pool_size`, default off): idle
  jailer+Firecracker processes with api.sock listening; restores and golden
  boots claim one and skip the ~30ms relaunch — the measured resume floor.
- **Quiet guest boot** (`quiet loglevel=1`, default on) and exponential-backoff
  polling (1→20ms) replacing fixed 10/20ms sleeps on every boot/restore wait.
- **provision-node.sh** can now stand up an XFS `reflink=1` data filesystem
  (`DATA_FS_DEVICE=...`) and warns when the data dir cannot reflink — on ext4,
  fork pays full multi-GB copies (~58s measured); on reflink it is instant CoW.

**Density:**
- **Restore-based warm pool:** warm VMs restore from the golden snapshot, so
  every sibling's clean guest pages are backed by the SAME `mem.file` host page
  cache — N warm VMs ≈ one memory image + dirty deltas.
- **Measured-memory overcommit** (`[capacity] overcommit`, opt-in): admission
  may pass the static shape-sum ceiling while the host still measures
  `overcommit_headroom_gb` of `MemAvailable` beyond the request. Backpressure:
  the **PSI pressure reaper** (`psi_standby_threshold`) parks the least-recently-
  active sandbox ahead of its idle window when memory PSI spikes.
- **virtio-balloon soft standby** (`runtime.balloon` +
  `standby.balloon_idle_seconds`): the tier between running and snapshot
  eviction — idle guests hand free memory back to the host at zero resume
  latency; deflated when activity returns. Balloon stats feed the new metrics.
- `[capacity] host_reserve_gb` / `practical_derate` are config knobs now (the
  spec defaults predate shared rootfs + standby; measure, then tighten).

**Features:**
- **Persistent volumes (Phase 5) wired end to end:** labelled ext4 backing
  images attached as extra virtio drives (hardlinked into the chroot — guest
  writes land on the one persistent inode), mounted by label in the guest,
  re-staged across standby/restore, exclusive-attach enforced, detach on
  delete, fork refused while attached. Integration-tested (write → delete →
  re-attach → read back).
- **PTY over vsock:** `GET /v1/sandboxes/:id/pty` now bridges a REAL in-guest
  TTY on Firecracker (guest-agent `pty` op: openpty + shell on the slave,
  setsid + TIOCSCTTY; the per-connection agent process makes the stream the
  session). `PtySession` became stream-based; the dev runtime still serves the
  same API with a piped shell.
- **Per-VM working-set metrics:** `GET /v1/sandboxes/:id/metrics` — host VmRSS
  (ground truth vs reserved shape), balloon target + guest stats, net counters.

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
Run fresh sweeps on each `/dev/kvm` node class after changing runtime/image
configuration so published numbers stay tied to the active deployment.

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
intact), and on the KVM node as described in the production notes above. Restore
now relaunches under the jailer with a fresh chroot and re-stages the snapshot
artifacts before `snapshot/load`.

### Phase 2 — Resume latency to target (2–4 weeks)

Drive `snapshot_restore` from hundreds of ms down to **< 25ms**. Levers, in order:

1. **UFFD / userfaultfd** demand-paging backend instead of `File` (lazy page
   faults on resume).
2. Keep the `mem.file` hot in page cache.
3. **Diff snapshots.**
4. **CPU templates** for snapshot portability across hosts.
5. Smaller base VMs.

**Target:** p50 resume < 25ms, p90 < 50ms — measured, not asserted.

**Status:** mostly landed without a userspace UFFD handler. Done: lazy `File`
mem backends with page-cache behavior measured on the node, background prewarm
instead of restore-path prewarm, diff snapshots, a `restore_mem_backend` config
that keeps `uffd` reserved until a real handler exists, a `cpu_template` knob for
cross-host portability, quiet guest boot, exponential backoff, and optional
pre-spawned jailers to remove the measured relaunch floor. Smaller base VMs and
any future post-copy/live-migration UFFD work remain deferred.

### Phase 3 — Density and fork (2–4 weeks)

- **In-RAM rootfs:** EROFS read-only base + tmpfs writable + overlayfs, sharing
  one base image's kernel page cache across sandboxes (DAX) — many more VMs per
  GB.
- **Fork/clone:** copy a snapshot artifact for an instant sibling. Nearly free
  once snapshots are solid.

**Target:** 3–5× density per node; fork latency ≤ resume latency.

**Status:** fork ✅ and in-RAM shared rootfs ✅ — both validated on the KVM node.

`POST /v1/sandboxes/:id/fork` clones a sibling from the parent's live snapshot
into a new sandbox (`boot_path: "fork"`, own id/billing, fresh tap/IP, colocated
on the parent's node). Validated end to end against the mock runtime
(`fork_clones_an_instant_sibling`) **and on the node**: the jailer-aware fork
snapshots the parent, relaunches the child under the jailer in a fresh chroot,
loads the snapshot with `network_overrides` repointing eth0 at the child's tap
(the parent still holds the original, so reopening it would hit EBUSY), then
re-IPs the guest and re-adds its default route. The child inherits the parent's
disk, has its own IP/egress/DNS, and is fully independent (parent survives the
child's writes and deletion). Fork wall time is ~58s on the node, dominated by
the parent Full snapshot + a 2 GB mem copy — the workspaces fs is ext4 (no
reflink), so the copies are real I/O; a reflink-capable fs or a UFFD CoW scheme
would make it instant.

**In-RAM shared rootfs** (`runtime.shared_rootfs = true`, live on the node):
every base VM **hardlinks** the one read-only base `rootfs.ext4` into its jailer
chroot — a single inode, so the host page cache holds **one** copy shared across
all sandboxes (verified: N base VMs all reference the same inode; no per-VM 4 GB
copy). The guest `sandbox-init` mounts that base read-only, layers a per-VM
tmpfs, and `pivot_root`s into a writable overlayfs merged root (`wd.overlay=tmpfs`);
writes land in RAM, reads share the cached base. pivot_root adds negligible boot
latency (hot-pool boot still ~0 ms). The guest kernel has overlayfs + squashfs
but **not** erofs, so true erofs+DAX (guest pages mapped straight from host RAM,
zero guest-side duplication) remains a guest-kernel rebuild — the ext4-ro +
overlay path already delivers the shared-page-cache density today.

### Phase 4 — Scale out: worker RPC (3–6 weeks)

`RemoteNodeClient` is wired on `main`: when the control plane and worker share
`node.rpc_token`, data-plane placement, exec, files, ports, readiness,
lifecycle, snapshots, standby/restore, fork, delete, and hot-pool queries go
through the worker's `/internal` API. The remaining work is remote PTY and
host-routed preview proxying, automatic image distribution, and a two-box
production validation pass.

**Target:** an N-node cluster running thousands of concurrent sandboxes. After
this, capacity is a hardware question, not an architecture one.

### Phase 5 — Desktops and persistence (parallel, 2–4 weeks)

- **Browser/VNC desktop — validated on the node.** The `browser` image boots the
  full computer-use desktop (Xvfb → fluxbox → Chrome → x11vnc → noVNC :6080),
  exposes VNC via the preview proxy, and serves
  `GET /v1/sandboxes/:id/browser/screenshot` — a PNG of the live X desktop
  (captured with ImageMagick `import`, then read back; verified end to end). It
  also gets the Phase 3 shared-rootfs overlay (the browser init pivots into a
  tmpfs+overlayfs root). Chrome binds CDP to loopback, and the browser init
  forwards it to the guest IP, so VNC, screenshot, and CDP are available through
  preview routes.
- **Persistent volumes — done, validated on the node.** Org-scoped block storage
  (`/v1/volumes` CRUD) backed by ext4 images under `runtime.volumes_dir`. Attach
  at create with `volumes: [{volume_id, mount_path}]`; the backing image is
  hardlinked into the jailer chroot (writes hit the real file), attached as an
  extra drive, and mounted by ext4 LABEL in the guest. Attaching forces a cold
  boot. A volume is exclusive to one running sandbox, refuses deletion while
  attached (`409`), and **survives the sandbox's deletion** — verified end to
  end: write into a volume from sandbox A, delete A, attach the same volume to
  sandbox B, read the data back.
- Declarative custom-image builds already exist.

**Target:** persistent workspaces plus a working desktop. **Both delivered.**

### Phase 6 — Deferred (sequence last)

IPv6/VPP/DPDK networking, GPU passthrough, agent colocation, SOC2/HIPAA. These
are large investments; product parity comes first.

## Sequencing

The near-term sequence is operational: run fresh benchmark sweeps on each KVM
host class, enable and measure standby/density knobs per node, keep
browser/volume images staged, then harden remote PTY/preview proxying and image
distribution for multi-node production. Scaling should follow measured per-node
behavior, not replace it.
