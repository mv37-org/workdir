//! Org-scoped secret management.
//!
//! Secrets are encrypted at rest with AES-256-GCM. The encryption key is kept
//! **separate from the database** (env var or a 0600 key file under data_dir),
//! so a stolen DB file does not reveal secret values. Plaintext values are only
//! ever returned internally to inject into a sandbox after assignment — never
//! over the API, never into logs, and never into snapshots (review M3).
//!
//! For a production deployment, replace [`load_or_create_key`] with a KMS /
//! sealed-secret integration; the rest of the code is unchanged.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{AeadCore, Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Encrypted secret as stored. Never contains plaintext.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretRecord {
    pub org_id: String,
    pub name: String,
    /// base64(nonce) — 12 bytes.
    pub nonce_b64: String,
    /// base64(ciphertext+tag).
    pub ciphertext_b64: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Load the master key from `SANDBOXD_SECRET_KEY` (base64, 32 bytes) if set,
/// else from `<data_dir>/secret.key`, generating one (0600) on first boot.
pub fn load_or_create_key(data_dir: &Path) -> Result<[u8; 32]> {
    if let Ok(b64) = std::env::var("SANDBOXD_SECRET_KEY") {
        let bytes = b64_decode(&b64).map_err(|e| anyhow!("bad SANDBOXD_SECRET_KEY: {e}"))?;
        if bytes.len() != 32 {
            bail!("SANDBOXD_SECRET_KEY must decode to 32 bytes, got {}", bytes.len());
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    let path = data_dir.join("secret.key");
    if path.exists() {
        let b64 = std::fs::read_to_string(&path).context("read secret.key")?;
        let bytes = b64_decode(b64.trim()).map_err(|e| anyhow!("corrupt secret.key: {e}"))?;
        if bytes.len() != 32 {
            bail!("secret.key must be 32 bytes");
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    // Generate a fresh key and persist it with restrictive permissions.
    let key = Aes256Gcm::generate_key(OsRng);
    std::fs::create_dir_all(data_dir).ok();
    std::fs::write(&path, b64_encode(key.as_slice())).context("write secret.key")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(key.as_slice());
    Ok(out)
}

pub fn encrypt(key: &[u8; 32], org_id: &str, name: &str, plaintext: &str) -> Result<SecretRecord> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(OsRng);
    let ct = cipher
        .encrypt(&nonce, plaintext.as_bytes())
        .map_err(|_| anyhow!("encryption failed"))?;
    let now = Utc::now();
    Ok(SecretRecord {
        org_id: org_id.to_string(),
        name: name.to_string(),
        nonce_b64: b64_encode(nonce.as_slice()),
        ciphertext_b64: b64_encode(&ct),
        created_at: now,
        updated_at: now,
    })
}

pub fn decrypt(key: &[u8; 32], rec: &SecretRecord) -> Result<String> {
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce_bytes = b64_decode(&rec.nonce_b64).map_err(|e| anyhow!("{e}"))?;
    let ct = b64_decode(&rec.ciphertext_b64).map_err(|e| anyhow!("{e}"))?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    let pt = cipher.decrypt(nonce, ct.as_slice()).map_err(|_| anyhow!("decryption failed"))?;
    String::from_utf8(pt).context("secret is not valid UTF-8")
}

/// A valid secret name: env-var-like, so it can be injected as `$NAME`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_').unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// --- base64 (shared alphabet) ------------------------------------------------

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64_encode(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(B64[(b[0] >> 2) as usize] as char);
        out.push(B64[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn b64_decode(input: &str) -> std::result::Result<Vec<u8>, String> {
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = table[c as usize];
            if v == 255 {
                return Err("invalid base64".into());
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        let bytes = bits / 8;
        acc <<= 24 - bits;
        for i in 0..bytes {
            out.push((acc >> (16 - i * 8)) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let key = [7u8; 32];
        let rec = encrypt(&key, "org_1", "API_KEY", "s3cr3t-value").unwrap();
        assert!(!rec.ciphertext_b64.contains("s3cr3t"));
        assert_eq!(decrypt(&key, &rec).unwrap(), "s3cr3t-value");
    }

    #[test]
    fn wrong_key_fails() {
        let rec = encrypt(&[1u8; 32], "o", "N", "v").unwrap();
        assert!(decrypt(&[2u8; 32], &rec).is_err());
    }

    #[test]
    fn name_validation() {
        assert!(valid_name("OPENAI_API_KEY"));
        assert!(valid_name("_x"));
        assert!(!valid_name("1bad"));
        assert!(!valid_name("has-dash"));
        assert!(!valid_name(""));
    }
}
