#!/usr/bin/env bash
# Install + start the workdir durability stack (Litestream → R2 for the DB,
# restic → R2 nightly for secret.key/volumes/custom images). Idempotent.
#
# Prereq: /etc/workdir/backup.env must exist with the R2 credentials:
#   R2_ENDPOINT=https://<account_id>.r2.cloudflarestorage.com
#   R2_BUCKET=workdir-backups
#   R2_ACCESS_KEY_ID=...
#   R2_SECRET_ACCESS_KEY=...
#   RESTIC_PASSWORD=<a long random passphrase — KEEP IT; restic data is useless without it>
#
# Run as root from a checkout:  sudo bash deploy/backup/setup-backups.sh
set -euo pipefail
REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
[ -f /etc/workdir/backup.env ] || { echo "missing /etc/workdir/backup.env (R2 creds) — see header" >&2; exit 1; }
chmod 600 /etc/workdir/backup.env

echo "==> installing litestream + restic"
command -v restic >/dev/null 2>&1 || apt-get install -y -qq restic >/dev/null
# Pin Litestream to the 0.3.x line: 0.5.x rewrote the config schema and our
# litestream.yml is the documented 0.3.x format. The .deb installs to
# /usr/bin; symlink into /usr/local/bin so the systemd unit's path resolves
# regardless of install method.
LITESTREAM_VER=v0.3.13
if ! command -v litestream >/dev/null 2>&1; then
  arch=$(dpkg --print-architecture)  # amd64/arm64
  tmp=$(mktemp -d)
  curl -fsSL -o "$tmp/litestream.deb" \
    "https://github.com/benbjohnson/litestream/releases/download/${LITESTREAM_VER}/litestream-${LITESTREAM_VER}-linux-${arch}.deb"
  dpkg -i "$tmp/litestream.deb" >/dev/null
  rm -rf "$tmp"
fi
ln -sf "$(command -v litestream)" /usr/local/bin/litestream
# The .deb ships its own empty sample /etc/litestream.yml — ours overwrites it below.
echo "    litestream $(litestream version 2>/dev/null || echo '?'), restic $(restic version 2>/dev/null | head -1)"

echo "==> installing configs + units"
install -m600 "$REPO_ROOT/deploy/backup/litestream.yml" /etc/litestream.yml
install -m755 "$REPO_ROOT/deploy/backup/backup-state.sh" /usr/local/bin/workdir-backup-state
install -m644 "$REPO_ROOT/deploy/systemd/litestream.service" /etc/systemd/system/
install -m644 "$REPO_ROOT/deploy/systemd/workdir-backup.service" /etc/systemd/system/
install -m644 "$REPO_ROOT/deploy/systemd/workdir-backup.timer" /etc/systemd/system/
systemctl daemon-reload

echo "==> starting continuous DB replication + nightly state backup"
systemctl enable --now litestream
systemctl enable --now workdir-backup.timer

# Kick a first state backup now so the repo exists and you can verify.
echo "==> first state backup (initializes the restic repo)"
/usr/local/bin/workdir-backup-state || true

echo
echo "litestream:      $(systemctl is-active litestream)"
echo "backup.timer:    $(systemctl is-active workdir-backup.timer)"
echo "next backup:     $(systemctl show workdir-backup.timer -p NextElapseUSecRealtime --value 2>/dev/null)"
echo
echo "DONE. Verify off-box copies landed in R2 (bucket prefixes: workdir-db/, workdir-state/)."
echo "Disaster-recovery steps: deploy/backup/RESTORE.md"
