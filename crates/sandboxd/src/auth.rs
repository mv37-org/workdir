//! API-key authentication (spec §6.1 "Auth and API keys", §18 kill switches).
//!
//! Keys are presented as `Authorization: Bearer sk_live_...`. We store only the
//! SHA-256 hash; the plaintext is shown once at creation.

use crate::store::Store;
use crate::usage::{ApiKey, Org, OrgStatus};
use sha2::{Digest, Sha256};

pub fn hash_key(plaintext: &str) -> String {
    let mut h = Sha256::new();
    h.update(plaintext.as_bytes());
    hex::encode(h.finalize())
}

/// Resolved identity for a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub org_id: String,
    pub admin: bool,
}

#[derive(Debug)]
pub enum AuthOutcome {
    Ok(AuthContext),
    Missing,
    Invalid,
    /// Org suspended via kill switch (spec §18).
    Suspended,
}

/// Verify a presented bearer token against the store.
pub fn authenticate(store: &Store, bearer: Option<&str>) -> AuthOutcome {
    let token = match bearer {
        Some(t) if !t.is_empty() => t,
        _ => return AuthOutcome::Missing,
    };
    let hash = hash_key(token);
    let key: ApiKey = match store.get_api_key(&hash) {
        Ok(Some(k)) => k,
        _ => return AuthOutcome::Invalid,
    };
    if key.disabled {
        return AuthOutcome::Invalid;
    }
    match store.get_org(&key.org_id) {
        Ok(Some(org)) if org.status == OrgStatus::Suspended => AuthOutcome::Suspended,
        Ok(Some(_)) => AuthOutcome::Ok(AuthContext { org_id: key.org_id, admin: key.admin }),
        _ => AuthOutcome::Invalid,
    }
}

/// Seed the bootstrap org + admin key on first boot. Returns the plaintext key
/// if a new one was generated (so the installer can print it once).
pub fn bootstrap(
    store: &Store,
    org_id: &str,
    provided_key: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if store.get_org(org_id)?.is_none() {
        store.put_org(&Org {
            id: org_id.to_string(),
            name: "admin".to_string(),
            status: OrgStatus::Active,
            prepaid_credits_usd: 1_000_000_000.0, // admin org is effectively uncapped
            spent_usd: 0.0,
            quota_units: 0.0, // convention: <= 0 means unlimited (see Org::quota_unlimited)
            created_at: chrono::Utc::now(),
        })?;
    }
    // If an admin key already exists for this org, do nothing.
    // (We can't enumerate by org cheaply; instead store a marker in meta.)
    let marker = format!("admin_key_seeded:{org_id}");
    if store.get_meta(&marker)?.is_some() {
        return Ok(None);
    }
    let plaintext = provided_key
        .filter(|k| !k.is_empty())
        .map(|k| k.to_string())
        .unwrap_or_else(crate::ids::api_key);
    store.put_api_key(&ApiKey {
        key_hash: hash_key(&plaintext),
        org_id: org_id.to_string(),
        name: "bootstrap-admin".to_string(),
        admin: true,
        disabled: false,
        created_at: chrono::Utc::now(),
    })?;
    store.set_meta(&marker, "1")?;
    Ok(Some(plaintext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashing_is_stable() {
        assert_eq!(hash_key("abc"), hash_key("abc"));
        assert_ne!(hash_key("abc"), hash_key("abd"));
    }

    #[test]
    fn bootstrap_then_authenticate() {
        let store = Store::open_in_memory().unwrap();
        let key = bootstrap(&store, "org_admin", None).unwrap().unwrap();
        match authenticate(&store, Some(&key)) {
            AuthOutcome::Ok(ctx) => {
                assert_eq!(ctx.org_id, "org_admin");
                assert!(ctx.admin);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        assert!(matches!(authenticate(&store, Some("nope")), AuthOutcome::Invalid));
        assert!(matches!(authenticate(&store, None), AuthOutcome::Missing));
    }
}
