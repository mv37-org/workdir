#!/usr/bin/env bash
# Run sandboxd locally with the dev (mock) runtime and exercise the cheap path.
# No KVM required. From the repo root:  bash examples/quickstart_dev.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p sandboxd

export SANDBOXD_DATA_DIR="${SANDBOXD_DATA_DIR:-/tmp/sandboxd-dev}"
export SANDBOXD_RUNTIME=mock
export SANDBOXD_ALLOW_INSECURE_RUNTIME=1   # mock = dev only, no isolation
export SANDBOXD_BIND=127.0.0.1:8080
export SANDBOXD_PUBLIC_DOMAIN=sandboxes.local
export SANDBOXD_ADMIN_KEY=sk_live_dev
rm -rf "$SANDBOXD_DATA_DIR"

./target/release/sandboxd serve & SERVER=$!
trap 'kill $SERVER 2>/dev/null || true' EXIT
for i in $(seq 1 40); do curl -fsS 127.0.0.1:8080/healthz >/dev/null 2>&1 && break; sleep 0.25; done
sleep 2  # let the base hot pool warm

KEY=sk_live_dev; B=http://127.0.0.1:8080
auth=(-H "Authorization: Bearer $KEY" -H "Content-Type: application/json")

echo "==> default create (cheap path)"
ID=$(curl -s -X POST "$B/v1/sandboxes" "${auth[@]}" | python3 -c "import sys,json;print(json.load(sys.stdin)['id'])")
echo "    sandbox: $ID"

echo "==> exec echo ok"
curl -s -X POST "$B/v1/sandboxes/$ID/exec" "${auth[@]}" -d '{"cmd":"echo ok"}' \
  | python3 -c "import sys,json;print('   ', json.load(sys.stdin)['stdout'].strip())"

echo "==> start a preview server (background) + expose + fetch"
curl -s -X POST "$B/v1/sandboxes/$ID/exec" "${auth[@]}" \
  -d '{"cmd":"echo PREVIEW_OK > index.html; python3 -m http.server 3000","background":true}' >/dev/null
sleep 1
curl -s -X POST "$B/v1/sandboxes/$ID/ports/3000/expose" "${auth[@]}" >/dev/null
echo "    preview body: $(curl -s "$B/_preview/$ID/3000/index.html?key=$KEY")"

echo "==> delete"
curl -s -X DELETE "$B/v1/sandboxes/$ID" "${auth[@]}" >/dev/null
echo "    done"
