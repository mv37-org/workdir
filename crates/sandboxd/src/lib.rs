//! sandboxd — low-cost Firecracker microVM sandbox provider.
//!
//! This crate implements the single-binary, all-in-one node from spec §6.2:
//! control plane API + scheduler + host agent + runtime, plus the multi-node
//! primitives (node registry, join token, drain) from §6.3.

pub mod api;
pub mod app;
pub mod auth;
pub mod background;
pub mod capacity;
pub mod catalog;
pub mod config;
pub mod config_example;
pub mod error;
pub mod hotpool;
pub mod ids;
pub mod images;
pub mod knobs;
pub mod lifecycle;
pub mod model;
pub mod node;
pub mod nodes;
pub mod pricing;
pub mod runtime;
pub mod scheduler;
pub mod secrets;
pub mod service;
pub mod state;
pub mod store;
pub mod usage;
pub mod views;

/// Process entrypoint (CLI parsing + serve), implemented in [`app`].
pub fn run() -> anyhow::Result<()> {
    app::run()
}
