#!/usr/bin/env bash
# provision-node.sh — bring a fresh Ubuntu 24.04 / Debian 12 dedicated server
# up as a workdir Firecracker data-plane node. Idempotent: safe to re-run.
#
# This reproduces the manual bring-up of the first node (see docs/RUNBOOK.md).
# Run as root from a checkout of this repo on the node:
#
#   sudo bash deploy/provision-node.sh
#
# Env overrides:
#   KERNEL_CI_VERSION=v1.12   Firecracker CI kernel channel
#   GUEST_KERNEL=6.1.128      guest kernel version to fetch
#   SANDBOX_CIDR=10.200.0.0/16
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DATA_DIR=/var/lib/workdir
KERNEL_CI_VERSION="${KERNEL_CI_VERSION:-v1.12}"
GUEST_KERNEL="${GUEST_KERNEL:-6.1.128}"
SANDBOX_CIDR="${SANDBOX_CIDR:-10.200.0.0/16}"
BRIDGE=wdbr0
BRIDGE_IP=10.200.0.1/16

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31mERROR:\033[0m %s\n' "$*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root"

# --- 0. preflight ----------------------------------------------------------
log "preflight"
[ -e /dev/kvm ] || die "/dev/kvm missing — this must be a bare-metal/dedicated server with virtualization"
grep -qE 'vmx|svm' /proc/cpuinfo || die "no CPU virtualization flags (vmx/svm)"
log "  /dev/kvm present, virtualization OK"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl ca-certificates git build-essential pkg-config \
  rsync nftables iproute2 ufw >/dev/null
command -v docker >/dev/null 2>&1 || { log "installing docker"; curl -fsSL https://get.docker.com | sh >/dev/null 2>&1; }

# --- 1. Firecracker + jailer -----------------------------------------------
if ! command -v firecracker >/dev/null 2>&1; then
  log "installing Firecracker + jailer"
  TAG=$(curl -s https://api.github.com/repos/firecracker-microvm/firecracker/releases/latest | grep -oP '"tag_name": "\K[^"]+')
  tmp=$(mktemp -d)
  curl -sSL "https://github.com/firecracker-microvm/firecracker/releases/download/${TAG}/firecracker-${TAG}-x86_64.tgz" | tar -xz -C "$tmp"
  install -m755 "$tmp/release-${TAG}-x86_64/firecracker-${TAG}-x86_64" /usr/local/bin/firecracker
  install -m755 "$tmp/release-${TAG}-x86_64/jailer-${TAG}-x86_64" /usr/local/bin/jailer
  rm -rf "$tmp"
fi
log "  $(firecracker --version | head -1)"

# --- 1.5 data filesystem (optional but strongly recommended) ----------------
# Reflink copies (cp --reflink) make per-VM rootfs copies, golden-snapshot
# staging, and fork's child artifact copies instant CoW instead of multi-GB
# I/O. (Note: fork wall time is still dominated by the parent's Firecracker Full
# snapshot — ~23s for 2 GB guest RAM — which reflink can't touch; reflink only
# removes the COPY cost. Truly instant fork needs Diff snapshots or UFFD CoW.)
#
# Two ways to get a reflink fs at $DATA_DIR:
#   • DATA_FS_DEVICE=/dev/nvmeXnY — an EMPTY partition/device, formatted XFS.
#   • DATA_FS_LOOPBACK_GB=300     — no spare device (e.g. both disks in one
#                                   RAID1): a loopback XFS image on the root fs.
#                                   Real reflink inside XFS regardless of the
#                                   ext4 backing (validated on the live node).
# If $DATA_DIR already holds data, migrate it by hand (rsync into the new fs
# before mounting) — this script only sets up an EMPTY data dir.
if [ -n "${DATA_FS_DEVICE:-}" ]; then
  log "data filesystem: XFS (reflink=1) on ${DATA_FS_DEVICE} -> ${DATA_DIR}"
  command -v mkfs.xfs >/dev/null 2>&1 || apt-get install -y -qq xfsprogs >/dev/null
  if ! blkid "$DATA_FS_DEVICE" >/dev/null 2>&1; then
    mkfs.xfs -f -m reflink=1 "$DATA_FS_DEVICE"
  else
    log "  ${DATA_FS_DEVICE} already has a filesystem; NOT reformatting"
  fi
  mkdir -p "$DATA_DIR"
  mountpoint -q "$DATA_DIR" || mount "$DATA_FS_DEVICE" "$DATA_DIR"
  grep -q "$DATA_FS_DEVICE $DATA_DIR" /etc/fstab || \
    echo "$DATA_FS_DEVICE $DATA_DIR xfs defaults,noatime 0 2" >> /etc/fstab
elif [ -n "${DATA_FS_LOOPBACK_GB:-}" ]; then
  IMG=/var/lib/workdir-disk.img
  log "data filesystem: loopback XFS (reflink=1) ${DATA_FS_LOOPBACK_GB}G at ${IMG} -> ${DATA_DIR}"
  command -v mkfs.xfs >/dev/null 2>&1 || apt-get install -y -qq xfsprogs >/dev/null
  if [ ! -e "$IMG" ]; then
    truncate -s "${DATA_FS_LOOPBACK_GB}G" "$IMG"
    mkfs.xfs -q -m reflink=1 -L workdir-data "$IMG"
  fi
  mkdir -p "$DATA_DIR"
  mountpoint -q "$DATA_DIR" || mount -o loop "$IMG" "$DATA_DIR"
  grep -q "$IMG $DATA_DIR" /etc/fstab || \
    echo "$IMG $DATA_DIR xfs loop,defaults,noatime 0 0" >> /etc/fstab
  # The daemon must not start before this mount (it would write under it).
  mkdir -p /etc/systemd/system/workdir.service.d
  printf '[Unit]\nRequiresMountsFor=%s\n' "$DATA_DIR" > /etc/systemd/system/workdir.service.d/10-datamount.conf
fi

# --- 2. system user + dirs -------------------------------------------------
log "user + directories"
id -u workdir >/dev/null 2>&1 || useradd --system --home "$DATA_DIR" --shell /usr/sbin/nologin workdir
install -d -o workdir -g workdir "$DATA_DIR" "$DATA_DIR/kernel" "$DATA_DIR/images" "$DATA_DIR/workspaces" "$DATA_DIR/volumes" /etc/workdir
usermod -aG kvm workdir

# Warn loudly when the workspaces fs cannot reflink: every fork/golden staging
# then pays full multi-GB copies (the measured ~58s fork on the ext4 node).
probe="$DATA_DIR/.reflink-probe"
echo x > "$probe.src"
if cp --reflink=always "$probe.src" "$probe.dst" 2>/dev/null; then
  log "  reflink OK: fork/golden staging will be instant CoW"
else
  log "  WARNING: $DATA_DIR cannot reflink (ext4?). Forks and golden-snapshot"
  log "  staging pay full multi-GB copies. Re-run with DATA_FS_DEVICE=<empty"
  log "  partition> to mount an XFS (reflink) volume here."
fi
rm -f "$probe.src" "$probe.dst"

# --- 3. guest kernel -------------------------------------------------------
if [ ! -s "$DATA_DIR/kernel/vmlinux" ]; then
  log "downloading guest kernel ${GUEST_KERNEL}"
  curl -sSL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/${KERNEL_CI_VERSION}/x86_64/vmlinux-${GUEST_KERNEL}" \
    -o "$DATA_DIR/kernel/vmlinux"
  chown workdir:workdir "$DATA_DIR/kernel/vmlinux"
fi
log "  kernel: $(du -h "$DATA_DIR/kernel/vmlinux" | cut -f1)"

# --- 4. sandbox networking (bridge + NAT) ----------------------------------
log "networking: bridge ${BRIDGE} + NAT for ${SANDBOX_CIDR}"
UPLINK=$(ip route show default | awk '/default/ {print $5; exit}')
[ -n "$UPLINK" ] || die "no default route / uplink interface"

install -m644 "$REPO_ROOT/deploy/nftables/sandbox-nat.nft" /etc/nftables-workdir.nft

cat > /etc/systemd/system/workdir-net.service <<EOF
[Unit]
Description=workdir sandbox network (bridge + NAT)
After=network-online.target
Wants=network-online.target
[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/sh -c 'ip link add ${BRIDGE} type bridge 2>/dev/null; ip addr add ${BRIDGE_IP} dev ${BRIDGE} 2>/dev/null; ip link set ${BRIDGE} up; sysctl -qw net.ipv4.ip_forward=1; nft -f /etc/nftables-workdir.nft'
[Install]
WantedBy=multi-user.target
EOF
echo 'net.ipv4.ip_forward=1' > /etc/sysctl.d/99-workdir.conf
systemctl daemon-reload
systemctl enable --now workdir-net >/dev/null 2>&1

# ufw must allow forwarded (routed) traffic for sandbox egress
if command -v ufw >/dev/null 2>&1; then
  sed -i 's/^DEFAULT_FORWARD_POLICY=.*/DEFAULT_FORWARD_POLICY="ACCEPT"/' /etc/default/ufw
  ufw allow 22/tcp >/dev/null 2>&1 || true
  ufw allow in on ${BRIDGE} to 10.200.0.1 port 53 proto udp >/dev/null 2>&1 || true
  ufw allow in on ${BRIDGE} to 10.200.0.1 port 53 proto tcp >/dev/null 2>&1 || true
  yes | ufw enable >/dev/null 2>&1 || true
  ufw reload >/dev/null 2>&1 || true
fi
log "  uplink=$UPLINK, forwarding on, controlled DNS allowed"

# --- 5. build the daemon ---------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
  log "installing Rust"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1
fi
# shellcheck disable=SC1090
. "$HOME/.cargo/env" 2>/dev/null || . /root/.cargo/env
log "building workdir (this takes a few minutes)"
( cd "$REPO_ROOT" && cargo build --release -p sandboxd )
install -m755 "$REPO_ROOT/target/release/workdir" /usr/local/bin/workdir
install -m755 "$REPO_ROOT/target/release/sandbox-guest-agent" "$DATA_DIR/sandbox-guest-agent"

# Static (musl) guest agent for the custom-image builder: injected into custom
# images so musl userlands (alpine, docker:dind) boot. Best-effort — the builder
# falls back to the dynamic agent (glibc-only) and warns if this is absent.
if rustup target list --installed 2>/dev/null | grep -q x86_64-unknown-linux-musl || rustup target add x86_64-unknown-linux-musl 2>/dev/null; then
  command -v musl-gcc >/dev/null 2>&1 || apt-get install -y -qq musl-tools >/dev/null 2>&1 || true
  if ( cd "$REPO_ROOT" && cargo build --release -p guest-agent --target x86_64-unknown-linux-musl ) 2>/dev/null; then
    install -m755 "$REPO_ROOT/target/x86_64-unknown-linux-musl/release/sandbox-guest-agent" "$DATA_DIR/sandbox-guest-agent-static"
    log "  staged static (musl) agent for the custom-image builder"
  else
    log "  WARNING: static agent build failed — custom musl images (alpine) won't boot until staged"
  fi
fi

# --- 6. base image ---------------------------------------------------------
log "building base curated image"
bash "$REPO_ROOT/deploy/build-image.sh" base
# node-python shares the base contents (bigger disk knob)
install -d -o workdir -g workdir "$DATA_DIR/images/node-python"
cp "$DATA_DIR/images/base/rootfs.ext4" "$DATA_DIR/images/node-python/rootfs.ext4"
chown -R workdir:workdir "$DATA_DIR/images"

# --- 7. config + service ---------------------------------------------------
if [ ! -f /root/workdir-admin-key.txt ]; then
  echo "sk_live_$(openssl rand -hex 24)" > /root/workdir-admin-key.txt
  chmod 600 /root/workdir-admin-key.txt
fi
ADMIN_KEY="$(cat /root/workdir-admin-key.txt)"
if [ ! -f /etc/workdir/config.toml ]; then
  cat > /etc/workdir/config.toml <<EOF
[server]
bind = "127.0.0.1:8080"
# Preview/VNC wildcard domain. Use a SEPARATE registrable domain from the
# control panel so untrusted preview content is origin-isolated, and keep it a
# single level (<id>-<port>.workdir.run) so free Cloudflare SSL covers it.
public_domain = "workdir.run"
public_https = true
data_dir = "$DATA_DIR"

[node]
role = "all-in-one"

[runtime]
kind = "firecracker"
# Jailer on: each microVM is chrooted + dropped to its own uid/gid (spec §18).
# The systemd unit runs the daemon as root so the jailer can set this up.
use_jailer = true
firecracker_bin = "/usr/local/bin/firecracker"
jailer_bin = "/usr/local/bin/jailer"
kernel_image = "$DATA_DIR/kernel/vmlinux"
images_dir = "$DATA_DIR/images"
workspace_dir = "$DATA_DIR/workspaces"

[hotpool]
enabled = true

[auth]
bootstrap_admin_key = "$ADMIN_KEY"
bootstrap_org = "org_admin"
EOF
  chown workdir:workdir /etc/workdir/config.toml
fi

install -m644 "$REPO_ROOT/deploy/systemd/workdir.service" /etc/systemd/system/workdir.service
systemctl daemon-reload
systemctl enable --now workdir >/dev/null 2>&1
sleep 2

log "done."
echo
echo "  node:       $(hostname)"
echo "  admin key:  $ADMIN_KEY   (saved to /root/workdir-admin-key.txt)"
echo "  health:     $(curl -s --max-time 5 127.0.0.1:8080/healthz || echo 'starting...')"
echo
echo "  Next: expose it via a Cloudflare Tunnel (see docs/RUNBOOK.md §Tunnel)."
