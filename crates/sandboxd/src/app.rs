//! Process entrypoint: parse the CLI, load config, build the runtime, register
//! the local node, warm pools, and serve the API (spec §6.2 single-node mode).

use crate::config::Config;
use crate::node::LocalNode;
use crate::nodes::Node;
use crate::runtime::{firecracker::FirecrackerRuntime, mock::MockRuntime, Runtime};
use crate::state::Inner;
use crate::store::Store;
use crate::{auth, background, ids};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(name = "workdir", version, about = "Low-cost Firecracker microVM sandbox provider")]
struct Cli {
    /// Path to config.toml.
    #[arg(long, env = "WORKDIR_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the control plane + host agent (default).
    Serve,
    /// Print an example config to stdout.
    GenConfig,
    /// Print the resolved configuration and detected capabilities, then exit.
    Doctor,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::GenConfig) => {
            print!("{}", crate::config_example::EXAMPLE_CONFIG);
            return Ok(());
        }
        Some(Command::Doctor) => {
            let cfg = Config::load(cli.config.as_deref())?;
            return doctor(&cfg);
        }
        _ => {}
    }

    let cfg = Config::load(cli.config.as_deref())?;
    init_tracing();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(serve(cfg))
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,sandboxd=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn serve(cfg: Config) -> Result<()> {
    let state = build_state(cfg).await?;
    let bind = state.cfg.server.bind.clone();

    background::spawn_warmer(state.clone());
    background::spawn_idle_reaper(state.clone());
    background::spawn_credit_enforcer(state.clone());
    background::spawn_image_gc(state.clone());
    background::spawn_jail_gc(state.clone());
    background::spawn_heartbeat(state.clone());

    let app = crate::api::router(state);
    let listener = tokio::net::TcpListener::bind(&bind).await.with_context(|| format!("bind {bind}"))?;
    tracing::info!(%bind, "workdir listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    Ok(())
}

/// Build the fully-wired application state: store, bootstrap, runtime, local
/// node registration, and hot-pool configuration. Used by `serve` and by
/// integration tests (which drive the router directly).
pub async fn build_state(cfg: Config) -> Result<crate::state::AppState> {
    std::fs::create_dir_all(&cfg.server.data_dir).ok();
    std::fs::create_dir_all(&cfg.runtime.workspace_dir).ok();
    std::fs::create_dir_all(&cfg.runtime.images_dir).ok();
    std::fs::create_dir_all(&cfg.runtime.volumes_dir).ok();
    let store = Store::open(&cfg.db_path()).context("open store")?;

    // Bootstrap the admin org + key; print the key once if freshly generated.
    let provided = if cfg.auth.bootstrap_admin_key.is_empty() {
        None
    } else {
        Some(cfg.auth.bootstrap_admin_key.clone())
    };
    if let Some(key) = auth::bootstrap(&store, &cfg.auth.bootstrap_org, provided.as_deref())? {
        tracing::info!("bootstrap admin API key (store this; shown once): {key}");
        println!("\n==> admin API key (shown once): {key}\n");
    }

    // Build the data-plane runtime.
    //
    // SECURITY: the mock runtime executes user code on the host with NO
    // isolation. It must never be selected on a real deployment. Require an
    // explicit, loud opt-in so a stray config/env can't silently turn a prod
    // node into a host-RCE surface (review finding C1).
    let runtime: Arc<dyn Runtime> = match cfg.runtime.kind.as_str() {
        "firecracker" => Arc::new(FirecrackerRuntime::new(&cfg.runtime)),
        "mock" => {
            let allow = std::env::var("WORKDIR_ALLOW_INSECURE_RUNTIME").ok().as_deref() == Some("1");
            if !allow {
                anyhow::bail!(
                    "runtime.kind = 'mock' runs untrusted code on the HOST with no isolation and \
                     is for local development only. Refusing to start. Set \
                     WORKDIR_ALLOW_INSECURE_RUNTIME=1 to acknowledge this (dev only); use \
                     'firecracker' in production."
                );
            }
            tracing::warn!(
                "INSECURE mock runtime enabled — user code runs on the host with NO isolation. \
                 Development use only."
            );
            Arc::new(MockRuntime::new(cfg.runtime.workspace_dir.clone(), cfg.runtime.volumes_dir.clone()))
        }
        other => anyhow::bail!("unknown runtime kind '{other}' (use 'mock' or 'firecracker')"),
    };
    tracing::info!(runtime = runtime.kind(), "data-plane runtime selected");

    // Resolve and register this node.
    let node_id = resolve_node_id(&store, &cfg)?;
    let total_memory_gb = if cfg.node.total_memory_gb > 0.0 {
        cfg.node.total_memory_gb
    } else {
        detect_total_memory_gb()
    };
    let kvm_ok = runtime.kind() == "mock" || std::path::Path::new("/dev/kvm").exists();
    let advertise_addr = if cfg.node.advertise_addr.is_empty() {
        cfg.server.bind.clone()
    } else {
        cfg.node.advertise_addr.clone()
    };
    let node = Node {
        id: node_id.clone(),
        hostname: hostname(),
        role: cfg.node.role.clone(),
        total_memory_gb,
        advertise_addr,
        schedulable: true,
        draining: false,
        kvm_ok,
        registered_at: chrono::Utc::now(),
        last_heartbeat_at: chrono::Utc::now(),
    };
    store.put_node(&node)?;
    tracing::info!(node = %node_id, mem_gb = total_memory_gb, kvm_ok, "registered local node");

    let local = Arc::new(LocalNode::new(node_id.clone(), runtime));
    local.configure_default_pools(cfg.hotpool.base_target).await;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default();

    // Sweep sandboxes interrupted by a previous restart before serving.
    match store.reconcile_interrupted(chrono::Utc::now()) {
        Ok(0) => {}
        Ok(n) => tracing::warn!(count = n, "reconciled interrupted sandboxes from a prior run"),
        Err(e) => tracing::error!(error = %e, "startup reconciliation failed"),
    }

    let secret_key = crate::secrets::load_or_create_key(&cfg.server.data_dir)
        .context("load/generate secret encryption key")?;

    let state: crate::state::AppState = Arc::new(Inner {
        cfg,
        store,
        local,
        local_node_id: node_id,
        http,
        secret_key,
        admission: tokio::sync::Mutex::new(()),
    });
    Ok(state)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}

fn resolve_node_id(store: &Store, cfg: &Config) -> Result<String> {
    if !cfg.node.node_id.is_empty() {
        return Ok(cfg.node.node_id.clone());
    }
    if let Some(existing) = store.get_meta("node_id")? {
        return Ok(existing);
    }
    let id = ids::node_id();
    store.set_meta("node_id", &id)?;
    Ok(id)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

/// Best-effort total RAM detection. Falls back to a 64 GB assumption (the
/// reference EX44 node class) when it cannot read system memory.
fn detect_total_memory_gb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    if let Some(kb) = rest.trim().split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                        return kb / 1024.0 / 1024.0;
                    }
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output() {
            if let Ok(bytes) = String::from_utf8_lossy(&out.stdout).trim().parse::<f64>() {
                return bytes / 1024.0 / 1024.0 / 1024.0;
            }
        }
    }
    64.0
}

fn doctor(cfg: &Config) -> Result<()> {
    println!("workdir doctor");
    println!("  runtime.kind        = {}", cfg.runtime.kind);
    println!("  /dev/kvm present    = {}", std::path::Path::new("/dev/kvm").exists());
    println!("  detected memory GB  = {:.1}", detect_total_memory_gb());
    println!("  data_dir            = {}", cfg.server.data_dir.display());
    println!("  bind                = {}", cfg.server.bind);
    println!("  public_domain       = {}", cfg.server.public_domain);
    println!("  base unit price/hr  = {}", cfg.pricing.default_unit_price_usd_hr);
    if cfg.runtime.kind == "firecracker" && !std::path::Path::new("/dev/kvm").exists() {
        println!("\nWARNING: runtime=firecracker but /dev/kvm is missing; boots will fail.");
    }
    Ok(())
}
