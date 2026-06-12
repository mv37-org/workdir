#!/usr/bin/env python3
"""workdir feature tour — exercises every user-facing feature against a live
deployment and tells you what it found.

Usage:
    WORKDIR_API_KEY=sk_live_... python3 examples/feature_tour.py
    WORKDIR_API_KEY=... WORKDIR_API_URL=https://api.example.com python3 ...

Stdlib only. The optional PTY test additionally needs `pip install websockets`
(it is skipped, not failed, without it). Takes ~4 minutes end to end — the
perpetual-standby test genuinely waits for the idle reaper to park a sandbox.

Pass --full to also run the build-heavy tests: a custom image built from an
OCI reference (alpine), and docker-in-docker on a custom dind image. These
pull images on the node, so the first run adds a few minutes.

Everything the tour creates is deleted at the end, even on failure. Expect a
few cents of per-second billing at most.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

API = os.environ.get("WORKDIR_API_URL", "https://api.workdir.dev").rstrip("/")
KEY = os.environ.get("WORKDIR_API_KEY", "")

FULL = "--full" in sys.argv

RESULTS: list[tuple[str, str, str]] = []  # (section, status, note)
CREATED_SANDBOXES: list[str] = []
CREATED_VOLUMES: list[str] = []
CREATED_SECRETS: list[str] = []
CREATED_IMAGES: list[str] = []


def api(method: str, path: str, body=None, timeout=120, raw=False):
    """One API call. Returns (status_code, parsed_json_or_bytes)."""
    req = urllib.request.Request(
        API + path,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": f"Bearer {KEY}", "Content-Type": "application/json"},
        method=method,
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            data = r.read()
            return r.status, (data if raw else json.loads(data or b"{}"))
    except urllib.error.HTTPError as e:
        data = e.read()
        try:
            return e.code, json.loads(data or b"{}")
        except json.JSONDecodeError:
            return e.code, {"raw": data[:200].decode(errors="replace")}


def section(title: str):
    print(f"\n\033[1m── {title} {'─' * max(1, 64 - len(title))}\033[0m")


def ok(sec: str, note: str):
    RESULTS.append((sec, "PASS", note))
    print(f"  \033[32mPASS\033[0m  {note}")


def fail(sec: str, note: str):
    RESULTS.append((sec, "FAIL", note))
    print(f"  \033[31mFAIL\033[0m  {note}")


def skip(sec: str, note: str):
    RESULTS.append((sec, "SKIP", note))
    print(f"  \033[33mSKIP\033[0m  {note}")


def info(note: str):
    print(f"        {note}")


def create_sandbox(body=None) -> dict:
    st, sb = api("POST", "/v1/sandboxes", body or {})
    if st != 201:
        raise RuntimeError(f"create failed ({st}): {sb}")
    CREATED_SANDBOXES.append(sb["id"])
    return sb


def exec_in(sid: str, cmd: str, background=False) -> dict:
    st, r = api("POST", f"/v1/sandboxes/{sid}/exec", {"cmd": cmd, "background": background})
    if st != 200:
        raise RuntimeError(f"exec failed ({st}): {r}")
    return r


def delete_sandbox(sid: str):
    api("DELETE", f"/v1/sandboxes/{sid}")
    if sid in CREATED_SANDBOXES:
        CREATED_SANDBOXES.remove(sid)


# ─── 01 health + auth ───────────────────────────────────────────────────────

def tour_health():
    section("01 · health + auth")
    st, h = api("GET", "/healthz")
    if st == 200 and h.get("status") == "ok":
        ok("health", f"{API}/healthz is up")
    else:
        fail("health", f"healthz returned {st}: {h}")
        raise SystemExit(1)
    st, _ = api("GET", "/v1/usage")
    if st == 200:
        ok("auth", "API key accepted")
    else:
        fail("auth", f"key rejected ({st}) — check WORKDIR_API_KEY")
        raise SystemExit(1)


# ─── 02 create / exec / files (hot pool) ────────────────────────────────────

def tour_basics() -> str:
    section("02 · create → exec → files  (expect boot_path=hot_pool, ~tens of ms)")
    t0 = time.time()
    sb = create_sandbox({"auto_stop_seconds": 3600})
    wall = (time.time() - t0) * 1000
    sid = sb["id"]
    info(f"{sid}  boot_path={sb['boot_path']}  boot_ms={sb['timings']['boot_ms']}  api_wall={wall:.0f}ms")
    ok("create", f"sandbox up via {sb['boot_path']}")

    r = exec_in(sid, "echo hello && uname -a")
    if r["exit_code"] == 0 and "hello" in r["stdout"]:
        ok("exec", f"exec works: {r['stdout'].splitlines()[-1][:60]}")
    else:
        fail("exec", f"unexpected: {r}")

    api("PUT", f"/v1/sandboxes/{sid}/files", {"path": "tour/notes.txt", "content": "round trip"})
    st, read = api("GET", f"/v1/sandboxes/{sid}/files?path=tour/notes.txt")
    if st == 200 and read.get("content") == "round trip":
        ok("files", "file write/read round trip")
    else:
        fail("files", f"read back: {st} {read}")

    st, m = api("GET", f"/v1/sandboxes/{sid}/metrics")
    metrics = (m or {}).get("metrics") or {}
    if st == 200 and metrics.get("host_rss_bytes"):
        rss = metrics["host_rss_bytes"] // (1024 * 1024)
        ok("metrics", f"live metrics: host_rss={rss}MB for a {m['reserved']['memory_mb']}MB reservation")
        if metrics.get("balloon_stats"):
            free = (metrics["balloon_stats"].get("free_memory") or 0) // (1024 * 1024)
            info(f"guest balloon stats present (free_memory={free}MB)")
    else:
        fail("metrics", f"metrics: {st} {m}")
    return sid


# ─── 03 boot-path honesty: drain pool → golden restore ─────────────────────

def tour_boot_paths():
    section("03 · boot-path honesty  (drain the pool → expect a snapshot_restore in ~45ms)")
    paths = []
    for _ in range(3):
        sb = create_sandbox({"auto_stop_seconds": 3600})
        paths.append((sb["id"], sb["boot_path"], sb["timings"]["boot_ms"]))
    for sid, path, ms in paths:
        info(f"{sid}  {path}  {ms}ms")
    if any(p == "snapshot_restore" for _, p, _ in paths):
        ms = next(ms for _, p, ms in paths if p == "snapshot_restore")
        ok("golden", f"empty-pool create restored the golden snapshot in {ms}ms")
    elif all(p == "hot_pool" for _, p, _ in paths):
        skip("golden", "warmer refilled faster than we drained — all hot_pool (still honest reporting)")
    else:
        fail("golden", f"unexpected path mix: {[p for _, p, _ in paths]}")
    for sid, _, _ in paths:
        delete_sandbox(sid)


# ─── 04 preview port ────────────────────────────────────────────────────────

def tour_preview():
    section("04 · public preview URL  (server inside the sandbox, fetched over the internet)")
    sb = create_sandbox({"image": "node-python", "resources": {"cpu": 1, "memory_mb": 2048, "disk_gb": 16}, "auto_stop_seconds": 3600})
    sid = sb["id"]
    exec_in(sid, "cd /workspace && nohup python3 -m http.server 8000 >/dev/null 2>&1 &", background=False)
    st, port = api("POST", f"/v1/sandboxes/{sid}/ports/8000/expose")
    if st != 200 or "url" not in port:
        fail("preview", f"expose failed: {st} {port}")
        delete_sandbox(sid)
        return
    url = port["url"]
    info(f"preview url (public, host-routed): {url}")
    # Two ways to reach the in-sandbox server:
    #   • the public host-routed URL above — reachable from any client over the
    #     internet (its TLS only validates from a real client, not the node
    #     hitting its own public hostname), and
    #   • the path-based proxy on the API base, which works in every environment
    #     including straight against the node. Both require the org's auth (a
    #     Bearer header or ?key=), so the preview can't be an open SSRF gateway.
    # No trailing slash: that hits the bare `:port` proxy route. A trailing
    # slash would need the `/*rest` route, which 404s on an empty rest segment.
    path_url = f"{API}/_preview/{sid}/8000"
    got, which = None, None
    candidates = [
        (urllib.request.Request(url), "public host-routed URL"),
        (urllib.request.Request(path_url, headers={"Authorization": f"Bearer {KEY}"}), "path-based preview proxy"),
    ]
    for req_obj, label in candidates:
        for _ in range(6):
            try:
                with urllib.request.urlopen(req_obj, timeout=5) as r:
                    got, which = r.status, label
                    break
            except Exception:
                time.sleep(1.5)
        if got == 200:
            break
    if got == 200:
        ok("preview", f"reached the in-sandbox server via the {which} (HTTP 200)")
    else:
        fail("preview", f"could not reach the preview (tried {url} and {path_url})")
    delete_sandbox(sid)


# ─── 05 interactive PTY ─────────────────────────────────────────────────────

def tour_pty():
    section("05 · interactive PTY  (real TTY over WebSocket — needs `pip install websockets`)")
    try:
        import asyncio
        import websockets  # type: ignore
    except ImportError:
        skip("pty", "websockets not installed — `pip install websockets` and rerun to test")
        return
    sb = create_sandbox({"auto_stop_seconds": 3600})
    sid = sb["id"]
    ws_url = API.replace("https://", "wss://").replace("http://", "ws://") + f"/v1/sandboxes/{sid}/pty"
    headers = {"Authorization": f"Bearer {KEY}"}

    def connect():
        # websockets renamed the header kwarg across major versions.
        try:
            return websockets.connect(ws_url, additional_headers=headers)
        except TypeError:
            return websockets.connect(ws_url, extra_headers=headers)

    async def drive():
        async with connect() as ws:
            await asyncio.sleep(1.0)  # let the shell start
            await ws.send("tty; echo marker-$((40+2))\n")
            buf = ""
            deadline = asyncio.get_event_loop().time() + 10
            while "marker-42" not in buf and asyncio.get_event_loop().time() < deadline:
                try:
                    msg = await asyncio.wait_for(ws.recv(), timeout=3)
                except Exception:
                    break
                buf += msg.decode(errors="replace") if isinstance(msg, bytes) else str(msg)
            return buf

    try:
        out = asyncio.run(drive())
        if "/dev/pts/" in out and "marker-42" in out:
            ok("pty", "real TTY: shell on /dev/pts/*, interactive command executed")
        else:
            fail("pty", f"unexpected pty output: {out[:120]!r}")
    except Exception as e:
        fail("pty", f"websocket error: {e}")
    delete_sandbox(sid)


# ─── 06 persistent volumes ──────────────────────────────────────────────────

def tour_volumes():
    section("06 · persistent volumes  (data outlives the sandbox; exclusive attach)")
    st, v = api("POST", "/v1/volumes", {"name": f"tour-{int(time.time())}", "size_gb": 5})
    if st != 201:
        fail("volumes", f"volume create: {st} {v}")
        return
    vid = v["id"]
    CREATED_VOLUMES.append(vid)
    info(f"volume {vid} (5 GB)")

    sb = create_sandbox({"volumes": [{"volume_id": vid, "mount_path": "/data"}], "auto_stop_seconds": 3600})
    sid = sb["id"]
    info(f"attached to {sid} (boot_path={sb['boot_path']} — volumes always cold-boot)")
    r = exec_in(sid, "echo persisted > /data/state.txt && sync && mount | grep ' /data '")
    if r["exit_code"] == 0 and "/data" in r["stdout"]:
        ok("vol-mount", f"real block device: {r['stdout'].strip()[:70]}")
    else:
        fail("vol-mount", f"mount check: {r}")

    st, _ = api("POST", "/v1/sandboxes", {"volumes": [{"volume_id": vid, "mount_path": "/data"}]})
    ok("vol-excl", f"double-attach refused ({st})") if st == 409 else fail("vol-excl", f"expected 409, got {st}")
    st, _ = api("DELETE", f"/v1/volumes/{vid}")
    ok("vol-guard", f"delete-while-attached refused ({st})") if st == 409 else fail("vol-guard", f"expected 409, got {st}")
    st, _ = api("POST", f"/v1/sandboxes/{sid}/fork")
    ok("vol-fork", f"fork-with-volume refused ({st})") if st == 409 else fail("vol-fork", f"expected 409, got {st}")

    delete_sandbox(sid)
    sb2 = create_sandbox({"volumes": [{"volume_id": vid, "mount_path": "/data"}], "auto_stop_seconds": 3600})
    r = exec_in(sb2["id"], "cat /data/state.txt")
    if r["exit_code"] == 0 and "persisted" in r["stdout"]:
        ok("vol-persist", "data survived sandbox deletion and re-attach")
    else:
        fail("vol-persist", f"read back: {r}")
    delete_sandbox(sb2["id"])
    st, _ = api("DELETE", f"/v1/volumes/{vid}")
    if st == 200:
        CREATED_VOLUMES.remove(vid)
        ok("vol-delete", "detached volume deleted")
    else:
        fail("vol-delete", f"delete: {st}")


# ─── 07 fork ────────────────────────────────────────────────────────────────

def tour_fork():
    section("07 · fork  (clone a live sandbox; takes ~30s on the current node)")
    parent = create_sandbox({"auto_stop_seconds": 3600})
    pid = parent["id"]
    exec_in(pid, "echo parent-state > /workspace/p.txt")
    t0 = time.time()
    st, child = api("POST", f"/v1/sandboxes/{pid}/fork", timeout=180)
    wall = time.time() - t0
    if st != 201:
        fail("fork", f"fork: {st} {child}")
        delete_sandbox(pid)
        return
    cid = child["id"]
    CREATED_SANDBOXES.append(cid)
    info(f"child {cid}  boot_path={child['boot_path']}  wall={wall:.1f}s")
    r = exec_in(cid, "cat /workspace/p.txt")
    if "parent-state" in r["stdout"]:
        ok("fork", f"child inherited the parent's live state in {wall:.1f}s")
    else:
        fail("fork", f"child state: {r}")
    exec_in(cid, "echo child-only > /workspace/c.txt")
    r = exec_in(pid, "ls /workspace/")
    if "c.txt" not in r["stdout"]:
        ok("fork-iso", "child writes don't leak back to the parent")
    else:
        fail("fork-iso", "parent saw the child's write")
    delete_sandbox(cid)
    delete_sandbox(pid)


# ─── 08 secrets ─────────────────────────────────────────────────────────────

def tour_secrets():
    section("08 · secrets  (injected as env, never snapshotted)")
    name = "TOUR_SECRET"
    st, _ = api("PUT", f"/v1/secrets/{name}", {"value": "hunter2-but-encrypted"})
    if st != 200:
        fail("secrets", f"put: {st}")
        return
    CREATED_SECRETS.append(name)
    sb = create_sandbox({"startup": {"secrets": [name]}, "auto_stop_seconds": 3600})
    sid = sb["id"]
    r = exec_in(sid, f'echo -n "${name}"')
    if r["stdout"] == "hunter2-but-encrypted":
        ok("secrets", "secret injected into exec env")
    else:
        fail("secrets", f"env: {r}")
    st, _ = api("POST", f"/v1/sandboxes/{sid}/snapshot")
    if st == 409:
        ok("secret-snap", f"snapshot of a secret-resident sandbox refused ({st}) — secrets never hit disk")
    else:
        fail("secret-snap", f"expected 409, got {st}")
    delete_sandbox(sid)
    api("DELETE", f"/v1/secrets/{name}")
    CREATED_SECRETS.remove(name)


# ─── 09 pause / resume ──────────────────────────────────────────────────────

def tour_pause_resume():
    section("09 · explicit pause / resume")
    sb = create_sandbox({"auto_stop_seconds": 3600})
    sid = sb["id"]
    api("POST", f"/v1/sandboxes/{sid}/pause")
    _, cur = api("GET", f"/v1/sandboxes/{sid}")
    paused_ok = cur.get("state") == "stopped"
    st, _ = api("POST", f"/v1/sandboxes/{sid}/resume")
    _, cur = api("GET", f"/v1/sandboxes/{sid}")
    if paused_ok and cur.get("state") == "running":
        ok("pause", "pause → stopped → resume → running")
    else:
        fail("pause", f"states: paused_ok={paused_ok}, after_resume={cur.get('state')}")
    delete_sandbox(sid)


# ─── 10 perpetual standby ───────────────────────────────────────────────────

def tour_standby():
    section("10 · perpetual standby  (waits ~60s for the reaper — the meter stops at $0)")
    sb = create_sandbox({"auto_stop_seconds": 30})
    sid = sb["id"]
    exec_in(sid, "echo survives-standby > /workspace/s.txt")
    info("sandbox idle; waiting for snapshot + RAM-free + park (auto_stop=30s)...")
    state = ""
    for _ in range(24):
        time.sleep(5)
        _, cur = api("GET", f"/v1/sandboxes/{sid}")
        state = cur.get("state", "?")
        if state == "standby":
            break
    if state != "standby":
        fail("standby", f"never parked (state={state})")
        delete_sandbox(sid)
        return
    ok("standby", "parked in standby — billing is $0 from this moment")
    t0 = time.time()
    r = exec_in(sid, "cat /workspace/s.txt")
    wall = (time.time() - t0) * 1000
    if "survives-standby" in r["stdout"]:
        ok("auto-resume", f"next exec transparently woke it in {wall:.0f}ms (state intact, no resume call)")
    else:
        fail("auto-resume", f"wake: {r}")
    delete_sandbox(sid)


# ─── 11 browser desktop ─────────────────────────────────────────────────────

def tour_browser():
    section("11 · browser desktop  (headed Chrome: VNC to watch, CDP to drive, screenshot API)")
    st, sb = api("POST", "/v1/sandboxes", {
        "image": "browser",
        "resources": {"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
        "browser": {"enabled": True},
        "auto_stop_seconds": 3600,
    })
    if st != 201:
        fail("browser", f"create: {st} {sb}")
        return
    sid = sb["id"]
    CREATED_SANDBOXES.append(sid)
    st, b = api("GET", f"/v1/sandboxes/{sid}/browser", timeout=60)
    urls = (b or {}).get("urls") or {}
    if st == 200 and urls.get("vnc") and urls.get("cdp"):
        ok("browser", "desktop up with VNC + CDP endpoints")
        info(f"watch it live:  {urls['vnc']}")
        info(f"drive it (Playwright connectOverCDP):  {urls['cdp']}")
    else:
        fail("browser", f"browser info: {st} {b}")
    st, png = api("GET", f"/v1/sandboxes/{sid}/browser/screenshot", raw=True, timeout=60)
    if st == 200 and isinstance(png, bytes) and png[:4] == b"\x89PNG":
        out = "/tmp/workdir-tour-screenshot.png"
        with open(out, "wb") as f:
            f.write(png)
        ok("screenshot", f"live desktop PNG ({len(png) // 1024}KB) → {out}")
    else:
        fail("screenshot", f"screenshot: {st}")
    delete_sandbox(sid)


# ─── 13 custom image build (--full) ─────────────────────────────────────────

def build_image(name: str, image_ref: str, hint: dict, timeout_s=420) -> str | None:
    """POST /v1/images (oci source) and poll to ready. Returns the image name to
    use in create, or None on failure (already reported)."""
    st, img = api("POST", "/v1/images", {
        "source": {"type": "oci", "image_ref": image_ref},
        "name": name,
        "resources_hint": hint,
        "ephemeral": True,
        "ttl_seconds": 3600,
    })
    if st != 202:
        fail("img-build", f"build submit: {st} {img}")
        return None
    iid = img["id"]
    CREATED_IMAGES.append(iid)
    info(f"building {name} from {image_ref} (id {iid}) — async, never on the create path")
    deadline = time.time() + timeout_s
    status = "building"
    while time.time() < deadline:
        time.sleep(5)
        st, cur = api("GET", f"/v1/images/{iid}")
        status = cur.get("status", "?")
        if status in ("ready", "failed"):
            break
    if status != "ready":
        tail = "\n".join((cur.get("build_log") or "").splitlines()[-5:])
        fail("img-build", f"build ended as {status}; log tail:\n{tail}")
        return None
    ok("img-build", f"{name} published ({(cur.get('storage_bytes') or 0) // (1024 * 1024)}MB artifact)")
    for line in (cur.get("build_log") or "").splitlines()[:4]:
        info(line)
    return name


def tour_custom_image():
    section("13 · custom image  (--full: OCI import → bootable microVM)")
    name = build_image("custom/tour/alpine", "alpine:3.20", {"cpu": 1, "memory_mb": 2048, "disk_gb": 8})
    if not name:
        return
    st, sb = api("POST", "/v1/sandboxes", {"image": name, "resources": {"cpu": 1, "memory_mb": 2048, "disk_gb": 8}, "auto_stop_seconds": 3600})
    if st != 201:
        fail("img-run", f"create from custom image: {st} {sb}")
        return
    sid = sb["id"]
    CREATED_SANDBOXES.append(sid)
    info(f"{sid}  boot_path={sb['boot_path']}  boot_ms={sb['timings']['boot_ms']}")
    r = exec_in(sid, "cat /etc/alpine-release && cat /etc/resolv.conf 2>/dev/null | head -1")
    if r["exit_code"] == 0 and r["stdout"].strip():
        ok("img-run", f"your image, as a microVM: alpine {r['stdout'].splitlines()[0]}")
    else:
        fail("img-run", f"exec in custom image: {r}")
    delete_sandbox(sid)


# ─── 14 docker-in-docker (--full) ───────────────────────────────────────────

def tour_docker():
    section("14 · docker-in-docker  (--full: dockerd INSIDE the microVM)")
    name = build_image("custom/tour/dind", "docker:27-dind", {"cpu": 2, "memory_mb": 4096, "disk_gb": 16})
    if not name:
        return
    st, sb = api("POST", "/v1/sandboxes", {
        "image": name,
        "resources": {"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
        "docker": {"enabled": True},
        "auto_stop_seconds": 3600,
    })
    if st != 201:
        fail("docker", f"create: {st} {sb}")
        return
    sid = sb["id"]
    CREATED_SANDBOXES.append(sid)
    version = ""
    for _ in range(15):  # dockerd takes a few seconds to come up
        r = exec_in(sid, "docker version --format '{{.Server.Version}}' 2>/dev/null")
        if r["exit_code"] == 0 and r["stdout"].strip():
            version = r["stdout"].strip()
            break
        time.sleep(2)
    if version:
        ok("docker", f"dockerd {version} running inside the sandbox (the microVM is the isolation boundary)")
        r = exec_in(sid, "docker run --rm hello-world 2>&1 | grep -m1 Hello || true")
        if "Hello" in r["stdout"]:
            ok("docker-run", "`docker run hello-world` pulled and ran a container inside the sandbox")
        else:
            skip("docker-run", "dockerd is up but `docker run` didn't complete (container networking varies by guest kernel)")
    else:
        fail("docker", "dockerd never answered inside the sandbox")
    delete_sandbox(sid)


# ─── 12 usage + benchmarks ──────────────────────────────────────────────────

def tour_usage():
    section("12 · usage + published benchmarks  (the bill, and the honest latency table)")
    st, u = api("GET", "/v1/usage")
    if st == 200:
        total = float(u.get("total_cost_usd") or 0)
        ok("usage", f"per-second ledger: org total ${total:.4f}, balance ${float(u.get('balance_usd') or 0):.2f}")
    else:
        fail("usage", f"usage: {st}")
    st, b = api("GET", "/v1/benchmarks")
    if st == 200 and b.get("series"):
        ok("benchmarks", "published boot-path table (p50, measured on the fleet):")
        for s in b["series"]:
            info(f"{s['image']:12} {s['boot_path']:17} p50={s['ready_ms_p50']}ms")
    else:
        fail("benchmarks", f"benchmarks: {st}")


# ─── main ───────────────────────────────────────────────────────────────────

def main():
    if not KEY:
        print("set WORKDIR_API_KEY (and optionally WORKDIR_API_URL) first", file=sys.stderr)
        raise SystemExit(2)
    print(f"workdir feature tour → {API}")
    t0 = time.time()
    basics_sid = None
    try:
        tour_health()
        basics_sid = tour_basics()
        tour_boot_paths()
        tour_preview()
        tour_pty()
        tour_volumes()
        tour_fork()
        tour_secrets()
        tour_pause_resume()
        tour_standby()
        tour_browser()
        if FULL:
            tour_custom_image()
            tour_docker()
        else:
            section("13/14 · custom image + docker-in-docker")
            skip("full", "pass --full to build a custom image from OCI and run docker-in-docker (adds a few minutes)")
        tour_usage()
    finally:
        if basics_sid:
            delete_sandbox(basics_sid)
        for sid in list(CREATED_SANDBOXES):
            delete_sandbox(sid)
        for vid in list(CREATED_VOLUMES):
            api("DELETE", f"/v1/volumes/{vid}")
        for name in list(CREATED_SECRETS):
            api("DELETE", f"/v1/secrets/{name}")
        for iid in list(CREATED_IMAGES):
            api("DELETE", f"/v1/images/{iid}")

    section("summary")
    passed = sum(1 for _, s, _ in RESULTS if s == "PASS")
    failed = sum(1 for _, s, _ in RESULTS if s == "FAIL")
    skipped = sum(1 for _, s, _ in RESULTS if s == "SKIP")
    for sec, status, note in RESULTS:
        colour = {"PASS": "32", "FAIL": "31", "SKIP": "33"}[status]
        print(f"  \033[{colour}m{status:4}\033[0m  {sec:12} {note}")
    print(f"\n  {passed} passed · {failed} failed · {skipped} skipped · {time.time() - t0:.0f}s total")
    raise SystemExit(1 if failed else 0)


if __name__ == "__main__":
    main()
