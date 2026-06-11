# workdir Operations Runbook

How the production data-plane node is built and run, how to reproduce it from
scratch, and how to recover it. This is the source of truth for the manual
bring-up — the scripts in `deploy/` automate it.

## Topology

```
  Browser ── workdir.dev (Cloudflare Worker: site, auth, API keys)  ── workdir-client repo
                 │  admin API (provision/revoke keys by hash)
                 ▼
  SDK / curl ── api.workdir.dev ──[Cloudflare Tunnel]──▶ Hetzner node :8080
                                                          workdir daemon
                                                          ├─ scheduler, billing, preview proxy
                                                          └─ Firecracker microVMs (tap → wdbr0 → NAT)
```

- **Control panel** (separate `workdir-client` repo): Cloudflare Workers + D1. Issues API keys, pushes
  their SHA-256 hash to the daemon's admin API.
- **Daemon** (this repo, `workdir` binary): control plane + host agent on one
  dedicated server. Binds `127.0.0.1:8080`; reached publicly only via the
  Cloudflare Tunnel (no inbound port).
- **Data plane**: one Firecracker microVM per sandbox.

## Current production node

- Host: Hetzner dedicated, `136.243.153.182` (FSN1), Ubuntu 24.04, RAID1 on 2× NVMe
- CPU: i7-6700 (8 threads), 64 GB RAM → ~19 default-equivalent units
- Runtime: `firecracker`; hot pools: base (2) + node-python (1)
- Admin API key: `/root/workdir-admin-key.txt` (mode 0600) on the node
- Secret-encryption master key: `/var/lib/workdir/secret.key` — **back this up**

---

## 1. Provision a node from scratch

On a fresh Ubuntu 24.04 / Debian 12 **dedicated** server (must have `/dev/kvm`):

```bash
# OS install (Hetzner rescue → installimage): Ubuntu 24.04, RAID1, ext4.
git clone git@github.com:mv37-org/workdir.git && cd workdir
sudo bash deploy/provision-node.sh
```

`provision-node.sh` is idempotent and does all of the below:

1. **Preflight** — `/dev/kvm`, CPU virt flags.
2. **Packages** — docker, nftables, iproute2, build-essential, rustup.
3. **Firecracker + jailer** — latest release into `/usr/local/bin`.
4. **User + dirs** — `workdir` system user; `/var/lib/workdir/{kernel,images,workspaces}`; `workdir` added to the `kvm` group.
5. **Guest kernel** — Firecracker CI `vmlinux` 6.1.x → `/var/lib/workdir/kernel/vmlinux`.
6. **Networking** — bridge `wdbr0` (10.200.0.1/16), `ip_forward`, nftables NAT
   masquerade for 10.200.0.0/16, metadata + SMTP egress blocks; persisted as
   `workdir-net.service`. Sets ufw `DEFAULT_FORWARD_POLICY=ACCEPT`.
7. **Build** — `cargo build --release`; installs `workdir` + `sandbox-guest-agent`.
8. **Base image** — `deploy/build-image.sh base`; copies it to `node-python`.
9. **Config + service** — `/etc/workdir/config.toml` (firecracker, hot pools on),
   generates the admin key, installs + starts `workdir.service`.

---

## 2. Build / rebuild a curated image

Images are Docker builds converted to ext4 (spec §10.3). Definitions live in
`deploy/images/<name>/` (`Dockerfile` + `sandbox-init`).

```bash
cargo build --release -p guest-agent      # the in-VM agent
sudo bash deploy/build-image.sh base       # → /var/lib/workdir/images/base/rootfs.ext4
sudo systemctl restart workdir             # pick up the new image
```

The guest `sandbox-init` (PID 1) mounts /proc /sys /dev, configures `eth0` from
the kernel cmdline (`wd.ip`/`wd.gw`/`wd.dns` injected by the daemon), writes
`/etc/resolv.conf`, then `exec socat VSOCK-LISTEN:5005 ... EXEC:sandbox-guest-agent`.

- **base / node-python**: Python 3.12 + pip + venv, Node 18 + npm, git, build-essential, curl/wget.
- **browser** (TODO): + Chromium, Playwright, Xvfb, x11vnc, noVNC. Not built yet —
  the warmer skips it (`Runtime::image_available`), so browser sandboxes are
  unavailable until this image exists.

---

## 3. Networking model

Each microVM gets a host tap (`wdtapN`) attached to bridge `wdbr0`. The guest is
`10.200.0.<n>/16`, gateway `10.200.0.1` (the bridge), DNS `1.1.1.1`. Egress is
NAT-masqueraded out the uplink. Cloud-metadata IPs and outbound SMTP are dropped.

