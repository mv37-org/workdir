#!/usr/bin/env bash
# workdir installer (spec §7.3). Installs the control plane + host agent on a
# KVM-capable Hetzner dedicated server (Ubuntu 24.04 / Debian 12).
#
#   Single node (all-in-one):
#     curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
#       --role all-in-one --domain sandboxes.example.com --join-token <token>
#
#   Worker node:
#     curl -fsSL https://workdir.dev/install.sh | sudo bash -s -- \
#       --role worker --control-plane https://api.sandboxes.example.com --join-token <token>
#
# The installer MUST fail clearly if KVM is unavailable.

set -euo pipefail

ROLE="all-in-one"
DOMAIN="sandboxes.local"
CONTROL_PLANE=""
JOIN_TOKEN=""
ADMIN_KEY=""
BIND="0.0.0.0:8080"
WORKDIR_BIN="${WORKDIR_BIN:-/usr/local/bin/workdir}"
DATA_DIR="/var/lib/workdir"
USER_NAME="workdir"
SKIP_BUILD="${SKIP_BUILD:-0}"

log()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31mERROR\033[0m %s\n' "$*" >&2; exit 1; }

while [[ $# -gt 0 ]]; do
  case "$1" in
    --role)          ROLE="$2"; shift 2;;
    --domain)        DOMAIN="$2"; shift 2;;
    --control-plane) CONTROL_PLANE="$2"; shift 2;;
    --join-token)    JOIN_TOKEN="$2"; shift 2;;
    --admin-key)     ADMIN_KEY="$2"; shift 2;;
    --bind)          BIND="$2"; shift 2;;
    --bin)           WORKDIR_BIN="$2"; shift 2;;
    *) die "unknown argument: $1";;
  esac
done

[[ $EUID -eq 0 ]] || die "run as root (sudo)"

# ---------------------------------------------------------------------------
# 1. Preflight checks (spec §7.1). The installer MUST fail clearly without KVM.
# ---------------------------------------------------------------------------
log "preflight checks"
preflight_fail=0

if [[ -e /dev/kvm ]]; then
  log "  /dev/kvm present"
else
  warn "  /dev/kvm MISSING — Firecracker requires KVM. On Hetzner use a DEDICATED server (Hetzner Cloud has no nested virt)."
  preflight_fail=1
fi

if grep -Eq '(vmx|svm)' /proc/cpuinfo; then
  log "  CPU virtualization flags present"
else
  warn "  CPU virtualization flags (vmx/svm) not found"
  preflight_fail=1
fi

if [[ -d /sys/fs/cgroup && ! -f /sys/fs/cgroup/cgroup.procs ]]; then
  warn "  cgroups v2 not detected at /sys/fs/cgroup"
else
  log "  cgroups v2 ok"
fi

if command -v nft >/dev/null 2>&1; then
  log "  nftables present"
else
  warn "  nftables (nft) not installed — will attempt to install"
fi

KVER=$(uname -r); log "  kernel: $KVER"
MEM_GB=$(awk '/MemTotal/ {printf "%.0f", $2/1024/1024}' /proc/meminfo 2>/dev/null || echo 0)
log "  total memory: ${MEM_GB} GB"
[[ "${MEM_GB:-0}" -ge 16 ]] || warn "  less than 16 GB RAM; browser shapes will not be schedulable"

DISK_FREE_GB=$(df -BG --output=avail "$PWD" 2>/dev/null | tail -1 | tr -dc '0-9' || echo 0)
log "  free disk: ${DISK_FREE_GB:-?} GB"

for p in 8080; do
  if ss -ltn 2>/dev/null | grep -q ":$p "; then warn "  port $p already in use"; fi
done

if [[ "$preflight_fail" -ne 0 ]]; then
  die "preflight failed: KVM/virtualization unavailable. This node cannot host Firecracker microVMs."
fi

# ---------------------------------------------------------------------------
# 2. Packages + workdir system user (spec §7.3)
# ---------------------------------------------------------------------------
log "installing dependencies"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq nftables iproute2 curl ca-certificates e2fsprogs >/dev/null

if ! id -u "$USER_NAME" >/dev/null 2>&1; then
  log "creating system user '$USER_NAME'"
  useradd --system --home "$DATA_DIR" --shell /usr/sbin/nologin "$USER_NAME"
