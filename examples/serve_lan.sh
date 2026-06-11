#!/usr/bin/env bash
# Run sandboxd on your LAN so another machine can connect over WiFi.
#
#   bash examples/serve_lan.sh            # mock runtime (full API, no isolation)
#   RUNTIME=firecracker bash examples/serve_lan.sh   # real microVMs (needs KVM + images)
#
# Connect from another device at  http://<this-box-LAN-IP>:8080
set -euo pipefail
cd "$(dirname "$0")/.."

PORT="${PORT:-8080}"
RUNTIME="${RUNTIME:-mock}"
DATA_DIR="${SANDBOXD_DATA_DIR:-$HOME/.sandboxd}"
mkdir -p "$DATA_DIR"

# Stable admin key, generated once and reused across restarts.
KEY_FILE="$DATA_DIR/admin-key.txt"
if [[ ! -f "$KEY_FILE" ]]; then
  echo "sk_live_$(head -c16 /dev/urandom | od -An -tx1 | tr -d ' \n')" > "$KEY_FILE"
fi
ADMIN_KEY="$(cat "$KEY_FILE")"

echo "==> building (release)"
cargo build --release -p sandboxd >/dev/null

# Find this box's primary LAN IP (Linux: hostname -I; macOS fallback).
# `|| true` keeps a non-zero exit (e.g. macOS `hostname -I`) from tripping set -e.
LAN_IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
if [[ -z "${LAN_IP:-}" ]]; then
  LAN_IP="$(ipconfig getifaddr en0 2>/dev/null || ip route get 1 2>/dev/null | awk '{print $7; exit}' || true)"
fi
[[ -z "${LAN_IP:-}" ]] && LAN_IP="127.0.0.1"

export SANDBOXD_DATA_DIR="$DATA_DIR"
export SANDBOXD_BIND="0.0.0.0:$PORT"
export SANDBOXD_PUBLIC_DOMAIN="${LAN_IP}.nip.io"   # wildcard DNS that just works on a LAN
export SANDBOXD_PUBLIC_HTTPS=false                  # plain http on the LAN
export SANDBOXD_PUBLIC_PORT="$PORT"                 # so preview URLs include the port
export SANDBOXD_ADMIN_KEY="$ADMIN_KEY"
export SANDBOXD_RUNTIME="$RUNTIME"
export RUST_LOG="${RUST_LOG:-info,sandboxd=info}"

if [[ "$RUNTIME" == "mock" ]]; then
  export SANDBOXD_ALLOW_INSECURE_RUNTIME=1
  echo "!! mock runtime: user commands run on THIS host with no isolation. LAN/dev only."
fi

cat <<EOF

==> sandboxd is starting
    runtime:   $RUNTIME
    API:       http://$LAN_IP:$PORT
    health:    http://$LAN_IP:$PORT/healthz
    admin key: $ADMIN_KEY    (also saved to $KEY_FILE)

    From your laptop:
      curl -s http://$LAN_IP:$PORT/v1/sandboxes \\
        -H "Authorization: Bearer $ADMIN_KEY"

    App preview from a browser (each sandbox gets its own *.nip.io host, so
    asset paths work): the create/expose response 'urls' are directly clickable,
    e.g.  http://<id>-3000.$LAN_IP.nip.io:$PORT/

    If the laptop can't reach it, open the port on this box:
      sudo ufw allow $PORT/tcp        # Ubuntu firewall

EOF

exec ./target/release/sandboxd serve
