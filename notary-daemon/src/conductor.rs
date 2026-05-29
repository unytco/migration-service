//! The `Conductor` abstraction over the local Holochain conductor. The real impl
//! wraps `ham`; tests inject a mock so the HTTP↔zome mapping can be exercised
//! without a conductor.

use anyhow::{Context, Result};
use async_trait::async_trait;
use rave_engine::types::entries::migration::v0_1::{NotaryReadRequest, NotaryReadResponse};

use crate::config::Config;

#[async_trait]
pub trait Conductor: Send + Sync {
    /// Lightweight liveness probe against the conductor.
    async fn ping(&self) -> Result<()>;

    /// Call the alliance `notary_read_predecessor_close` zome fn on the
    /// from-DNA cell (read + validate + sign in one call).
    async fn notary_read_predecessor_close(
        &self,
        req: NotaryReadRequest,
    ) -> Result<NotaryReadResponse>;
}

/// Real conductor connection via `ham`.
pub struct HamConductor {
    ham: ham::Ham,
    role_name: String,
}

impl HamConductor {
    /// Connect with exponential backoff until the conductor is reachable or
    /// shutdown fires (mirrors the unyt_cli daemon pattern).
    pub async fn connect(cfg: &Config, shutdown: &mut ham::ShutdownRx) -> Option<Self> {
        let ham_cfg = ham::HamConfig::new(cfg.admin_port, cfg.app_port, cfg.app_id.clone())
            .with_request_timeout_secs(cfg.request_timeout_secs);
        let backoff = ham::BackoffConfig::default();
        let ham = ham::connect_with_backoff(
            || ham::Ham::connect(ham_cfg.clone()),
            &backoff,
            shutdown,
        )
        .await?;
        Some(Self {
            ham,
            role_name: cfg.role_name.clone(),
        })
    }
}

#[async_trait]
impl Conductor for HamConductor {
    async fn ping(&self) -> Result<()> {
        self.ham.ping().await
    }

    async fn notary_read_predecessor_close(
        &self,
        req: NotaryReadRequest,
    ) -> Result<NotaryReadResponse> {
        self.ham
            .call_zome(&self.role_name, "transactor", "notary_read_predecessor_close", req)
            .await
            .context("notary_read_predecessor_close zome call failed")
    }
}