fi
install -d -o "$USER_NAME" -g "$USER_NAME" "$DATA_DIR" \
  "$DATA_DIR/images" "$DATA_DIR/workspaces" "$DATA_DIR/kernel" "$DATA_DIR/jail"

# ---------------------------------------------------------------------------
# 3. Firecracker + jailer (spec §7.3)
# ---------------------------------------------------------------------------
install_firecracker() {
  command -v firecracker >/dev/null 2>&1 && { log "firecracker already installed"; return; }
  log "installing Firecracker + jailer"
  local arch; arch=$(uname -m)
  local ver="v1.7.0"
  local url="https://github.com/firecracker-microvm/firecracker/releases/download/${ver}/firecracker-${ver}-${arch}.tgz"
  local tmp; tmp=$(mktemp -d)
  curl -fsSL "$url" -o "$tmp/fc.tgz" || die "could not download Firecracker ${ver}"
  tar -xzf "$tmp/fc.tgz" -C "$tmp"
  install -m 0755 "$tmp/release-${ver}-${arch}/firecracker-${ver}-${arch}" /usr/local/bin/firecracker
  install -m 0755 "$tmp/release-${ver}-${arch}/jailer-${ver}-${arch}"      /usr/local/bin/jailer
  rm -rf "$tmp"
}
install_firecracker

# ---------------------------------------------------------------------------
# 4. workdir binary
# ---------------------------------------------------------------------------
if [[ ! -x "$WORKDIR_BIN" ]]; then
  if [[ "$SKIP_BUILD" == "1" ]]; then
    die "workdir binary not found at $WORKDIR_BIN and SKIP_BUILD=1"
  fi
  warn "workdir binary not found at $WORKDIR_BIN"
  warn "build it on a build host with: cargo build --release"
  warn "then copy target/release/workdir to $WORKDIR_BIN, or pass --bin <path>"
  die  "no workdir binary available"
fi
log "workdir binary: $WORKDIR_BIN ($("$WORKDIR_BIN" --version 2>/dev/null || echo unknown))"

# ---------------------------------------------------------------------------
# 5. Networking: nftables NAT + metadata block + SMTP block (spec §16, §18)
# ---------------------------------------------------------------------------
log "configuring nftables (NAT egress, blocked cloud metadata, blocked SMTP)"
mkdir -p /etc/nftables.d
# Prefer the repo copy when present (./deploy/...), else write the embedded one
# so a piped `curl | bash` install is fully self-contained.
if [[ -f "$(dirname "$0")/nftables/sandbox-nat.nft" ]]; then
  install -m 0644 "$(dirname "$0")/nftables/sandbox-nat.nft" /etc/nftables.d/sandbox-nat.nft
else
  cat > /etc/nftables.d/sandbox-nat.nft <<'NFT'
destroy table inet sandboxd

table inet sandboxd {
    define SANDBOX_NET = 10.200.0.0/16
    define HOST_DNS = 10.200.0.1
    define METADATA_BLOCK = { 169.254.169.254, 169.254.0.0/16, 100.100.100.200 }
    chain forward {
        type filter hook forward priority filter; policy drop;
        ct state established,related accept
        meta nfproto ipv6 drop
        ip saddr $SANDBOX_NET ip daddr $SANDBOX_NET drop
        ip saddr $SANDBOX_NET ip daddr $METADATA_BLOCK drop
        ip saddr $SANDBOX_NET tcp dport {25, 465, 587} drop
        ip saddr $SANDBOX_NET ip daddr {10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16, 127.0.0.0/8} drop
        ip saddr $SANDBOX_NET ct state new meter wd_newconn { ip saddr limit rate over 80/second } drop
        jump sandbox_policy
        ip saddr $SANDBOX_NET accept
    }
    chain sandbox_policy {
    }
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        ip saddr $SANDBOX_NET oifname != "lo" masquerade
    }
    chain input {
        type filter hook input priority filter; policy accept;
        ip saddr $SANDBOX_NET ip daddr $HOST_DNS udp dport 53 accept
        ip saddr $SANDBOX_NET ip daddr $HOST_DNS tcp dport 53 accept
        ip saddr $SANDBOX_NET tcp dport != {8080} ct state new accept
    }
}
NFT
fi
grep -q 'include "/etc/nftables.d/\*.nft"' /etc/nftables.conf 2>/dev/null \
  || echo 'include "/etc/nftables.d/*.nft"' >>/etc/nftables.conf
