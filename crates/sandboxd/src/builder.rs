//! Production custom-image builder (spec §10.3, §11): turn an OCI reference or
//! a Dockerfile build into a bootable Firecracker rootfs with the guest agent
//! injected, published under `images_dir/custom/`. Runs on the node via the
//! docker CLI the installer ships; the mock runtime keeps the simulated
//! pipeline in `api::images`.
//!
//! Custom images carry arbitrary userlands, so the injected init does NOT
//! depend on socat (the curated images' vsock↔stdio bridge): the guest agent
//! itself listens on AF_VSOCK (`--vsock-listen`). Networking is best-effort —
//! it needs an `ip` binary (busybox or iproute2) in the image; exec, files,
//! and the PTY work either way because they ride vsock, not the NIC.

use crate::images::{ImageSource, ImageStatus};
use crate::state::AppState;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Init injected at /sbin/sandbox-init. Mirrors the curated init (mounts,
/// cmdline-driven eth0, workspace dir) minus the socat bridge, plus a cgroup2
/// mount so docker-capable images can run dockerd.
const CUSTOM_INIT: &str = r#"#!/bin/sh
# workdir custom-image init (injected at build time).
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sysfs /sys 2>/dev/null
mount -t devtmpfs devtmpfs /dev 2>/dev/null
mkdir -p /dev/pts && mount -t devpts devpts /dev/pts 2>/dev/null
mkdir -p /sys/fs/cgroup && mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null
mkdir -p /workspace /run /tmp

ip link set lo up 2>/dev/null
WDIP=""; WDGW=""; WDDNS="1.1.1.1"
for tok in $(cat /proc/cmdline); do
  case "$tok" in
    wd.ip=*)  WDIP=${tok#wd.ip=} ;;
    wd.gw=*)  WDGW=${tok#wd.gw=} ;;
    wd.dns=*) WDDNS=${tok#wd.dns=} ;;
  esac
done
if [ -n "$WDIP" ] && command -v ip >/dev/null 2>&1; then
  i=0; while [ $i -lt 20 ] && ! ip link show eth0 >/dev/null 2>&1; do i=$((i+1)); sleep 0.1; done
  ip addr add "$WDIP/16" dev eth0 2>/dev/null
  ip link set eth0 up 2>/dev/null
  ip route add default via "$WDGW" 2>/dev/null
  printf 'nameserver %s\n' "$WDDNS" > /etc/resolv.conf 2>/dev/null
fi

exec /usr/local/bin/sandbox-guest-agent --vsock-listen 5005
"#;

/// Drive one image build to `ready` or `failed`, appending honest step logs to
/// the image record as it goes (spec §11.3 "expose image build logs").
pub async fn build(state: AppState, image_id: String) {
    let mut log = String::new();
    let t0 = Instant::now();
    let result = build_inner(&state, &image_id, &mut log).await;
    if let Ok(Some(mut img)) = state.store.get_image(&image_id) {
        match result {
            Ok(bytes) => {
                img.status = ImageStatus::Ready;
                img.storage_bytes = bytes;
                img.first_node_cache_miss_ms = Some(t0.elapsed().as_millis() as u64);
                log.push_str(&format!("published {}\n", img.reference()));
                tracing::info!(image = %img.reference(), bytes, "custom image published");
            }
            Err(e) => {
                img.status = ImageStatus::Failed;
                log.push_str(&format!("BUILD FAILED: {e:#}\n"));
                tracing::warn!(image = %img.name, error = %e, "custom image build failed");
            }
        }
        img.build_log = log;
        img.updated_at = Utc::now();
        state.store.put_image(&img).ok();
    }
}

async fn build_inner(state: &AppState, image_id: &str, log: &mut String) -> Result<u64> {
    let img = state
        .store
        .get_image(image_id)
        .ok()
        .flatten()
        .context("image record disappeared")?;

    sh(state, image_id, log, "docker available", "docker version --format {{.Server.Version}}")
        .await
        .context("the node's image builder needs the docker CLI (the installer ships it)")?;

    let work = state.cfg.server.data_dir.join("builds").join(image_id);
    let rootfs = work.join("rootfs");
    std::fs::create_dir_all(&rootfs).context("create build workspace")?;

    // 1. Materialize the container image to export.
    let export_ref = match &img.source {
        ImageSource::Oci { image_ref } => {
            sh(state, image_id, log, &format!("pull {image_ref}"), &format!("docker pull {}", q(image_ref))).await?;
            image_ref.clone()
        }
        ImageSource::Dockerfile { context_url, dockerfile } => {
            let ctx = work.join("ctx");
            std::fs::create_dir_all(&ctx).context("create context dir")?;
            let tarball = work.join("context.tar");
            sh(state, image_id, log, "fetch build context", &format!("curl -fsSL -o {} {}", q(&tarball.to_string_lossy()), q(context_url))).await?;
            sh(state, image_id, log, "unpack build context", &format!("tar -xaf {} -C {}", q(&tarball.to_string_lossy()), q(&ctx.to_string_lossy()))).await?;
            // GitHub-style archives nest everything under one top-level dir.
            let build_dir = single_subdir(&ctx).unwrap_or(ctx);
            let tag = format!("workdir-build-{image_id}");
            sh(state, image_id, log, "docker build", &format!(
                "docker build -f {} -t {} {}",
                q(&build_dir.join(dockerfile).to_string_lossy()),
                q(&tag),
                q(&build_dir.to_string_lossy()),
            ))
            .await?;
            tag
        }
    };

    // 2. Flatten it to a root filesystem (docker export = the merged layers).
    let cid = sh_capture(&format!("docker create {}", q(&export_ref))).await.context("docker create")?;
    let cid = cid.trim().to_string();
    let export = sh(state, image_id, log, "export rootfs", &format!("docker export {} | tar -x -C {}", q(&cid), q(&rootfs.to_string_lossy()))).await;
    let _ = sh_capture(&format!("docker rm {}", q(&cid))).await;
    export?;
    if let ImageSource::Dockerfile { .. } = &img.source {
        let _ = sh_capture(&format!("docker rmi workdir-build-{image_id}")).await;
    }

    // 3. Inject the guest agent + the socat-free init.
    //    Prefer a STATIC (musl) agent: custom images carry arbitrary userlands,
    //    and a glibc-linked agent can't exec on a musl image (alpine, dind) —
    //    PID 1 would die and panic the guest. A static binary runs on any libc.
    //    Fall back to the dynamic agent only if no static one is staged (it
    //    works for glibc-based custom images).
    let static_agent = state.cfg.server.data_dir.join("sandbox-guest-agent-static");
    let dynamic_agent = state.cfg.server.data_dir.join("sandbox-guest-agent");
    let agent_src = if static_agent.exists() { static_agent } else { dynamic_agent };
    if !agent_src.exists() {
        bail!("guest agent binary missing at {agent_src:?} (the installer stages it)");
    }
    if !agent_src.to_string_lossy().ends_with("-static") {
        log.push_str("[      ] warning: injecting the dynamic (glibc) agent — musl images (alpine) will not boot; stage sandbox-guest-agent-static\n");
    }
    std::fs::create_dir_all(rootfs.join("usr/local/bin")).ok();
    std::fs::create_dir_all(rootfs.join("sbin")).ok();
    std::fs::copy(&agent_src, rootfs.join("usr/local/bin/sandbox-guest-agent")).context("inject guest agent")?;
    std::fs::write(rootfs.join("sbin/sandbox-init"), CUSTOM_INIT).context("inject init")?;
    for p in ["usr/local/bin/sandbox-guest-agent", "sbin/sandbox-init"] {
        let _ = sh_capture(&format!("chmod 755 {}", q(&rootfs.join(p).to_string_lossy()))).await;
    }
    step(state, image_id, log, "inject guest agent + init");

    // 4. Pack a labelled ext4. The per-VM COW copy of this file IS the
    //    sandbox's writable disk, so size it for the hint, not just content.
    let content_mb: u64 = sh_capture(&format!("du -sm {} | cut -f1", q(&rootfs.to_string_lossy())))
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(1024);
    let hint_gb = img.resources_hint.disk_gb.unwrap_or(0) as u64;
    let size_gb = hint_gb.max(content_mb * 13 / 10 / 1024 + 1).max(4);
    let tmp_out = work.join("rootfs.ext4");
    let f = std::fs::File::create(&tmp_out).context("create image file")?;
    f.set_len(size_gb * 1024 * 1024 * 1024).context("size image file")?;
    drop(f);
    sh(state, image_id, log, &format!("mkfs.ext4 ({size_gb}G, content {content_mb}MB)"), &format!(
        "mkfs.ext4 -F -q -d {} -L wd-custom {}",
        q(&rootfs.to_string_lossy()),
        q(&tmp_out.to_string_lossy()),
    ))
    .await?;

    // 5. Publish where the runtime resolves it (rootfs_path: name with '/'
    //    and ':' flattened). Status flips to ready only after this lands, so a
    //    create can never see a half-written artifact.
    let safe = img.name.replace(['/', ':'], "_");
    let out_dir = state.cfg.runtime.images_dir.join("custom");
    std::fs::create_dir_all(&out_dir).context("create custom images dir")?;
    let out = out_dir.join(format!("{safe}.ext4"));
    std::fs::rename(&tmp_out, &out).or_else(|_| std::fs::copy(&tmp_out, &out).map(|_| ())).context("publish artifact")?;
    step(state, image_id, log, &format!("publish {}", out.display()));

    // Sparse-aware size: what the artifact actually occupies, not its length.
    use std::os::unix::fs::MetadataExt;
    let bytes = std::fs::metadata(&out).map(|m| m.blocks() * 512).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&work);
    Ok(bytes)
}

