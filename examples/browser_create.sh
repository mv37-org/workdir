#!/usr/bin/env bash
# Create a browser sandbox (Chromium + Playwright + VNC/CDP) and print its
# preview URLs. Browser sandboxes require explicit resources (spec §3.4, §12).
#   Usage:  SANDBOXD_URL=… SANDBOXD_KEY=… bash examples/browser_create.sh
set -euo pipefail
B="${SANDBOXD_URL:-http://127.0.0.1:8080}"
KEY="${SANDBOXD_KEY:-sk_live_dev}"
auth=(-H "Authorization: Bearer $KEY" -H "Content-Type: application/json")

curl -s -X POST "$B/v1/sandboxes" "${auth[@]}" -d '{
  "image": "browser",
  "resources": { "cpu": 2, "memory_mb": 4096, "disk_gb": 16 },
  "browser": { "enabled": true, "vnc": true, "cdp": true }
}' | python3 -m json.tool
