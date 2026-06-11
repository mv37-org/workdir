# Deploying sandboxd

This guide covers deploying sandboxd on Hetzner dedicated servers: a single
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
# produces target/release/sandboxd  (and target/release/sandbox-guest-agent)
scp target/release/sandboxd root@<node>:/usr/local/bin/sandboxd
```

The guest agent (`target/release/sandbox-guest-agent`) is baked into the curated
rootfs images, not installed on the host.

---

## 3. Single-node install (all-in-one)

The first node runs the **entire stack** — control plane API, SQLite, scheduler,
host agent, Firecracker runtime, image builder, hot pools, and the
preview/VNC proxy (spec §6.2). This is what lets a customer start with one cheap
dedicated server.

```bash
curl -fsSL https://deploy.example.com/install.sh | sudo bash -s -- \
  --role all-in-one \
  --domain sandboxes.example.com
```

The installer (`deploy/install.sh`) performs the spec §7.3 sequence:

1. **Preflight** (and **fails clearly if KVM is unavailable**): `/dev/kvm`,
   CPU virtualization flags, cgroups v2, nftables, kernel version, RAM, disk,
   ports, DNS wildcard readiness.
2. Creates the `sandboxd` system user.
3. Installs Firecracker + jailer.
4. Installs and enables the host-agent systemd unit.
5. Configures cgroups v2 and nftables/NAT (`deploy/nftables/sandbox-nat.nft`).
6. Installs the preview/VNC proxy (part of the `sandboxd` binary).
7. Writes `/etc/sandboxd/config.toml`, downloads curated image metadata.
8. Runs a validation sandbox and reports capacity.

The admin API key is printed **once** to the journal:

```bash
journalctl -u sandboxd | grep 'admin API key'
```

> Run the installer locally first with `SKIP_BUILD=1 ./deploy/install.sh
> --bin ./target/release/sandboxd ...` to preview the preflight on a test box.

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
sandboxd doctor --config /etc/sandboxd/config.toml
curl -s http://127.0.0.1:8080/healthz
```

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
4. Run the worker install (preflight validates KVM before joining):
     curl -fsSL https://deploy.example.com/install.sh | sudo bash -s -- \
       --role worker \
       --control-plane https://api.sandboxes.example.com \
       --join-token <token>
5. Wait for preflight validation + base image sync.
6. Confirm the hot pool is ready.
7. Mark the node schedulable.
```

The node-join flow (spec §8) registers the node, verifies host capabilities,
syncs curated image metadata, downloads the base image **first**, warms the base
hot pool, then downloads node-python/browser images if configured, and finally
starts accepting placements.

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

`/etc/sandboxd/config.toml` — see [`deploy/config.example.toml`](../deploy/config.example.toml)
or run `sandboxd gen-config`. Key fields:

| Section | Field | Meaning |
|---|---|---|
| `server` | `bind`, `public_domain`, `data_dir` | listen address, wildcard preview domain, state dir |
| `node` | `role` | `all-in-one` or `worker` |
| `node` | `control_plane_url`, `join_token` | worker → control-plane join |
| `runtime` | `kind` | `firecracker` (prod) or `mock` (dev) |
| `runtime` | `firecracker_bin`, `jailer_bin`, `kernel_image`, `images_dir`, `workspace_dir` | data-plane paths |
| `pricing` | `default_unit_price_usd_hr`, `image_multipliers`, `monthly_node_cost_usd` | at-cost pricing model |
| `hotpool` | `enabled`, `base_target`, `warm_interval_seconds` | hot-pool warmer |
| `auth` | `bootstrap_admin_key`, `bootstrap_org` | first-boot admin key |

Environment overrides (handy for containers/tests): `SANDBOXD_BIND`,
`SANDBOXD_DATA_DIR`, `SANDBOXD_PUBLIC_DOMAIN`, `SANDBOXD_RUNTIME`,
`SANDBOXD_ADMIN_KEY`, `SANDBOXD_CONFIG`.

---

## 8. Security posture (spec §18)

Programmed by the installer + host agent:

- Firecracker **jailer** enabled, unique uid/gid range per microVM.
- cgroups v2 for CPU/memory/pids/IO; seccomp for host agent + Firecracker.
- **No Docker socket** inside public sandboxes; no privileged host mounts.
- Cloud metadata IPs blocked; outbound SMTP blocked by default
  (`deploy/nftables/sandbox-nat.nft`).
- No inbound public IP per sandbox — VNC/preview only via the authenticated proxy.
- Secrets injected only after assignment, never baked into snapshots.
- Custom images scanned + policy-checked before publication.

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