/// Run one shell step, append its outcome (and stderr on failure) to the build
/// log, and persist the log so `GET /v1/images/:id` shows live progress.
async fn sh(state: &AppState, image_id: &str, log: &mut String, what: &str, cmd: &str) -> Result<()> {
    let t0 = Instant::now();
    let out = tokio::process::Command::new("sh")
        .args(["-c", cmd])
        .output()
        .await
        .with_context(|| format!("spawn: {what}"))?;
    let ms = t0.elapsed().as_millis();
    if out.status.success() {
        log.push_str(&format!("[{ms:>6}ms] {what}\n"));
        flush(state, image_id, log);
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        log.push_str(&format!("[{ms:>6}ms] {what} FAILED\n{}\n", err.trim()));
        flush(state, image_id, log);
        bail!("{what}: {}", err.lines().last().unwrap_or("(no stderr)").to_string())
    }
}

async fn sh_capture(cmd: &str) -> Result<String> {
    let out = tokio::process::Command::new("sh").args(["-c", cmd]).output().await?;
    if !out.status.success() {
        bail!("command failed: {cmd}");
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn step(state: &AppState, image_id: &str, log: &mut String, what: &str) {
    log.push_str(&format!("[      ] {what}\n"));
    flush(state, image_id, log);
}

fn flush(state: &AppState, image_id: &str, log: &str) {
    if let Ok(Some(mut img)) = state.store.get_image(image_id) {
        img.build_log = log.to_string();
        img.updated_at = Utc::now();
        state.store.put_image(&img).ok();
    }
}

/// If `dir` contains exactly one entry and it is a directory, return it.
fn single_subdir(dir: &Path) -> Option<PathBuf> {
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().collect();
    match entries.as_slice() {
        [one] if one.file_type().ok()?.is_dir() => Some(one.path()),
        _ => None,
    }
}

/// Shell-quote a value.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
