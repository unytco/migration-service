//! The `Conductor` abstraction over the local Holochain conductor. The real impl
//! wraps `ham`; tests inject a mock so the HTTP↔zome mapping can be exercised
//! without a conductor.

use anyhow::{Context, Result};
use async_trait::async_trait;
use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_1::ReadCloseResponse;

use crate::config::Config;

#[async_trait]
pub trait Conductor: Send + Sync {
    /// Lightweight liveness probe against the conductor.
    async fn ping(&self) -> Result<()>;

    /// `transactor::read_predecessor_close` on the from-DNA cell — fetch the
    /// agent's committed close (payload + notary signatures + close action).
    /// A pure read; this daemon has NO signing capability of any kind.
    async fn read_predecessor_close(&self, agent: AgentPubKey) -> Result<ReadCloseResponse>;

    /// Trivial read-only zome call proving the app cell answers
    /// (`transactor::whoami`) — the second half of the health check, distinct
    /// from `ping` (a conductor can be reachable while the cell is wedged).
    async fn whoami(&self) -> Result<AgentPubKey>;
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
        let ham =
            ham::connect_with_backoff(|| ham::Ham::connect(ham_cfg.clone()), &backoff, shutdown)
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

    async fn read_predecessor_close(&self, agent: AgentPubKey) -> Result<ReadCloseResponse> {
        self.ham
            .call_zome(
                &self.role_name,
                "transactor",
                "read_predecessor_close",
                agent,
            )
            .await
            .context("read_predecessor_close zome call failed")
    }

    async fn whoami(&self) -> Result<AgentPubKey> {
        self.ham
            .call_zome(&self.role_name, "transactor", "whoami", ())
            .await
            .context("whoami zome call failed")
    }
}
