# Setup

How to build and run **workdir** — for local development, on a LAN, and in
production. For the full deployment guide see [docs/DEPLOY.md](docs/DEPLOY.md);
for architecture see [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

---

## 1. Prerequisites

| | Dev / LAN (`mock` runtime) | Production (`firecracker` runtime) |
|---|---|---|
| OS | macOS or Linux | Linux (Ubuntu 24.04 / Debian 12) |
| Rust | 1.80+ (`rustup`) | 1.80+ (build host) |
| CPU | any | `/dev/kvm` (a Hetzner **dedicated** server, not Hetzner Cloud) |
| Extra | — | Firecracker + jailer, a guest `vmlinux` + rootfs images |

Install Rust if you don't have it:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
```

---

## 2. Clone & build

```bash
git clone git@github.com:mv37-org/workdir.git workdir
cd workdir
cargo build --release        # produces target/release/workdir and sandbox-guest-agent
cargo test --workspace       # unit + integration tests
```

---

## 3. Run — local development (any machine, no KVM)

The `mock` runtime exercises the entire product (API, SDKs, preview, secrets,
all features) but runs `exec` commands **on the host with no isolation**, so it
refuses to start without an explicit opt-in. Dev/LAN only — never expose it to
an untrusted network.

```bash
WORKDIR_DATA_DIR=/tmp/workdir \
WORKDIR_RUNTIME=mock \
WORKDIR_ALLOW_INSECURE_RUNTIME=1 \
WORKDIR_ADMIN_KEY=sk_live_dev \
WORKDIR_PUBLIC_DOMAIN=sandboxes.local \
  ./target/release/workdir serve
```

Smoke-test it:

```bash
bash examples/quickstart_dev.sh          # create → exec → preview → delete
# or the Python SDK:
WORKDIR_URL=http://127.0.0.1:8080 WORKDIR_API_KEY=sk_live_dev \
  python3 sdk/python/src/workdir/__init__.py
```

---

## 4. Run — on your LAN (connect from another device)

One command binds to your LAN IP, picks a stable admin key, and prints a
ready-to-use URL. Preview works in a browser with **zero DNS setup** (it uses
`<ip>.nip.io` wildcard DNS).

```bash
bash examples/serve_lan.sh               # mock runtime by default
# then from another laptop: http://<this-box-ip>:8080
```

If the other device can't connect, open the port on the host:

```bash
sudo ufw allow 8080/tcp                  # Ubuntu firewall
```

See the launcher's printed banner for the exact URL, admin key, and preview
format.

---

## 5. Run — production (Hetzner dedicated server)

```bash
# On a KVM-capable dedicated server (Ubuntu 24.04 / Debian 12):
curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
  --role all-in-one --domain sandboxes.example.com
```

The installer runs preflight (and **fails clearly without KVM**), installs
Firecracker + jailer + the systemd unit + nftables policy, writes
`/etc/workdir/config.toml`, and prints the admin key once. Full walkthrough,
scaling, and draining: [docs/DEPLOY.md](docs/DEPLOY.md).

> **Note:** the `firecracker` runtime needs a guest `vmlinux` and rootfs images
> with the guest agent baked in. The repo includes image-build scripts and
> curated image definitions, but it does not bundle prebuilt kernel/rootfs
> artifacts; see [docs/RUNBOOK.md](docs/RUNBOOK.md) and
> [docs/FEATURES.md](docs/FEATURES.md) for build steps and per-feature image
> requirements.

---

## 6. Configuration

Generate a starter config or inspect the resolved one:

```bash
./target/release/workdir gen-config > config.toml     # example config
./target/release/workdir doctor                        # show config + KVM/memory detection
./target/release/workdir serve --config config.toml
```

Reference: [`deploy/config.example.toml`](deploy/config.example.toml).

### Environment overrides (handy for dev/containers)

| Variable | Meaning |
|---|---|
| `WORKDIR_CONFIG` | path to `config.toml` |
| `WORKDIR_BIND` | listen address (default `0.0.0.0:8080`) |
| `WORKDIR_DATA_DIR` | state dir (DB, key, workspaces) |
| `WORKDIR_RUNTIME` | `mock` or `firecracker` |
| `WORKDIR_ALLOW_INSECURE_RUNTIME` | `1` to permit the `mock` runtime (dev only) |
| `WORKDIR_ADMIN_KEY` | seed a known admin key (else generated + printed once) |
| `WORKDIR_PUBLIC_DOMAIN` | wildcard domain for preview URLs |
| `WORKDIR_PUBLIC_HTTPS` | `true`/`false` scheme in preview URLs |
| `WORKDIR_PUBLIC_PORT` | include this port in preview URLs (non-443/80) |
| `WORKDIR_SECRET_KEY` | base64 32-byte AES key for secret encryption |
| `WORKDIR_RPC_TOKEN` | shared control-plane/worker token for `/internal` node RPC |
| `WORKDIR_STANDBY` | `1`/`true` to enable perpetual standby |
| `WORKDIR_FC_NO_SECCOMP` | `1`/`true` to disable Firecracker's built-in seccomp filter |

---

## 7. Secrets & keys — back these up

On first boot, workdir generates an **AES master key** at
`<data_dir>/secret.key` (mode `0600`) used to encrypt org secrets at rest. It is
`.gitignore`d and **must not be committed**.

- Back up `secret.key` (or set `WORKDIR_SECRET_KEY` from a vault) so stored
  secrets survive a node rebuild — without it, encrypted secrets are
  unrecoverable.
- The admin API key is shown once in the logs:
  `journalctl -u workdir | grep 'admin API key'` (prod) or the startup banner.

Never commit `secret.key`, `admin-key.txt`, `*.db`, or a `config.toml`
containing `bootstrap_admin_key`.