Gotchas:
- The daemon needs **`CAP_NET_ADMIN`** (set in `workdir.service`) to manage taps.
- **ufw** must have `DEFAULT_FORWARD_POLICY="ACCEPT"`, else forwarded sandbox
  traffic is dropped and egress silently fails.
- After an unclean restart, orphaned `wdtapN` / firecracker processes may linger;
  `pkill -9 firecracker` before a manual restart clears them.

Verify egress from inside a sandbox:
```bash
curl ... /v1/sandboxes/$ID/exec -d '{"cmd":"curl -s https://ifconfig.me"}'   # → the node's public IP
```

---

## 4. Cloudflare Tunnel (public api.workdir.dev)

Remotely-managed (token) tunnel — no inbound port on the node.

1. Cloudflare → Zero Trust → Networks → Tunnels → create `workdir` (cloudflared).
2. Add **Published application routes** (public hostnames):
   - `api.workdir.dev` → `http://localhost:8080`
   - `*.sandboxes.workdir.dev` → `http://localhost:8080` (preview URLs)
3. On the node, install the connector with the tunnel token:
   ```bash
   cloudflared service install <TOKEN>
   ```
4. Point the control panel at it: `WORKDIR_API_URL=https://api.workdir.dev`.

Quick public test without a domain: `cloudflared tunnel --url http://localhost:8080`
prints an ephemeral `*.trycloudflare.com` URL.

---

## 5. Day-to-day ops

```bash
systemctl status workdir            # daemon
journalctl -u workdir -f            # logs
systemctl restart workdir
cat /root/workdir-admin-key.txt     # admin API key

# capacity / nodes
curl -s 127.0.0.1:8080/v1/nodes -H "Authorization: Bearer $(cat /root/workdir-admin-key.txt)"

# live microVMs
pgrep -c firecracker
```

Per-VM Firecracker logs: `/var/lib/workdir/workspaces/jail/vm_<id>/firecracker.log`.

---

## 6. Disaster recovery

If the node dies, a replacement is reproducible with `provision-node.sh` — **except**:

- **`/var/lib/workdir/secret.key`** — the AES master key for org secrets. If lost,
  all stored secrets are unrecoverable. Back it up off-box (or inject via
  `WORKDIR_SECRET_KEY`). This is the one piece the script cannot regenerate.
- **The daemon SQLite DB** (`/var/lib/workdir/workdir.db`) — sandbox/usage/billing
  history. Customer API keys are re-provisioned by the control panel (D1 is the
  source of truth there), so a fresh DB self-heals on next key use; only local
  usage history is lost.

Sandboxes are ephemeral by design — losing the node loses running sandboxes
(acceptable per spec §27; persistent snapshots are the explicit-export path).

---

## 7. Browser sandboxes

`browser` image = Chromium + Playwright + Xvfb + fluxbox + x11vnc + noVNC. The
guest init starts the stack and forwards CDP (Chrome binds it to loopback) to
the guest IP. Build it like any image:

```bash
sudo bash deploy/build-image.sh browser 8G   # ~1.6 GB; min shape 2 vCPU / 4 GB / 16 GB
sudo systemctl restart workdir                # browser hot pool now warms
```

A browser sandbox auto-exposes noVNC (6080) and CDP (9222) preview routes; the
create response carries the URLs.

## 8. Jailer hardening (opt-in)

By default Firecracker is launched directly — the microVM is the isolation
boundary. For defense-in-depth (chroot + per-VM uid/gid + cgroups), set in
`[runtime]`:

```toml
use_jailer = true
jailer_uid_base = 100000
```

The jailer sets up the chroot and drops privileges, so the daemon must run as
**root** when this is on (adjust the systemd `User=`/capabilities accordingly).

## 9. Multi-node (horizontal scaling)

The scheduler/registry span nodes; with the worker RPC, the control plane
forwards data-plane ops to whichever node it places a sandbox on.

- Set the **same** `node.rpc_token` (shared secret) on the control plane and
  every worker — it authenticates the `/internal` RPC. Empty = single-node
  (internal API disabled).
- A worker registers via join token; the control plane reaches it at the node's
  `advertise_addr` and forwards create/exec/files/lifecycle.
- Limitations today: PTY and the preview proxy are served for **local** sandboxes
  only; remote PTY/preview proxying is the next step. Validate on two boxes
  before relying on it.
