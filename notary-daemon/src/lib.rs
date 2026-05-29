//! Notary daemon library surface. The binary (`main.rs`) wires this up; tests
//! drive `router()` with a mock `Conductor`.

pub mod config;
pub mod conductor;
pub mod http;

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::conductor::{Conductor, HamConductor};
use crate::config::Config;
use crate::http::{router, AppState};

/// Connect to the local conductor and serve the HTTP API until shutdown.
pub async fn serve(cfg: Config) -> Result<()> {
    let mut shutdown = ham::install_shutdown_handler();

    let conductor = HamConductor::connect(&cfg, &mut shutdown)
        .await
        .context("shut down before the conductor became reachable")?;

    serve_with_conductor(cfg, Arc::new(conductor), shutdown).await
}

/// Serve with an injected `Conductor` (used by the binary after connecting, and
/// available to integration callers/tests that supply their own).
pub async fn serve_with_conductor(
    cfg: Config,
    conductor: Arc<dyn Conductor>,
    mut shutdown: ham::ShutdownRx,
) -> Result<()> {
    let state = AppState {
        conductor,
        bearer_token: Arc::new(cfg.bearer_token.clone()),
    };
    let app = router(state);

    let addr = format!("{}:{}", cfg.bind_addr, cfg.bind_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!(%addr, "notary daemon listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
            tracing::info!("shutdown signal received");
        })
        .await
        .context("http server error")
}
