# Extended Features

Four capabilities added on top of the base spec: secret management,
docker-in-docker, S3 bucket mounts, and ephemeral files/images. All are opt-in
and preserve the cheap default path (a no-option `create()` is unchanged).

---

## 1. Secret management

Org-scoped secrets, **encrypted at rest** with AES-256-GCM. The encryption key
is kept out of the database (env `SANDBOXD_SECRET_KEY` base64-32-bytes, or a
`0600` `secret.key` file under `data_dir`, generated on first boot). Plaintext
values are never returned over the API, never logged, and never written into a
snapshot.

```bash
# store / list (names only) / delete
curl -X PUT  $API/v1/secrets/OPENAI_API_KEY -H "$AUTH" -d '{"value":"sk-..."}'
curl         $API/v1/secrets                -H "$AUTH"     # -> [{"name":"OPENAI_API_KEY",...}]
curl -X DELETE $API/v1/secrets/OPENAI_API_KEY -H "$AUTH"
```

Reference a secret by name in a sandbox; the value is decrypted and injected
**after assignment**, available to every `exec`:

```jsonc
{ "startup": { "secrets": ["OPENAI_API_KEY"] } }
```

Injection is per-exec from host memory (the Firecracker runtime never writes
secrets to a guest file), so a sandbox with resident secrets **cannot be
snapshotted** — `POST /snapshot` returns `409` until the secrets are removed.

> Production: swap [`secrets::load_or_create_key`](../crates/sandboxd/src/secrets.rs)
> for a KMS / sealed-secret integration. Nothing else changes.

---

## 2. Docker-in-Docker

`dockerd` runs **inside the guest microVM**. The microVM is the isolation
boundary — the host Docker socket is **never** exposed to a sandbox (spec §18).

```jsonc
{
  "image": "heavy-build",
  "resources": { "cpu": 2, "memory_mb": 8192, "disk_gb": 32 },
  "docker": { "enabled": true }
}
```

The base image deliberately ships **without** a Docker daemon (spec §10.2), so
DinD requires a docker-capable image (`heavy-build` or a custom image) — the API
rejects `docker.enabled` on `base` with a `400`.

**Guest requirements** (baked into the docker-capable rootfs at image-build
time): `dockerd` + `containerd` + `runc`, an overlayfs-capable guest kernel
(`CONFIG_OVERLAY_FS`), cgroups v2, and `iptables`. On boot the runtime starts
`dockerd` on `unix:///var/run/docker.sock` and waits for the socket. After that,
`docker build` / `docker run` work normally inside the sandbox via `exec`.

> Nested KVM is **not** required — containers share the guest kernel. The only
> host requirement is the same `/dev/kvm` Firecracker already needs.

---

## 3. S3 bucket mounts

Mount an S3 (or S3-compatible: MinIO, Cloudflare R2) bucket into the guest via
`mountpoint-s3`.

```jsonc
{
  "startup": { "secrets": ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"] },
  "mounts": [
    { "type": "s3", "bucket": "my-data", "prefix": "datasets/",
      "mount_path": "/mnt/data", "read_only": true,
      "region": "us-east-1", "endpoint": "https://s3.us-east-1.amazonaws.com" }
  ]
}
```

Credentials are **not** inline — they come from the sandbox's injected secret
env (`AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`), so they inherit the
encrypted-at-rest + never-snapshotted guarantees above. The mount spec on the
persisted record carries no credentials.

**Guest requirement**: `mount-s3` (the [mountpoint-s3](https://github.com/awslabs/mountpoint-s3)
binary, FUSE-based) in the rootfs. The runtime runs
`mount-s3 <bucket> <mount_path> [--prefix …] [--read-only] [--region …] [--endpoint-url …]`
with the AWS creds in the command environment.

> The dev (`mock`) runtime simulates the mount by creating the directory; real
> mounting happens only on the Firecracker runtime inside the guest.

---

## 4. Ephemeral files and images

**Ephemeral files** — inline files written into the workspace at boot, living
only for the session (wiped on delete, not snapshotted). Saves a round-trip of
`PUT /files` calls and keeps transient inputs out of any image:

```jsonc
{
  "files": [
    { "path": "config.json", "content": "{\"k\":1}" },
    { "path": "data/seed.bin", "content": "AAEC", "encoding": "base64" }
  ]
}
```

**Ephemeral images** — a custom image that is built, used, then
garbage-collected, so one-off builds don't accumulate in the registry or its
storage bill:

```jsonc
// POST /v1/images
{
  "source": { "type": "dockerfile", "context_url": "https://…/ctx.tar.gz" },
  "name": "custom/acme/throwaway",
  "ephemeral": true,
  "ttl_seconds": 3600
}
```

A background GC sweep soft-deletes ephemeral images past `expires_at` **once no
active sandbox references them** (running sandboxes are never disrupted, spec
§25.3). Soft delete blocks new creates but leaves existing sandboxes alone.

---

## Create request — full option surface

```jsonc
{
  "image": "base | node-python | browser | heavy-build | custom/<org>/<name>",
  "image_version": "2026-06-10-ab12cd",
  "resources": { "cpu": 0.5|1|2|4, "memory_mb": 1024|2048|4096|8192|16384, "disk_gb": 8|16|32|64 },
  "auto_stop_seconds": 30..3600,
  "snapshot": false,
  "browser": { "enabled": true, "vnc": true, "cdp": true },
  "docker":  { "enabled": true },
  "secrets-> via startup.secrets": [],
  "mounts":  [{ "type": "s3", "bucket": "...", "mount_path": "/mnt/...", "read_only": true }],
  "files":   [{ "path": "...", "content": "...", "encoding": "utf8|base64" }],
  "startup": { "git": {...}, "env": {...}, "secrets": [...], "commands": [...],
               "ports": [...], "ready": {...} }
}
```

## Installer / image-build implications

For production use of these features, the curated/custom rootfs images need the
corresponding tooling baked in (added to the §10.3 image-build pipeline):

- **docker-capable image**: `dockerd`, `containerd`, `runc`, overlayfs kernel, `iptables`.
- **s3-mount support**: the `mount-s3` binary + FUSE.

Secret management needs no image changes (host-side). Set
`SANDBOXD_SECRET_KEY` (or back up the generated `data_dir/secret.key`) so
secrets survive a node rebuild.
