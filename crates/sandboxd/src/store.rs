//! SQLite persistence (spec §6.2 "SQLite or Postgres"). Rich domain structs are
//! stored as JSON in a `data` column alongside a few indexed columns used for
//! filtering. All methods are synchronous and short-lived; the control plane is
//! single-node, so a single connection behind a mutex is sufficient and keeps
//! the storage layer dependency-light.

use crate::images::{CustomImage, ImageStatus};
use crate::lifecycle::State;
use crate::model::{ExecJob, ExecJobState, Sandbox};
use crate::nodes::Node;
use crate::usage::{ApiKey, Org, UsageInterval};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

const MIGRATIONS: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS orgs (
    id   TEXT PRIMARY KEY,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys (
    key_hash TEXT PRIMARY KEY,
    org_id   TEXT NOT NULL,
    data     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS nodes (
    id   TEXT PRIMARY KEY,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sandboxes (
    id         TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL,
    state      TEXT NOT NULL,
    node_id    TEXT,
    created_at TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sandboxes_org   ON sandboxes(org_id);
CREATE INDEX IF NOT EXISTS idx_sandboxes_node  ON sandboxes(node_id);
CREATE INDEX IF NOT EXISTS idx_sandboxes_state ON sandboxes(state);
CREATE TABLE IF NOT EXISTS exec_jobs (
    id         TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL,
    org_id     TEXT NOT NULL,
    state      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_exec_jobs_sandbox ON exec_jobs(sandbox_id);
CREATE INDEX IF NOT EXISTS idx_exec_jobs_org ON exec_jobs(org_id);
CREATE INDEX IF NOT EXISTS idx_exec_jobs_state ON exec_jobs(state);
CREATE TABLE IF NOT EXISTS images (
    id      TEXT PRIMARY KEY,
    org_id  TEXT NOT NULL,
    name    TEXT NOT NULL,
    version TEXT NOT NULL,
    status  TEXT NOT NULL,
    data    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_images_org ON images(org_id);
CREATE TABLE IF NOT EXISTS usage (
    id         TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL,
    org_id     TEXT NOT NULL,
    open       INTEGER NOT NULL,
    data       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_org ON usage(org_id);
CREATE INDEX IF NOT EXISTS idx_usage_sandbox ON usage(sandbox_id);
CREATE TABLE IF NOT EXISTS snapshots (
    id         TEXT PRIMARY KEY,
    sandbox_id TEXT NOT NULL,
    org_id     TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS secrets (
    org_id TEXT NOT NULL,
    name   TEXT NOT NULL,
    data   TEXT NOT NULL,
    PRIMARY KEY (org_id, name)
);
CREATE TABLE IF NOT EXISTS benchmarks (
    id         TEXT PRIMARY KEY,
    image      TEXT NOT NULL,
    boot_path  TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_benchmarks_path ON benchmarks(image, boot_path);
CREATE TABLE IF NOT EXISTS volumes (
    id         TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL,
    name       TEXT NOT NULL,
    data       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_volumes_org ON volumes(org_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_volumes_org_name ON volumes(org_id, name);
"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub sandbox_id: String,
    pub org_id: String,
    pub image: String,
    pub created_at: DateTime<Utc>,
    /// Approx stored size, billed separately (spec §17).
    pub storage_bytes: u64,
    /// Opaque runtime handle to the stored disk/memory artifact.
    pub handle: String,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open db {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(MIGRATIONS)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("store mutex poisoned")
    }

    // --- meta -----------------------------------------------------------

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // --- orgs -----------------------------------------------------------

    pub fn put_org(&self, org: &Org) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO orgs(id, data) VALUES(?1, ?2)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![org.id, serde_json::to_string(org)?],
        )?;
        Ok(())
    }

    pub fn get_org(&self, id: &str) -> Result<Option<Org>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row("SELECT data FROM orgs WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    // --- api keys -------------------------------------------------------

    pub fn put_api_key(&self, key: &ApiKey) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO api_keys(key_hash, org_id, data) VALUES(?1, ?2, ?3)
             ON CONFLICT(key_hash) DO UPDATE SET data = excluded.data",
            params![key.key_hash, key.org_id, serde_json::to_string(key)?],
        )?;
        Ok(())
    }

    pub fn get_api_key(&self, key_hash: &str) -> Result<Option<ApiKey>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM api_keys WHERE key_hash = ?1",
                params![key_hash],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    // --- nodes ----------------------------------------------------------

    pub fn put_node(&self, node: &Node) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO nodes(id, data) VALUES(?1, ?2)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![node.id, serde_json::to_string(node)?],
        )?;
        Ok(())
    }

    pub fn get_node(&self, id: &str) -> Result<Option<Node>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row("SELECT data FROM nodes WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    pub fn list_nodes(&self) -> Result<Vec<Node>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM nodes ORDER BY id")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    // --- sandboxes ------------------------------------------------------

    pub fn put_sandbox(&self, sb: &Sandbox) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO sandboxes(id, org_id, state, node_id, created_at, data)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                org_id=excluded.org_id, state=excluded.state,
                node_id=excluded.node_id, data=excluded.data",
            params![
                sb.id,
                sb.org_id,
                sb.state.as_str(),
                sb.node_id,
                sb.created_at.to_rfc3339(),
                serde_json::to_string(sb)?
            ],
        )?;
        Ok(())
    }

    pub fn get_sandbox(&self, id: &str) -> Result<Option<Sandbox>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM sandboxes WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    /// Atomic compare-and-set transition. Under the single store lock, read the
    /// current state; if it is in `allowed`, apply `mutate` (which sets the new
    /// state and any fields) and persist. Otherwise return `Conflict`. This is
    /// how every lifecycle change must be applied so concurrent or stale-copy
    /// writers cannot resurrect or double-process a sandbox (review #2, #3).
    pub fn cas_sandbox<F>(&self, id: &str, allowed: &[State], mutate: F) -> Result<CasOutcome>
    where
        F: FnOnce(&mut Sandbox),
    {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM sandboxes WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        let mut sb: Sandbox = match row {
            Some(d) => serde_json::from_str(&d)?,
            None => return Ok(CasOutcome::NotFound),
        };
        if !allowed.contains(&sb.state) {
            return Ok(CasOutcome::Conflict(sb.state));
        }
        mutate(&mut sb);
        conn.execute(
            "UPDATE sandboxes SET org_id=?2, state=?3, node_id=?4, data=?5 WHERE id=?1",
            params![
                sb.id,
                sb.org_id,
                sb.state.as_str(),
                sb.node_id,
                serde_json::to_string(&sb)?
            ],
        )?;
        Ok(CasOutcome::Updated(sb))
    }

    /// Cheap activity touch: update only `last_active_at` via json_set, avoiding
    /// a read-modify-write that could clobber a concurrent state change
    /// (review #6, #7).
    pub fn touch_last_active(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "UPDATE sandboxes SET data = json_set(data, '$.last_active_at', ?2) WHERE id = ?1",
            params![id, now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// On control-plane restart, in-memory runtime state is gone, so sandboxes
    /// left mid-flight (`creating`/`running`/`resuming`/`stopping`) no longer
    /// have a backing VM. Mark them failed and close their open billing
    /// intervals so cost stops and capacity/quota are released (review #5).
    pub fn reconcile_interrupted(&self, now: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT id, data FROM sandboxes
             WHERE state IN ('creating','running','resuming','stopping')",
        )?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let count = rows.len();
        for (id, data) in rows {
            let mut sb: Sandbox = serde_json::from_str(&data)?;
            sb.state = State::Failed;
            sb.error = Some("interrupted by control-plane restart".to_string());
            sb.updated_at = now;
            conn.execute(
                "UPDATE sandboxes SET state='failed', data=?2 WHERE id=?1",
                params![id, serde_json::to_string(&sb)?],
            )?;
            close_open_usage_locked(&conn, &id, now)?;
        }
        Ok(count)
    }

    pub fn list_sandboxes_for_org(&self, org_id: &str) -> Result<Vec<Sandbox>> {
        let conn = self.lock();
        let mut stmt =
            conn.prepare("SELECT data FROM sandboxes WHERE org_id = ?1 ORDER BY created_at DESC")?;
        let rows = stmt.query_map(params![org_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    /// Active sandboxes on a node (for admission and capacity).
    pub fn active_sandboxes_on_node(&self, node_id: &str) -> Result<Vec<Sandbox>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM sandboxes
             WHERE node_id = ?1 AND state IN ('creating','running','resuming')",
        )?;
        let rows = stmt.query_map(params![node_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn all_active_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT data FROM sandboxes WHERE state IN ('creating','running','resuming')",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    // --- exec jobs --------------------------------------------------------

    pub fn put_exec_job(&self, job: &ExecJob) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO exec_jobs(id, sandbox_id, org_id, state, created_at, data)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                sandbox_id=excluded.sandbox_id, org_id=excluded.org_id,
                state=excluded.state, data=excluded.data",
            params![
                job.id,
                job.sandbox_id,
                job.org_id,
                job.state.as_str(),
                job.started_at.to_rfc3339(),
                serde_json::to_string(job)?,
            ],
        )?;
        Ok(())
    }

    pub fn get_exec_job(&self, id: &str) -> Result<Option<ExecJob>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM exec_jobs WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    pub fn has_running_exec_jobs(&self, sandbox_id: &str) -> Result<bool> {
        let conn = self.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM exec_jobs WHERE sandbox_id = ?1 AND state = 'running'",
            params![sandbox_id],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn reconcile_interrupted_exec_jobs(&self, now: DateTime<Utc>) -> Result<usize> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT id, data FROM exec_jobs WHERE state = 'running'")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        let count = rows.len();
        for (id, data) in rows {
            let mut job: ExecJob = serde_json::from_str(&data)?;
            job.state = ExecJobState::Failed;
            job.error = Some("interrupted by control-plane restart".to_string());
            job.finished_at = Some(now);
            conn.execute(
                "UPDATE exec_jobs SET state = 'failed', data = ?2 WHERE id = ?1",
                params![id, serde_json::to_string(&job)?],
            )?;
        }
        Ok(count)
    }

    // --- images ---------------------------------------------------------

    pub fn put_image(&self, img: &CustomImage) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO images(id, org_id, name, version, status, data)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET status=excluded.status, data=excluded.data",
            params![
                img.id,
                img.org_id,
                img.name,
                img.version,
                img.status.as_str(),
                serde_json::to_string(img)?
            ],
        )?;
        Ok(())
    }

    pub fn get_image(&self, id: &str) -> Result<Option<CustomImage>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row("SELECT data FROM images WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    /// Find a ready image by `custom/<org>/<name>:<version>` reference.
    pub fn find_ready_image(&self, reference: &str) -> Result<Option<CustomImage>> {
        let (name, version) = match reference.rsplit_once(':') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (reference.to_string(), None),
        };
        let conn = self.lock();
        let sql = match &version {
            Some(_) => {
                "SELECT data FROM images WHERE name = ?1 AND version = ?2 AND status = 'ready'"
            }
            None => {
                "SELECT data FROM images WHERE name = ?1 AND status = 'ready' ORDER BY version DESC"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let row: Option<String> = match &version {
            Some(v) => stmt.query_row(params![name, v], |r| r.get(0)).optional()?,
            None => stmt.query_row(params![name], |r| r.get(0)).optional()?,
        };
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    /// All non-deleted images across orgs (for the ephemeral-image GC sweep).
    pub fn all_images(&self) -> Result<Vec<CustomImage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM images WHERE status != 'deleted'")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn list_images_for_org(&self, org_id: &str) -> Result<Vec<CustomImage>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM images WHERE org_id = ?1")?;
        let rows = stmt.query_map(params![org_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            let img: CustomImage = serde_json::from_str(&r?)?;
            if img.status != ImageStatus::Deleted {
                out.push(img);
            }
        }
        Ok(out)
    }

    // --- usage ----------------------------------------------------------

    /// Open a billing interval only if the sandbox has no open interval already,
    /// so a double create/resume cannot double-bill (review #2). Returns whether
    /// a new interval was opened.
    pub fn open_usage_if_none(&self, iv: &UsageInterval) -> Result<bool> {
        let conn = self.lock();
        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM usage WHERE sandbox_id = ?1 AND open = 1",
            params![iv.sandbox_id],
            |r| r.get(0),
        )?;
        if existing > 0 {
            return Ok(false);
        }
        conn.execute(
            "INSERT INTO usage(id, sandbox_id, org_id, open, data) VALUES(?1, ?2, ?3, 1, ?4)",
            params![iv.id, iv.sandbox_id, iv.org_id, serde_json::to_string(iv)?],
        )?;
        Ok(true)
    }

    /// Close all open intervals for a sandbox at `ended_at`.
    pub fn close_open_usage(&self, sandbox_id: &str, ended_at: DateTime<Utc>) -> Result<()> {
        let conn = self.lock();
        close_open_usage_locked(&conn, sandbox_id, ended_at)
    }

    pub fn usage_for_org(&self, org_id: &str) -> Result<Vec<UsageInterval>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM usage WHERE org_id = ?1")?;
        let rows = stmt.query_map(params![org_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn all_usage(&self) -> Result<Vec<UsageInterval>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM usage")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    // --- snapshots ------------------------------------------------------

    pub fn put_snapshot(&self, snap: &Snapshot) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO snapshots(id, sandbox_id, org_id, data) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data",
            params![
                snap.id,
                snap.sandbox_id,
                snap.org_id,
                serde_json::to_string(snap)?
            ],
        )?;
        Ok(())
    }

    pub fn list_snapshots_for_sandbox(&self, sandbox_id: &str) -> Result<Vec<Snapshot>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM snapshots WHERE sandbox_id = ?1")?;
        let rows = stmt.query_map(params![sandbox_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    // --- secrets (encrypted at rest) ------------------------------------

    pub fn put_secret(&self, rec: &crate::secrets::SecretRecord) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO secrets(org_id, name, data) VALUES(?1, ?2, ?3)
             ON CONFLICT(org_id, name) DO UPDATE SET data = excluded.data",
            params![rec.org_id, rec.name, serde_json::to_string(rec)?],
        )?;
        Ok(())
    }

    pub fn get_secret(
        &self,
        org_id: &str,
        name: &str,
    ) -> Result<Option<crate::secrets::SecretRecord>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM secrets WHERE org_id = ?1 AND name = ?2",
                params![org_id, name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    /// List secret metadata (names + timestamps) — never values.
    pub fn list_secrets(&self, org_id: &str) -> Result<Vec<crate::secrets::SecretRecord>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM secrets WHERE org_id = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![org_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn delete_secret(&self, org_id: &str, name: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute(
            "DELETE FROM secrets WHERE org_id = ?1 AND name = ?2",
            params![org_id, name],
        )?;
        Ok(n > 0)
    }

    // --- persistent volumes (Phase 5) -----------------------------------

    pub fn put_volume(&self, v: &crate::model::Volume) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO volumes(id, org_id, name, data) VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET data = excluded.data, name = excluded.name",
            params![v.id, v.org_id, v.name, serde_json::to_string(v)?],
        )?;
        Ok(())
    }

    pub fn get_volume(&self, id: &str) -> Result<Option<crate::model::Volume>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row("SELECT data FROM volumes WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    pub fn get_volume_by_name(
        &self,
        org_id: &str,
        name: &str,
    ) -> Result<Option<crate::model::Volume>> {
        let conn = self.lock();
        let row: Option<String> = conn
            .query_row(
                "SELECT data FROM volumes WHERE org_id = ?1 AND name = ?2",
                params![org_id, name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.map(|d| serde_json::from_str(&d)).transpose()?)
    }

    pub fn list_volumes_for_org(&self, org_id: &str) -> Result<Vec<crate::model::Volume>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM volumes WHERE org_id = ?1 ORDER BY name")?;
        let rows = stmt.query_map(params![org_id], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }

    pub fn delete_volume(&self, id: &str) -> Result<bool> {
        let conn = self.lock();
        let n = conn.execute("DELETE FROM volumes WHERE id = ?1", params![id])?;
        Ok(n > 0)
    }

    // --- benchmark samples (roadmap Phase 0) ----------------------------

    pub fn put_benchmark_sample(&self, s: &crate::bench::BenchmarkSample) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO benchmarks(id, image, boot_path, data) VALUES(?1, ?2, ?3, ?4)",
            params![
                s.id,
                s.image,
                s.boot_path.as_str(),
                serde_json::to_string(s)?
            ],
        )?;
        Ok(())
    }

    pub fn all_benchmark_samples(&self) -> Result<Vec<crate::bench::BenchmarkSample>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT data FROM benchmarks")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = vec![];
        for r in rows {
            out.push(serde_json::from_str(&r?)?);
        }
        Ok(out)
    }
}

/// Result of a compare-and-set state transition. Transient return value (never
/// stored in bulk), so the size difference between variants is irrelevant.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CasOutcome {
    Updated(Sandbox),
    /// The sandbox was not in any of the allowed states; carries the actual one.
    Conflict(State),
    NotFound,
}

/// Close all open usage intervals for a sandbox, using an already-held lock.
fn close_open_usage_locked(
    conn: &Connection,
    sandbox_id: &str,
    ended_at: DateTime<Utc>,
) -> Result<()> {
    let mut stmt = conn.prepare("SELECT id, data FROM usage WHERE sandbox_id = ?1 AND open = 1")?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![sandbox_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for (id, data) in rows {
        let mut iv: UsageInterval = serde_json::from_str(&data)?;
        iv.ended_at = Some(ended_at);
        conn.execute(
            "UPDATE usage SET open = 0, data = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(&iv)?],
        )?;
    }
    Ok(())
}

/// Count of active sandboxes by state, used in capacity reporting.
pub fn count_active(sandboxes: &[Sandbox]) -> usize {
    sandboxes.iter().filter(|s| s.state.is_active()).count()
}

/// Sum of memory (GB) consumed by active sandboxes on a node.
pub fn active_memory_gb(sandboxes: &[Sandbox]) -> f64 {
    sandboxes
        .iter()
        .filter(|s| s.state.is_active())
        .map(|s| s.resources.memory_gb())
        .sum()
}

/// Helper: does this sandbox count as active?
pub fn is_state_active(state: State) -> bool {
    state.is_active()
}

#[cfg(test)]
mod volume_tests {
    use super::*;
    use crate::model::Volume;
    use chrono::Utc;

    fn vol(id: &str, org: &str, name: &str) -> Volume {
        let now = Utc::now();
        Volume {
            id: id.into(),
            org_id: org.into(),
            name: name.into(),
            size_gb: 10,
            attached_to: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn volume_crud_and_org_scope() {
        let s = Store::open_in_memory().unwrap();
        s.put_volume(&vol("vol_a", "org1", "data")).unwrap();
        s.put_volume(&vol("vol_b", "org1", "cache")).unwrap();
        s.put_volume(&vol("vol_c", "org2", "data")).unwrap();

        // org-scoped list + by-name lookup
        assert_eq!(s.list_volumes_for_org("org1").unwrap().len(), 2);
        assert_eq!(s.list_volumes_for_org("org2").unwrap().len(), 1);
        assert_eq!(
            s.get_volume_by_name("org1", "cache").unwrap().unwrap().id,
            "vol_b"
        );
        assert!(s.get_volume_by_name("org2", "cache").unwrap().is_none());

        // attach reservation round-trips through the JSON blob
        let mut v = s.get_volume("vol_a").unwrap().unwrap();
        v.attached_to = Some("sbx_1".into());
        s.put_volume(&v).unwrap();
        assert_eq!(
            s.get_volume("vol_a")
                .unwrap()
                .unwrap()
                .attached_to
                .as_deref(),
            Some("sbx_1")
        );

        // delete frees the slot
        assert!(s.delete_volume("vol_a").unwrap());
        assert!(!s.delete_volume("vol_a").unwrap());
        assert_eq!(s.list_volumes_for_org("org1").unwrap().len(), 1);
    }

    #[test]
    fn volume_label_fits_ext4() {
        let l = crate::ids::volume_label("vol_a1b2c3d4e5f6");
        assert!(l.starts_with("wdv"));
        assert!(
            l.len() <= 16,
            "ext4 labels are capped at 16 bytes, got {}",
            l.len()
        );
    }
}
