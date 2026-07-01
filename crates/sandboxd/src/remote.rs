//! `RemoteNodeClient` — the control plane drives a worker node's data plane
//! over the worker's internal HTTP API (see `api::internal`). This is the
//! multi-node counterpart to [`crate::node::LocalNode`]: when the scheduler
//! places a sandbox on another node, the control plane forwards the runtime
//! operations there instead of running them locally.
//!
//! Auth is a shared cluster secret (`node.rpc_token`) sent as `X-Node-Token`.
//! The worker validates it before touching its runtime.

use crate::knobs::Resources;
use crate::node::NodeClient;
use crate::runtime::{DirEntry, ExecRequest, ExecResult, SnapshotArtifact, VmInstance, VmSpec};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::json;
use std::net::SocketAddr;

pub const NODE_TOKEN_HEADER: &str = "X-Node-Token";

pub struct RemoteNodeClient {
    node_id: String,
    base: String, // e.g. "http://10.0.0.5:8080"
    token: String,
    http: reqwest::Client,
}

impl RemoteNodeClient {
    pub fn new(
        node_id: impl Into<String>,
        advertise_addr: impl Into<String>,
        token: impl Into<String>,
        http: reqwest::Client,
    ) -> RemoteNodeClient {
        let mut base = advertise_addr.into();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            base = format!("http://{base}");
        }
        RemoteNodeClient {
            node_id: node_id.into(),
            base: base.trim_end_matches('/').to_string(),
            token: token.into(),
            http,
        }
    }

    async fn post_json(&self, path: &str, body: serde_json::Value) -> Result<serde_json::Value> {
        let res = self
            .http
            .post(format!("{}{path}", self.base))
            .header(NODE_TOKEN_HEADER, &self.token)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("remote node {} {path}", self.node_id))?;
        if !res.status().is_success() {
            let code = res.status();
            let msg = res.text().await.unwrap_or_default();
            return Err(anyhow!(
                "remote node {} {path} -> {code}: {msg}",
                self.node_id
            ));
        }
        Ok(res.json().await.unwrap_or(serde_json::Value::Null))
    }
}

#[async_trait]
impl NodeClient for RemoteNodeClient {
    fn node_id(&self) -> &str {
        &self.node_id
    }

