//! The `Conductor` abstraction over the local Holochain conductor. The real impl
//! wraps `ham`; tests inject a mock so the HTTP↔zome mapping can be exercised
//! without a conductor.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_2::AttestCloseResponse;

use crate::config::Config;

#[async_trait]
pub trait Conductor: Send + Sync {
    async fn ping(&self) -> Result<()>;

    /// `transactor::notary_attest_close` on this notary's own cell: its
    /// signature over the agent's closed chain. It commits nothing.
    async fn notary_attest_close(&self, agent: AgentPubKey) -> Result<AttestCloseResponse>;

    /// `transactor::whoami`: proves the app cell answers, since a conductor can
    /// be reachable while its cell is wedged.
    async fn whoami(&self) -> Result<AgentPubKey>;
}

pub type HamConductor = Reconnecting<HamConnection>;

pub struct HamConnection {
    ham: ham::Ham,
    role_name: String,
}

pub fn ham_config(cfg: &Config) -> ham::HamConfig {
    cfg.signing.apply(
        ham::HamConfig::new(cfg.admin_port, cfg.app_port, cfg.app_id.clone())
            .with_request_timeout_secs(cfg.request_timeout_secs),
    )
}

impl HamConductor {
    /// Waits with backoff for the first connection; `None` means shutdown fired
    /// first.
    pub async fn connect(cfg: &Config, shutdown: &mut ham::ShutdownRx) -> Option<Self> {
        let ham_cfg = ham_config(cfg);
        let backoff = ham::BackoffConfig::default();
        let ham =
            ham::connect_with_backoff(|| ham::Ham::connect(ham_cfg.clone()), &backoff, shutdown)
                .await?;
        let role_name = cfg.role_name.clone();
        let first = HamConnection {
            ham,
            role_name: role_name.clone(),
        };
        Some(Reconnecting::new(
            first,
            Duration::from_secs(cfg.request_timeout_secs),
            move || {
                let (ham_cfg, role_name) = (ham_cfg.clone(), role_name.clone());
                async move {
                    Ok(HamConnection {
                        ham: ham::Ham::connect(ham_cfg).await?,
                        role_name,
                    })
                }
            },
        ))
    }
}

#[async_trait]
impl Conductor for HamConnection {
    async fn ping(&self) -> Result<()> {
        self.ham.ping().await
    }

    async fn notary_attest_close(&self, agent: AgentPubKey) -> Result<AttestCloseResponse> {
        self.ham
            .call_zome(&self.role_name, "transactor", "notary_attest_close", agent)
            .await
            .context("notary_attest_close zome call failed")
    }

    async fn whoami(&self) -> Result<AgentPubKey> {
        self.ham
            .call_zome(&self.role_name, "transactor", "whoami", ())
            .await
            .context("whoami zome call failed")
    }
}

type Connect<C> = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Result<C>> + Send>> + Send + Sync>;

/// A [`Conductor`] that replaces its connection once the conductor has closed
/// it. A conductor restart closes every connection open to it, and a closed
/// `ham` connection never reopens.
pub struct Reconnecting<C> {
    shared: Arc<Shared<C>>,
}

struct Shared<C> {
    /// Never held across an await, so a caller can read it while an attempt
    /// runs and, once it holds `connect`, see whether one ran since it looked.
    state: Mutex<State<C>>,
    /// Held for the whole of an attempt.
    connect: tokio::sync::Mutex<Connect<C>>,
    connect_timeout: Duration,
}

struct State<C> {
    link: Link<C>,
    attempts: u64,
    failures_in_a_row: u32,
}

enum Link<C> {
    Open(Arc<C>),
    ReconnectFailed(Arc<str>),
}

fn reconnect_failed(cause: &str) -> anyhow::Error {
    anyhow::anyhow!("{cause}").context("reconnecting to the conductor")
}

