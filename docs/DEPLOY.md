# Deploying workdir

This guide covers deploying workdir on Hetzner dedicated servers: a single
all-in-one node first, then adding capacity one node at a time, draining, and
the day-2 operations playbooks. It maps directly to spec §7, §8, and §24.

---

## 1. Why dedicated servers (not Hetzner Cloud)

Firecracker needs `/dev/kvm`. **Hetzner Cloud does not expose nested
virtualization**, so it cannot host the sandbox data plane (spec §7.2). Use a
Hetzner **dedicated root server**. Hetzner Cloud *may* host auxiliary services
later, but the data plane must be dedicated.

### Recommended node class (spec §7.1)

```
CPU:     Intel Core i5-13500 class (EX44-style)
RAM:     64 GB minimum
Disk:    2 × NVMe SSD
Network: 1 Gbit/s
OS:      Ubuntu 24.04 LTS or Debian 12
Capability required: /dev/kvm available
```

A 64 GB node delivers ~**20 default-equivalent units** (1 unit = 1 vCPU / 2 GB /
8 GB base sandbox). The practical ceiling is intentionally below the theoretical
26 (memory-only) to absorb host pressure, page cache, Firecracker overhead, and
browser workloads (spec §9.1).

---

## 2. Build the binary

The installer does not compile Rust on the target node. Build once on any Linux
build host (or in CI) and ship the binary:

```bash
cargo build --release -p sandboxd
# produces target/release/workdir  (and target/release/sandbox-guest-agent)
scp target/release/workdir root@<node>:/usr/local/bin/workdir
```

The guest agent (`target/release/sandbox-guest-agent`) is baked into the curated
rootfs images, not installed on the host.

**For the custom-image builder**, also stage a **static (musl)** agent at
`<data_dir>/sandbox-guest-agent-static` — the builder injects it into custom
images so musl-based ones (alpine, `docker:dind`) boot (a glibc agent can't exec
there and panics the guest). Build it once on the node:

```bash
rustup target add x86_64-unknown-linux-musl   # one-time; needs musl-tools
cargo build --release -p guest-agent --target x86_64-unknown-linux-musl
install -m755 target/x86_64-unknown-linux-musl/release/sandbox-guest-agent \
  /var/lib/workdir/sandbox-guest-agent-static
```

Without it the builder falls back to the dynamic agent (which only boots
glibc-based custom images) and logs a warning in the build log.

---

## 3. Single-node install (all-in-one)

The first node runs the **entire stack** — control plane API, SQLite, scheduler,
host agent, Firecracker runtime, image builder, hot pools, and the
preview/VNC proxy (spec §6.2). This is what lets a customer start with one cheap
dedicated server.

```bash
curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
  --role all-in-one \
  --domain sandboxes.example.com
```

The installer (`deploy/install.sh`) performs the spec §7.3 sequence:

1. **Preflight** (and **fails clearly if KVM is unavailable**): `/dev/kvm`,
   CPU virtualization flags, cgroups v2, nftables, kernel version, RAM, disk,
   ports, DNS wildcard readiness.
2. Creates the `workdir` system user.
3. Installs Firecracker + jailer.
4. Installs and enables the host-agent systemd unit.
5. Configures cgroups v2 and nftables/NAT (`deploy/nftables/sandbox-nat.nft`).
6. Installs the preview/VNC proxy (part of the `workdir` binary).
7. Writes `/etc/workdir/config.toml`.
8. Runs `workdir doctor` and reports host capacity.

The admin API key is printed **once** to the journal:

```bash
journalctl -u workdir | grep 'admin API key'
```

> Run the installer locally first with `SKIP_BUILD=1 ./deploy/install.sh
> --bin ./target/release/workdir ...` to preview the preflight on a test box.

### DNS

Point a wildcard record at the node so preview URLs resolve:

```
*.sandboxes.example.com   A   <node public IP>
api.sandboxes.example.com A   <node public IP>
```

Preview URLs take the form `https://<sandbox-id>-<port>.sandboxes.example.com`
(spec §16.2). TLS termination (e.g. a reverse proxy / ACME) sits in front of
sandboxd on 443 → the configured `bind`.

