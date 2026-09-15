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

/// The `HamConfig` every `ham` connection this daemon makes is built from: the
/// conductor coordinates plus the signer [`Config`] resolved at startup. One
/// home, so the daemon and the tests that assert which signer is configured are
/// looking at the same thing.
pub fn ham_config(cfg: &Config) -> ham::HamConfig {
    cfg.signing.apply(
        ham::HamConfig::new(cfg.admin_port, cfg.app_port, cfg.app_id.clone())
            .with_request_timeout_secs(cfg.request_timeout_secs),
    )
}

impl HamConductor {
    /// Connect with exponential backoff until the conductor is reachable or
    /// shutdown fires (mirrors the unyt_cli daemon pattern).
    pub async fn connect(cfg: &Config, shutdown: &mut ham::ShutdownRx) -> Option<Self> {
        let ham_cfg = ham_config(cfg);
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::signing;

    const LAIR_URL: &str = "unix:///var/lib/holochain/lair/socket?k=abc123";

    /// A `Config` shaped like a deployed notary, carrying the signer under
    /// test. Built directly rather than through `from_env`, which reads the
    /// process-global environment every other test shares.
    fn config_with(signing: ham::SigningPolicy) -> Config {
        Config {
            admin_port: 8800,
            app_port: 30000,
            app_id: "unyt".into(),
            role_name: "alliance".into(),
            bind_addr: "127.0.0.1".into(),
            bind_port: 8790,
            bearer_token: "token".into(),
            request_timeout_secs: 30,
            signing,
        }
    }

    #[test]
    fn the_connection_is_built_with_the_lair_signer_the_config_resolved() {
        let signing = signing::resolve(Some(LAIR_URL.into()), Some("pass".into()), None).unwrap();
        let ham_cfg = ham_config(&config_with(signing));
        assert!(
            ham_cfg.lair.is_some(),
            "every ham connection this daemon makes must sign through lair"
        );
        assert_eq!(ham_cfg.admin_port, 8800);
        assert_eq!(ham_cfg.request_timeout_secs, 30);
    }

    #[test]
    fn the_opt_in_builds_the_connection_on_the_cap_grant_path() {
        let signing = signing::resolve(None, None, Some("1".into())).unwrap();
        let cfg = ham_config(&config_with(signing));
        assert!(
            cfg.lair.is_none(),
            "the escape hatch must actually reach ham as client signing"
        );
        assert!(
            cfg.allow_cap_grant_signing,
            "and ham refuses that path unless the config asks for it by name"
        );
    }
}
