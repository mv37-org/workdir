# Extended Features

Capabilities added on top of the base spec: secret management, docker-in-docker,
S3 bucket mounts, ephemeral files/images, coding agent, persistent volumes, and
create-time network egress controls. All are opt-in and preserve the cheap
default path (a no-option `create()` is unchanged).

---

## 1. Secret management

Org-scoped secrets, **encrypted at rest** with AES-256-GCM. The encryption key
is kept out of the database (env `WORKDIR_SECRET_KEY` base64-32-bytes, or a
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
(`CONFIG_OVERLAY_FS`), and cgroups v2. On boot the runtime starts `dockerd` on
`unix:///var/run/docker.sock` and waits for the socket. After that,
`docker build` / `docker run` work normally inside the sandbox via `exec`.

**iptables backend.** The stock Firecracker guest kernel has **legacy iptables**
(`IP_NF_IPTABLES`) + bridge + veth, but **not** `nf_tables`. Modern images
(`docker:dind` on Alpine) default iptables to the *nft* backend, so dockerd's
bridge/NAT setup would die with `iptables: Failed to initialize nft: Protocol not
supported`. Before starting the daemon the runtime repoints the iptables family
at the **legacy** backend the kernel supports (`update-alternatives` on Debian,
the `xtables-legacy-multi` symlink on Alpine) and enables `ip_forward`. dockerd
then comes up with its **normal bridge networking** — validated on the node:
`docker run hello-world` *and* a default-bridge container reaching the internet,
no kernel rebuild required.

> Nested KVM is **not** required — containers share the guest kernel. The only
> host requirement is the same `/dev/kvm` Firecracker already needs.

### Building a docker-capable custom image

`docker:dind` (or any image bundling `dockerd`) works directly:

```bash
# 1. publish it
POST /v1/images { "source": {"type":"oci","image_ref":"docker:27-dind"},
                  "name": "custom/acme/dind",
                  "resources_hint": {"cpu":2,"memory_mb":4096,"disk_gb":16} }
# 2. run it with dockerd auto-started
POST /v1/sandboxes { "image":"custom/acme/dind",
                     "resources":{"cpu":2,"memory_mb":4096,"disk_gb":16},
                     "docker":{"enabled":true} }
```

The image builder injects a **statically-linked** guest agent, so musl-based
images (alpine, `docker:dind`) boot fine — a glibc agent would fail to exec and
panic the guest.

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

## 5. Coding agent (opt-in)

Install a lightweight in-sandbox coding-agent CLI ([opencode](https://opencode.ai))
so an agent can read, write, and run code directly inside the sandbox.

```jsonc
{
  "coding_agent": { "enabled": true },
  "startup": { "secrets": ["ANTHROPIC_API_KEY"] }
}
```

It is **opt-in by design**: the agent is **not** baked into the base rootfs, so
the common case stays minimal (smaller images, smaller attack surface, fewer
CVEs to track, faster hot-pool warming). When requested, the runtime installs it
into the guest at provision time and records the install time separately as
`timings.agent_ms`. The installed agent is reflected on the record as
`"coding_agent": "opencode"`.

| Field | Meaning | Default |
|---|---|---|
| `enabled` | Opt in to the agent | `false` |
| `kind` | Which agent CLI; only `opencode` today | `opencode` |
| `version` | Pin a specific version | installer's latest |

The agent needs a provider key to be useful — supply one through the normal
secret path (`startup.secrets`, e.g. `ANTHROPIC_API_KEY`), so it inherits the
encrypted-at-rest + never-snapshotted guarantees. An unknown `kind` is rejected
with a `400`. Works on any curated image (all ship `curl` + default egress).

> The dev (`mock`) runtime records the intent but does **not** run the network
> install; real installation happens only on the Firecracker runtime.

---

## 6. Persistent volumes

Attach org-scoped block storage that survives sandbox deletion, so workspace
state can move from one sandbox session to the next.

Create the volume once:

```bash
curl -X POST $API/v1/volumes -H "$AUTH" \
  -d '{"name":"project-cache","size_gb":20}'
```

Attach it at sandbox create:

```jsonc
{
  "volumes": [
    { "volume_id": "vol_...", "mount_path": "/mnt/project" }
  ]
}
```

A volume attaches to at most one running sandbox at a time, refuses deletion
while attached, detaches when the sandbox is deleted, and can be reattached to a
later sandbox. On Firecracker it is a labelled ext4 backing image staged into
the jailer chroot; the mock runtime simulates the same flow with host
directories. Attaching a volume forces a cold boot because the extra virtio drive
is configured before VM start, and fork is refused while volumes are attached.

---

## 7. Network egress controls

Attach a create-time egress policy to a sandbox under `startup.network`.
Omitting it keeps the backward-compatible default internet egress.

```jsonc
{
  "startup": {
    "network": {
      "egress": "allowlist",
      "allow": [
        { "type": "domain", "value": "api.openai.com", "protocol": "tcp", "ports": [443] },
        { "type": "cidr", "value": "93.184.216.34/32", "protocol": "tcp", "ports": [443] }
      ]
    }
  }
}
```

Modes:

| Mode | Meaning |
|---|---|
| `default` | Default internet egress with baseline metadata/private/SMTP blocks. |
| `none` | Drop all forwarded sandbox egress. |
| `allowlist` | Permit only listed CIDR/IP/domain rules. |
| `denylist` | Drop listed CIDR/IP/domain rules and allow the rest. |

Rules can be object form or simple strings (`"api.example.com"`,
`"93.184.216.34"`, `"203.0.113.0/24"`). Domain policies use the host DNS proxy
and dynamic nftables sets, and alternate DNS is blocked for explicit domain
policies. URLs, paths, raw wildcards, invalid ports, IPv6 rules, and private or
metadata allowlist ranges are rejected at create time.

Hard-deny safety boundaries remain unreachable even if listed: metadata,
link-local, sandbox CIDR, RFC1918/private ranges, IPv6 forwarding, and outbound
SMTP.

> The dev (`mock`) runtime persists and reports the policy but does not install
> nftables rules. Real enforcement happens on the Firecracker runtime.

---

## Create request — full option surface

```jsonc
{
  "image": "base | node-python | browser | heavy-build | custom/<org>/<name>",
  "image_version": "2026-06-10-ab12cd",
  "resources": { "cpu": 0.5|1|2|4, "memory_mb": 512|1024|2048|4096|8192|16384, "disk_gb": 8|16|32|64 },
  "auto_stop_seconds": 30..3600,
  "snapshot": false,
  "browser": { "enabled": true, "vnc": true, "cdp": true },
  "docker":  { "enabled": true },
  "coding_agent": { "enabled": true, "kind": "opencode", "version": "latest" },
  "secrets-> via startup.secrets": [],
  "mounts":  [{ "type": "s3", "bucket": "...", "mount_path": "/mnt/...", "read_only": true }],
  "volumes": [{ "volume_id": "vol_...", "mount_path": "/mnt/project" }],
  "files":   [{ "path": "...", "content": "...", "encoding": "utf8|base64" }],
  "startup": { "git": {...}, "env": {...}, "secrets": [...], "commands": [...],
               "ports": [...], "ready": {...}, "network": { "egress": "default|none|allowlist|denylist" } }
}
```

## Installer / image-build implications

For production use of these features, the curated/custom rootfs images need the
corresponding tooling baked in (added to the §10.3 image-build pipeline):

- **docker-capable image**: `dockerd`, `containerd`, `runc`, overlayfs kernel, `iptables`.
- **s3-mount support**: the `mount-s3` binary + FUSE.
- **coding agent**: nothing required in the rootfs — it is installed on demand
  over the network. To cut the per-create `agent_ms` install cost, pre-stage the
  `opencode` binary in a layered image and the install step becomes a no-op.

Secret management needs no image changes (host-side). Set
`WORKDIR_SECRET_KEY` (or back up the generated `data_dir/secret.key`) so
secrets survive a node rebuild.
