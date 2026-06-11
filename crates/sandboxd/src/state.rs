//! Shared application state handed to every request handler.

use crate::config::Config;
use crate::node::LocalNode;
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
        let host = format!("{sandbox_id}-{port}.{}", self.cfg.server.public_domain);
        let default_port = if https { 443 } else { 80 };
        match self.cfg.server.public_port {
            Some(p) if p != default_port => format!("{scheme}://{host}:{p}"),
            _ => format!("{scheme}://{host}"),
        }
    }
}