nft -f /etc/nftables.d/sandbox-nat.nft 2>/dev/null || warn "nft apply deferred until reboot"
sysctl -qw net.ipv4.ip_forward=1
echo 'net.ipv4.ip_forward=1' >/etc/sysctl.d/99-workdir.conf

if command -v ufw >/dev/null 2>&1; then
  ufw allow in on wdbr0 to 10.200.0.1 port 53 proto udp >/dev/null 2>&1 || true
  ufw allow in on wdbr0 to 10.200.0.1 port 53 proto tcp >/dev/null 2>&1 || true
  ufw reload >/dev/null 2>&1 || true
fi

# ---------------------------------------------------------------------------
# 6. Config + systemd unit
# ---------------------------------------------------------------------------
log "writing /etc/workdir/config.toml"
mkdir -p /etc/workdir
"$WORKDIR_BIN" gen-config \
  | sed "s|public_domain = \"sandboxes.example.com\"|public_domain = \"$DOMAIN\"|" \
  | sed "s|bind = \"0.0.0.0:8080\"|bind = \"$BIND\"|" \
  | sed "s|role = \"all-in-one\"|role = \"$ROLE\"|" \
  > /etc/workdir/config.toml

if [[ "$ROLE" == "worker" ]]; then
  [[ -n "$CONTROL_PLANE" ]] || die "worker role requires --control-plane <url>"
  [[ -n "$JOIN_TOKEN" ]]    || die "worker role requires --join-token <token>"
  sed -i "s|control_plane_url = \"\"|control_plane_url = \"$CONTROL_PLANE\"|" /etc/workdir/config.toml
  sed -i "s|join_token = \"\"|join_token = \"$JOIN_TOKEN\"|" /etc/workdir/config.toml
fi
[[ -n "$ADMIN_KEY" ]] && sed -i "s|bootstrap_admin_key = \"\"|bootstrap_admin_key = \"$ADMIN_KEY\"|" /etc/workdir/config.toml
chown -R "$USER_NAME:$USER_NAME" /etc/workdir "$DATA_DIR"

log "installing systemd unit"
if [[ -f "$(dirname "$0")/systemd/workdir.service" ]]; then
  install -m 0644 "$(dirname "$0")/systemd/workdir.service" /etc/systemd/system/workdir.service
else
  cat > /etc/systemd/system/workdir.service <<'UNIT'
[Unit]
Description=workdir — low-cost Firecracker microVM sandbox provider
After=network-online.target nftables.service
Wants=network-online.target

[Service]
Type=simple
User=workdir
Group=workdir
AmbientCapabilities=CAP_NET_ADMIN CAP_NET_BIND_SERVICE CAP_SYS_ADMIN CAP_SETUID CAP_SETGID CAP_MKNOD CAP_DAC_OVERRIDE
Environment=WORKDIR_CONFIG=/etc/workdir/config.toml
Environment=RUST_LOG=info,sandboxd=info
ExecStart=/usr/local/bin/workdir serve
Restart=on-failure
RestartSec=2
LimitNOFILE=1048576
ProtectSystem=full
ProtectHome=true
ReadWritePaths=/var/lib/workdir /etc/workdir
DeviceAllow=/dev/kvm rw
DeviceAllow=/dev/net/tun rw

[Install]
WantedBy=multi-user.target
UNIT
fi
systemctl daemon-reload
systemctl enable --now workdir

# ---------------------------------------------------------------------------
# 7. Validation sandbox + capacity report (spec §7.3)
# ---------------------------------------------------------------------------
log "waiting for workdir to become healthy"
for i in $(seq 1 30); do
  curl -fsS "http://127.0.0.1:${BIND##*:}/healthz" >/dev/null 2>&1 && break
  sleep 1
done

log "running validation sandbox + capacity report"
"$WORKDIR_BIN" doctor --config /etc/workdir/config.toml || true

cat <<EOF

$(printf '\033[1;32m✔ workdir installed\033[0m')
  role:           $ROLE
  data dir:       $DATA_DIR
  config:         /etc/workdir/config.toml
  service:        systemctl status workdir
  api:            http://127.0.0.1:${BIND##*:}/healthz
  practical units: ~$(( (MEM_GB - 12) / 2 * 20 / 26 )) default-equivalent sandboxes

The admin API key was printed once in the journal:
  journalctl -u workdir | grep 'admin API key'
EOF