impl<C: Conductor + 'static> Reconnecting<C> {
    pub fn new<F, Fut>(first: C, connect_timeout: Duration, connect: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<C>> + Send + 'static,
    {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    link: Link::Open(Arc::new(first)),
                    attempts: 0,
                    failures_in_a_row: 0,
                }),
                connect: tokio::sync::Mutex::new(Box::new(move || Box::pin(connect()))),
                connect_timeout,
            }),
        }
    }

    async fn call<R, F, Fut>(&self, call: F) -> Result<R>
    where
        F: FnOnce(Arc<C>) -> Fut,
        Fut: Future<Output = Result<R>>,
    {
        let (link, attempts) = {
            let state = self.shared.state();
            let link = match &state.link {
                Link::Open(connection) => Some(connection.clone()),
                Link::ReconnectFailed(_) => None,
            };
            (link, state.attempts)
        };
        match link {
            Some(connection) => match call(connection).await {
                Err(lost) if ham::is_connection_error(&lost) => {
                    // The call may have run before its answer was lost, and a
                    // request is one zome call, so this request keeps its
                    // error and the next one gets the new connection.
                    let _ = self.reconnect(attempts, Some(format!("{lost:#}"))).await;
                    Err(lost)
                }
                answered => answered,
            },
            None => call(self.reconnect(attempts, None).await?).await,
        }
    }

    async fn reconnect(&self, attempts: u64, lost: Option<String>) -> Result<Arc<C>> {
        // In a task of its own, so a caller that gives up does not cancel the
        // attempt the callers queued behind it are waiting on.
        let shared = self.shared.clone();
        tokio::spawn(async move { shared.reconnect(attempts, lost).await })
            .await
            .context("reconnecting to the conductor")?
    }
}

impl<C: Conductor + 'static> Shared<C> {
    fn state(&self) -> MutexGuard<'_, State<C>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn link(&self) -> Result<Arc<C>> {
        match &self.state().link {
            Link::Open(connection) => Ok(connection.clone()),
            Link::ReconnectFailed(cause) => Err(reconnect_failed(cause)),
        }
    }

    async fn reconnect(&self, seen: u64, lost: Option<String>) -> Result<Arc<C>> {
        let connect = self.connect.lock().await;
        let attempted_meanwhile = self.state().attempts != seen;
        if attempted_meanwhile {
            return self.link();
        }
        if let Some(lost) = lost {
            tracing::warn!(error = %lost, "conductor connection lost; reconnecting");
        }
        let opened = tokio::time::timeout(self.connect_timeout, connect())
            .await
            .unwrap_or_else(|_| {
                Err(anyhow::anyhow!(
                    "connecting to the conductor and its lair keystore took longer than {:?}",
                    self.connect_timeout
                ))
            })
            .map(Arc::new)
            .map_err(|e| Arc::<str>::from(format!("{e:#}")));
        let failures = {
            let mut state = self.state();
            state.attempts += 1;
            match &opened {
                Ok(connection) => {
                    state.link = Link::Open(connection.clone());
                    std::mem::take(&mut state.failures_in_a_row)
                }
                Err(cause) => {
                    state.link = Link::ReconnectFailed(cause.clone());
                    state.failures_in_a_row = state.failures_in_a_row.saturating_add(1);
                    state.failures_in_a_row
                }
            }
        };
        match opened {
            Ok(connection) => {
                tracing::info!(failed_before = failures, "reconnected to the conductor");
                Ok(connection)
            }
            Err(cause) => {
                if failures >= ham::BackoffConfig::default().escalate_after {
                    tracing::error!(error = %cause, failures_in_a_row = failures, "reconnect failed");
                } else {
                    tracing::warn!(error = %cause, failures_in_a_row = failures, "reconnect failed");
                }
                Err(reconnect_failed(&cause))
            }
        }
    }
}

#[async_trait]
impl<C: Conductor + 'static> Conductor for Reconnecting<C> {
    async fn ping(&self) -> Result<()> {
        self.call(|c| async move { c.ping().await }).await
    }

    async fn notary_attest_close(&self, agent: AgentPubKey) -> Result<AttestCloseResponse> {
        self.call(|c| async move { c.notary_attest_close(agent).await })
            .await
    }

    async fn whoami(&self) -> Result<AgentPubKey> {
        self.call(|c| async move { c.whoami().await }).await
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
