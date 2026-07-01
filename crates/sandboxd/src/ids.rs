//! Short, prefixed identifiers (e.g. `sbx_3f9a1c2b`) matching the spec's
//! `sbx_123` style, plus opaque tokens for API keys and node join.

use rand::Rng;

fn rand_hex(bytes: usize) -> String {
    let mut rng = rand::thread_rng();
    let mut s = String::with_capacity(bytes * 2);
    for _ in 0..bytes {
        s.push_str(&format!("{:02x}", rng.gen::<u8>()));
    }
    s
}

pub fn sandbox_id() -> String {
    format!("sbx_{}", rand_hex(6))
}

pub fn node_id() -> String {
    format!("node_{}", rand_hex(4))
}

pub fn image_id() -> String {
    format!("img_{}", rand_hex(6))
}

pub fn snapshot_id() -> String {
    format!("snap_{}", rand_hex(6))
}

pub fn build_id() -> String {
    format!("build_{}", rand_hex(6))
}

pub fn command_id() -> String {
    format!("cmd_{}", rand_hex(6))
}

pub fn volume_id() -> String {
    format!("vol_{}", rand_hex(6))
}

/// ext4 label for a volume's backing image (≤16 bytes), so the guest can mount
/// it by `LABEL=` regardless of which `/dev/vdX` Firecracker assigns it.
/// `vol_a1b2c3d4d5e6` → `wdvb2c3d4d5e6` (drop the `vol_` prefix, prefix `wdv`).
pub fn volume_label(volume_id: &str) -> String {
    let hex = volume_id.strip_prefix("vol_").unwrap_or(volume_id);
    format!("wdv{}", &hex[..hex.len().min(13)])
}

/// API key shown once to the caller. Stored only as a SHA-256 hash.
pub fn api_key() -> String {
    format!("sk_live_{}", rand_hex(24))
}

/// Node join token. Single control-plane secret, rotatable.
pub fn join_token() -> String {
    format!("jt_{}", rand_hex(24))
}