    async fn place(&self, spec: &VmSpec, snapshot_available: bool) -> Result<VmInstance> {
        let v = self
            .post_json(
                "/internal/place",
                json!({ "spec": spec, "snapshot_available": snapshot_available }),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn exec(&self, handle: &str, req: &ExecRequest) -> Result<ExecResult> {
        let v = self
            .post_json("/internal/exec", json!({ "handle": handle, "req": req }))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn write_file(&self, handle: &str, path: &str, bytes: &[u8]) -> Result<()> {
        self.post_json(
            "/internal/write_file",
            json!({ "handle": handle, "path": path, "data_b64": b64(bytes) }),
        )
        .await?;
        Ok(())
    }

    async fn read_file(&self, handle: &str, path: &str) -> Result<Vec<u8>> {
        let res = self
            .http
            .post(format!("{}/internal/read_file", self.base))
            .header(NODE_TOKEN_HEADER, &self.token)
            .json(&json!({ "handle": handle, "path": path }))
            .send()
            .await
            .with_context(|| format!("remote node {} /internal/read_file", self.node_id))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(crate::node::file_not_found(path));
        }
        if !res.status().is_success() {
            let code = res.status();
            let msg = res.text().await.unwrap_or_default();
            return Err(anyhow!(
                "remote node {} /internal/read_file -> {code}: {msg}",
                self.node_id
            ));
        }
        let v = res.json().await.unwrap_or(serde_json::Value::Null);
        let data = v.get("data_b64").and_then(|d| d.as_str()).unwrap_or("");
        unb64(data)
    }

    async fn list_dir(&self, handle: &str, path: &str) -> Result<Vec<DirEntry>> {
        let v = self
            .post_json(
                "/internal/list_dir",
                json!({ "handle": handle, "path": path }),
            )
            .await?;
        Ok(serde_json::from_value(
            v.get("entries").cloned().unwrap_or(json!([])),
        )?)
    }

    async fn expose_port(&self, handle: &str, port: u16) -> Result<SocketAddr> {
        let v = self
            .post_json(
                "/internal/expose_port",
                json!({ "handle": handle, "port": port }),
            )
            .await?;
        let addr = v
            .get("addr")
            .and_then(|a| a.as_str())
            .ok_or_else(|| anyhow!("no addr"))?;
        Ok(addr.parse()?)
    }

    async fn ready_check(&self, handle: &str, url: &str, timeout_seconds: u32) -> Result<()> {
        self.post_json(
            "/internal/ready_check",
            json!({ "handle": handle, "url": url, "timeout_seconds": timeout_seconds }),
        )
        .await?;
        Ok(())
    }

    async fn pause(&self, handle: &str, persist: bool) -> Result<()> {
        self.post_json(
            "/internal/pause",
            json!({ "handle": handle, "persist": persist }),
        )
        .await?;
        Ok(())
    }

    async fn resume(&self, handle: &str) -> Result<u64> {
        let v = self
            .post_json("/internal/resume", json!({ "handle": handle }))
            .await?;
        Ok(v.get("resume_ms").and_then(|m| m.as_u64()).unwrap_or(0))
    }

    async fn standby(&self, handle: &str) -> Result<u64> {
        let v = self
            .post_json("/internal/standby", json!({ "handle": handle }))
            .await?;
        Ok(v.get("standby_ms").and_then(|m| m.as_u64()).unwrap_or(0))
    }

    async fn restore(&self, handle: &str) -> Result<u64> {
        let v = self
            .post_json("/internal/restore", json!({ "handle": handle }))
            .await?;
        Ok(v.get("restore_ms").and_then(|m| m.as_u64()).unwrap_or(0))
    }

    async fn snapshot(&self, handle: &str) -> Result<SnapshotArtifact> {
        let v = self
            .post_json("/internal/snapshot", json!({ "handle": handle }))
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn fork(&self, parent_handle: &str, child_spec: &VmSpec) -> Result<VmInstance> {
        let v = self
            .post_json(
                "/internal/fork",
                json!({ "parent_handle": parent_handle, "spec": child_spec }),
            )
            .await?;
        Ok(serde_json::from_value(v)?)
    }

    async fn delete(&self, handle: &str) -> Result<()> {
        self.post_json("/internal/delete", json!({ "handle": handle }))
            .await?;
        Ok(())
    }

    async fn hot_pool_available(&self, image_key: &str, resources: &Resources) -> u32 {
        self.post_json(
            "/internal/hot_pool_available",
            json!({ "image_key": image_key, "resources": resources }),
        )
        .await
        .ok()
        .and_then(|v| v.get("count").and_then(|c| c.as_u64()))
        .unwrap_or(0) as u32
    }
}

// Minimal base64 (std-only) for file payloads over JSON.
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn b64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub(crate) fn unb64(s: &str) -> Result<Vec<u8>> {
    let mut rev = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        rev[c as usize] = i as u8;
    }
    let clean: Vec<u8> = s
        .bytes()
        .filter(|&c| c != b'=' && !c.is_ascii_whitespace())
        .collect();
    let mut out = Vec::with_capacity(clean.len() / 4 * 3);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = rev[c as usize];
            if v == 255 {
                return Err(anyhow!("invalid base64"));
            }
            n = (n << 6) | v as u32;
            bits += 6;
        }
        n <<= 24 - bits;
        let nbytes = (bits) / 8;
        for i in 0..nbytes {
            out.push((n >> (16 - i * 8)) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn base64_roundtrip() {
        for input in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"abcd",
            b"hello world \x00\xff\xfe",
        ] {
            assert_eq!(unb64(&b64(input)).unwrap(), input, "roundtrip {input:?}");
        }
    }
}
