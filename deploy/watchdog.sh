#!/usr/bin/env bash
# workdir node watchdog — runs from a systemd timer (see deploy/systemd/
# workdir-watchdog.{service,timer}). Checks the things that quietly kill a
# single-node service and POSTs a JSON alert to WEBHOOK_URL on every STATE
# TRANSITION (ok→alert and alert→ok), so it never spams. With no webhook
# configured it logs to the journal only.
#
# Config: /etc/workdir/alert.env   e.g.  WEBHOOK_URL=https://hooks.slack.com/...
#         (Discord webhooks work too; payload carries both "text" and "content")
set -u

ENV_FILE=/etc/workdir/alert.env
STATE_FILE=/run/workdir-watchdog.state
DATA_DIR=/var/lib/workdir
MIN_FREE_GB=15
MAX_JOURNAL_ERRORS_10M=20

[ -f "$ENV_FILE" ] && . "$ENV_FILE"
WEBHOOK_URL="${WEBHOOK_URL:-}"

problems=()

# 1. daemon up + answering
if [ "$(systemctl is-active workdir)" != "active" ]; then
  problems+=("workdir.service is $(systemctl is-active workdir)")
elif ! curl -fsS --max-time 5 127.0.0.1:8080/healthz >/dev/null 2>&1; then
  problems+=("daemon active but /healthz not answering")
fi

# 2. data dir mounted (loopback XFS) and not nearly full
if ! mountpoint -q "$DATA_DIR"; then
  problems+=("$DATA_DIR is NOT a mountpoint (loop image did not mount?)")
else
  free_gb=$(df -BG --output=avail "$DATA_DIR" | tail -1 | tr -dc '0-9')
  [ "${free_gb:-0}" -lt "$MIN_FREE_GB" ] && problems+=("data dir low on space: ${free_gb}G free (< ${MIN_FREE_GB}G)")
fi
root_free_gb=$(df -BG --output=avail / | tail -1 | tr -dc '0-9')
[ "${root_free_gb:-0}" -lt "$MIN_FREE_GB" ] && problems+=("root fs low on space: ${root_free_gb}G free")

# 3. journal error spike
errs=$(journalctl -u workdir --since "10 minutes ago" --no-pager 2>/dev/null | grep -ciE ' ERROR | panic' || true)
[ "${errs:-0}" -gt "$MAX_JOURNAL_ERRORS_10M" ] && problems+=("journal error spike: ${errs} errors in 10m")

# 4. hot pools warming (only meaningful while the daemon is up)
if [ ${#problems[@]} -eq 0 ] && [ -f /root/workdir-admin-key.txt ]; then
  deficit=$(curl -fsS --max-time 5 127.0.0.1:8080/v1/admin/overview \
      -H "Authorization: Bearer $(cat /root/workdir-admin-key.txt)" 2>/dev/null \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(p["deficit"] for p in d["hot_pools"]))' 2>/dev/null || echo "")
  # A transient deficit right after restart is normal; alert only on total famine.
  ready=$(curl -fsS --max-time 5 127.0.0.1:8080/v1/admin/overview \
      -H "Authorization: Bearer $(cat /root/workdir-admin-key.txt)" 2>/dev/null \
    | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(p["ready"] for p in d["hot_pools"]))' 2>/dev/null || echo "")
  if [ "${ready:-x}" = "0" ] && [ "${deficit:-0}" != "0" ]; then
    problems+=("hot pools empty with deficit ${deficit} (warmer broken?)")
  fi
fi

# --- state transition + delivery -------------------------------------------
new_state="ok"
msg="workdir node: all checks passing"
if [ ${#problems[@]} -gt 0 ]; then
  new_state="alert"
  msg="workdir node ALERT: $(IFS='; '; echo "${problems[*]}")"
fi
old_state="$(cat "$STATE_FILE" 2>/dev/null || echo ok)"
echo "$new_state" > "$STATE_FILE"

if [ "$new_state" != "$old_state" ]; then
  logger -t workdir-watchdog "$msg"
  if [ -n "$WEBHOOK_URL" ]; then
    payload=$(python3 -c 'import json,sys; print(json.dumps({"text": sys.argv[1], "content": sys.argv[1]}))' "$msg")
    curl -fsS --max-time 10 -X POST -H "Content-Type: application/json" -d "$payload" "$WEBHOOK_URL" >/dev/null 2>&1 \
      || logger -t workdir-watchdog "webhook delivery failed"
  fi
else
  # steady state: journal breadcrumb only when unhealthy
  [ "$new_state" = "alert" ] && logger -t workdir-watchdog "still alerting: $msg"
fi
exit 0