### Verify

```bash
workdir doctor --config /etc/workdir/config.toml
curl -s http://127.0.0.1:8080/healthz
```

### Stage guest artifacts

The installer does not bundle kernel or rootfs artifacts. Before scheduling real
Firecracker sandboxes, stage a guest kernel and build the curated images you
want available:

```bash
cargo build --release -p guest-agent
sudo bash deploy/build-image.sh base
sudo bash deploy/build-image.sh browser 8G   # optional browser image
sudo systemctl restart workdir
```

The fuller `deploy/provision-node.sh` path performs kernel download, base image
build, and node setup from a repo checkout. See [docs/RUNBOOK.md](RUNBOOK.md)
for the day-2 image build and rebuild flow.

---

## 4. Add a node (spec §8, §24.1)

Scaling is one node at a time. The admin dashboard's **Add Hetzner node** flow
shows current capacity, hot-pool status, the exact install command, and expected
monthly cost.

```
1. Buy a Hetzner dedicated server (EX44 class).
2. Install Ubuntu/Debian.
3. On the control plane, mint a token:
     curl -s -X POST https://api.sandboxes.example.com/v1/nodes/join-token \
       -H "Authorization: Bearer <admin-key>"
4. Set the same `node.rpc_token` (or `WORKDIR_RPC_TOKEN`) on the control plane
   and the worker. This shared secret enables the worker's `/internal` API.
5. Run the worker install (preflight validates KVM before joining):
     curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
       --role worker \
       --control-plane https://api.sandboxes.example.com \
       --join-token <token>
6. Wait for preflight validation.
7. Stage the same guest kernel/rootfs artifacts used by the control-plane node.
8. Confirm the hot pool is ready.
9. Mark the node schedulable.
```

The node-join flow (spec §8) registers the node, verifies host capabilities,
and starts accepting placements after the operator has staged the required
curated image artifacts. Automatic image distribution is not included in this
repo yet.

### Control-plane location

Keep the control plane on node 1 until any of: node count exceeds 5, hosted users
need better control-plane uptime, image builds interfere with sandbox workloads,
node 1 becomes a measurable bottleneck, or losing node 1 would be unacceptable
(spec §6.3). At that point, move the control plane to a dedicated node.

### Scale triggers (spec §8)

Watch these and add a node when they fire:

```
cluster_free_units < max(10, 20% of total units)
base_hot_pool_empty_for      > 2 minutes
browser_hot_pool_empty_for   > 5 minutes
custom_image_cache_pressure  > 80%
```

`GET /v1/nodes` reports `cluster.free_units` and per-node `hot_pools` for this.

---

## 5. Drain a node (spec §8, §24.2)

```bash
curl -s -X POST https://api.sandboxes.example.com/v1/nodes/<node-id>/drain \
  -H "Authorization: Bearer <admin-key>"
```

This marks the node **unschedulable** and **draining**. Then:

```
1. Stop assigning new sandboxes (automatic once draining).
2. Let ephemeral sandboxes finish or auto-stop.
3. Notify users with persistent snapshots if needed.
4. Export only snapshots that are explicitly exportable.
5. Remove the node from the registry after no active sandboxes remain.
```

---

## 6. Operations playbooks

### Custom image build failure (spec §24.3)
Surface build logs (`GET /v1/images/:id` → `build_log`), mark the image failed,
keep the previous version active, do not affect running sandboxes, allow retry
with a new version.

### Abuse response (spec §24.4, §18)
Throttle the sandbox, block egress if needed, mark the org under review, stop the
sandbox if a policy threshold is exceeded, preserve audit logs, notify
operator/customer. Kill switches exist by org, API key, sandbox, image, node,
and IP range. (Org suspend and API-key disable are wired today; the remaining
switches share the same mechanism.)

---

## 7. Configuration reference

`/etc/workdir/config.toml` — see [`deploy/config.example.toml`](../deploy/config.example.toml)
or run `workdir gen-config`. Key fields:

