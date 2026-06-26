//! Shared application state handed to every request handler.

use crate::config::Config;
use crate::node::{LocalNode, NodeClient};
use crate::remote::RemoteNodeClient;
use crate::store::Store;
use std::sync::Arc;

pub struct Inner {
    pub cfg: Config,
    pub store: Store,
    pub local: Arc<LocalNode>,
    pub local_node_id: String,
    pub http: reqwest::Client,
    /// AES-256 master key for secret encryption at rest (kept out of the DB).
    pub secret_key: [u8; 32],
    /// Serializes the admission decision (capacity/quota check → reserve a
    /// `creating` row) so concurrent creates cannot both pass a stale capacity
    /// snapshot and overcommit a node (review #1). Held only across the short
    /// admission section, never across the VM boot.
    pub admission: tokio::sync::Mutex<()>,
}

pub type AppState = Arc<Inner>;

impl Inner {
    /// Build the public preview URL for an `<id>-<port>` host (spec §16.2).
    /// Includes the public port when it isn't the scheme default, so the URL is
    /// directly usable behind a non-standard port (e.g. on a LAN at :8080).
    pub fn preview_url(&self, sandbox_id: &str, port: u16) -> String {
        let https = self.cfg.server.public_https;
        let scheme = if https { "https" } else { "http" };
        // Preview hosts must be valid DNS labels so a per-host TLS cert can be
        // issued on demand: public ACME CAs (Let's Encrypt/ZeroSSL) reject
        // underscores. Render the id's `_` as `-` (e.g. `sbx_ab12` -> `sbx-ab12`);
        // the host-routed proxy maps it back to the canonical id in
        // `parse_preview_label`. The hex id body has no `-`/`_`, so this is
        // unambiguous and reversible.
        let host_id = sandbox_id.replace('_', "-");
        let host = format!("{host_id}-{port}.{}", self.cfg.server.public_domain);
        let default_port = if https { 443 } else { 80 };
        match self.cfg.server.public_port {
            Some(p) if p != default_port => format!("{scheme}://{host}:{p}"),
            _ => format!("{scheme}://{host}"),
        }
    }

    /// Resolve the data-plane client for a node: the in-process [`LocalNode`] if
    /// it's this node, else a [`RemoteNodeClient`] that drives the worker over
    /// its `/internal` API. This is how the control plane forwards runtime ops
    /// to whichever node the scheduler placed a sandbox on.
    pub fn node_for(&self, node_id: &str) -> Arc<dyn NodeClient> {
        if node_id == self.local_node_id {
            return self.local.clone();
        }
        match self.store.get_node(node_id) {
            Ok(Some(n)) if !n.advertise_addr.is_empty() => Arc::new(RemoteNodeClient::new(
                node_id,
                n.advertise_addr,
                self.cfg.node.rpc_token.clone(),
                self.http.clone(),
            )),
            // Unknown/addressless node: fall back to local (single-node default).
            _ => self.local.clone(),
        }
    }
}