| Section | Field | Meaning |
|---|---|---|
| `server` | `bind`, `public_domain`, `public_https`, `public_port`, `capacity_exhausted_message`, `data_dir` | listen address, wildcard preview URL settings, optional hosted-demo capacity copy, state dir |
| `node` | `role`, `node_id`, `advertise_addr`, `total_memory_gb` | node identity and advertised capacity |
| `node` | `control_plane_url`, `join_token`, `rpc_token` | worker join plus control-plane/worker RPC auth |
| `runtime` | `kind` | `firecracker` (prod) or `mock` (dev) |
| `runtime` | `firecracker_bin`, `jailer_bin`, `kernel_image`, `images_dir`, `workspace_dir`, `volumes_dir` | data-plane paths |
| `runtime` | `use_jailer`, `jailer_uid_base`, `firecracker_no_seccomp` | jailer isolation and Firecracker seccomp behavior |
| `runtime` | `restore_mem_backend`, `prewarm_mem_cache`, `shared_rootfs`, `cpu_template`, `quiet_guest_boot`, `jailer_pool_size`, `balloon` | snapshot, density, boot-latency, and memory tuning |
| `pricing` | `default_unit_price_usd_hr`, `image_multipliers`, `monthly_node_cost_usd` | at-cost pricing model |
| `hotpool` | `enabled`, `base_target`, `warm_interval_seconds` | hot-pool warmer |
| `standby` | `enabled`, `balloon_idle_seconds` | perpetual standby and soft-standby ballooning |
| `capacity` | `host_reserve_gb`, `practical_derate`, `overcommit`, `overcommit_headroom_gb`, `psi_standby_threshold` | admission and pressure backpressure |
| `auth` | `bootstrap_admin_key`, `bootstrap_org` | first-boot admin key |

Environment overrides (handy for containers/tests): `WORKDIR_BIND`,
`WORKDIR_DATA_DIR`, `WORKDIR_PUBLIC_DOMAIN`, `WORKDIR_PUBLIC_HTTPS`,
`WORKDIR_PUBLIC_PORT`, `WORKDIR_CAPACITY_EXHAUSTED_MESSAGE`,
`WORKDIR_RUNTIME`, `WORKDIR_ADMIN_KEY`, `WORKDIR_CONFIG`,
`WORKDIR_RPC_TOKEN`, `WORKDIR_STANDBY`, `WORKDIR_FC_NO_SECCOMP`.

---

## 8. Security posture (spec §18)

Programmed by the installer + host agent:

- Firecracker **jailer** support, unique uid/gid range per microVM. The
  `deploy/provision-node.sh` production path enables `runtime.use_jailer = true`;
  set it explicitly if you use the generic installer for a tenant-facing node.
- cgroups v2 for CPU/memory/pids/IO; Firecracker seccomp remains on unless
  `runtime.firecracker_no_seccomp = true` is set for snapshot/restore
  troubleshooting.
- **No Docker socket** inside public sandboxes; no privileged host mounts.
- Cloud metadata IPs blocked; outbound SMTP blocked by default
  (`deploy/nftables/sandbox-nat.nft`).
- No inbound public IP per sandbox — VNC/preview only via the authenticated proxy.
- Secrets injected only after assignment, never baked into snapshots.
- Custom image artifacts are built asynchronously and isolated at runtime; the
  enforcing capability scan/policy-check path is still deferred. See
  [docs/REVIEW.md](REVIEW.md).

---

## 9. Acceptance targets (spec §25.1)

On a clean EX44-class node you should see:

| Scenario | p50 | p95 |
|---|---|---|
| base hot pool → `echo ok` | < 50 ms | < 150 ms |
| base snapshot restore → `echo ok` | < 250 ms | < 750 ms |
| browser hot pool → shell ready | < 250 ms | < 750 ms |
| browser services ready | < 1.5 s | < 4 s |
| cold boot recovery | < 2 s | < 5 s |

Plus: install succeeds on a clean node, preflight fails clearly without KVM,
default sandbox creates, exec/file/preview work, delete cleans routes and disk,
18 default-equivalent units admitted without host swap, 2 base hot sandboxes kept
ready. Check live numbers at `GET /v1/benchmarks` (boot paths are reported
**separately and labeled** — hot-pool numbers are never published as the headline
without their label, spec §21.3).
