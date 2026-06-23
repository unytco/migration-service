//! Agent configuration, read from the environment (mirrors notary-daemon's
//! `Config::from_env`). The `automation/` installer renders these into the
//! systemd `EnvironmentFile`; every field has a sensible default except the
//! ones that have no safe default (`MIGRATION_AGENT_STATE_FILE`, and — for the
//! open service — `MIGRATION_AGENT_HAPP_PATH` / `MIGRATION_AGENT_JOINING_URL`,
//! validated by the open command itself, not here, so close/status need no
//! open-only vars).

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use holo_hash::DnaHashB64;

use crate::policy::PolicyOpts;

/// How the supervised loops connect to the local conductor and where they
/// record progress, plus the M-of-N collection policy knobs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Holochain conductor admin websocket port (local).
    pub admin_port: u16,
    /// Holochain conductor app websocket port (local).
    pub app_port: u16,
    /// Installed app id holding the alliance cell this agent migrates.
    pub app_id: String,
    /// DNA role name within the app (default `alliance`).
    pub role_name: String,
    /// `ham` per-request timeout (seconds) — the per-zome-call budget.
    pub request_timeout_secs: u64,
    /// Machine-readable progress file the `automation/` report collector reads
    /// (`make migrate-status`). Required: the report contract depends on it.
    pub state_file: PathBuf,
    /// Backoff for the supervised loops' transient-failure retries (distinct
    /// from `ham`'s connect backoff, which is internal to the connection).
    pub retry_initial: Duration,
    pub retry_max: Duration,
    /// The signature-collection policy (open question knobs all live here).
    pub policy: PolicyOpts,
    /// The successor DNA a close binds to (`prepare_closing_summary(to_dna)`).
    /// Read from `MIGRATION_AGENT_TO_DNA`; `None` for the open / verify / status
    /// commands (which take from/to as CLI args) — the close command requires it.
    pub to_dna: Option<DnaHashB64>,
}

/// Open-service-only configuration, validated when the open command runs (so
/// the close / status / verify commands need none of it set).
#[derive(Debug, Clone)]
pub struct OpenConfig {
    /// Path to the target release's happ bundle on the new droplet — installed
    /// for the carried key (the open service performs `install_app` itself).
    pub happ_path: PathBuf,
    /// The target release's joining-service base URL — where a FRESH membrane
    /// proof is requested for the carried key (the old proof is never reused).
    pub joining_url: String,
    /// Network seed for the new DNA's app install. The joining service may also
    /// return one in `dna_modifiers`; that takes precedence when present.
    pub network_seed: Option<String>,
}

fn var(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn parse_var<T>(key: &str, default: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let raw = var(key).unwrap_or_else(|| default.to_string());
    raw.parse()
        .map_err(|e| anyhow::anyhow!("{key}: {e}"))
        .with_context(|| format!("parsing {key}"))
}

/// Reject degenerate backoff bounds: a zero `retry_initial` would spin a tight
/// retry loop (no delay), and an inverted pair (initial > max) makes the curve
/// ill-defined. Fail fast in a supervised service rather than discovering it at
/// runtime. Pure (no env access) so it is unit-tested without touching process
/// state.
fn validate_backoff_bounds(retry_initial_ms: u64, retry_max_ms: u64) -> Result<()> {
    if retry_initial_ms == 0 || retry_max_ms == 0 {
        anyhow::bail!(
            "MIGRATION_AGENT_RETRY_INITIAL_MS and MIGRATION_AGENT_RETRY_MAX_MS must be > 0 \
             (got {retry_initial_ms} / {retry_max_ms})"
        );
    }
    if retry_initial_ms > retry_max_ms {
        anyhow::bail!(
            "MIGRATION_AGENT_RETRY_INITIAL_MS ({retry_initial_ms}) must be \
             <= MIGRATION_AGENT_RETRY_MAX_MS ({retry_max_ms})"
        );
    }
    Ok(())
}

impl Config {
    /// The supervised loops' transient-retry backoff as a `ham::BackoffConfig`,
    /// so they delay via `ham::compute_delay_ms` — gaining its ~10% jitter,
    /// which keeps many agents retrying after the same gossip blip from
    /// thundering in lockstep. One source of truth for the backoff curve (the
    /// close + open loops and the policy's same-notary retry all use ham's), not
    /// a hand-rolled `delay * 2` copy that drops the jitter.
    pub fn loop_backoff(&self) -> ham::BackoffConfig {
        ham::BackoffConfig {
            initial_ms: self.retry_initial.as_millis().min(u64::MAX as u128) as u64,
            max_ms: self.retry_max.as_millis().min(u64::MAX as u128) as u64,
            escalate_after: ham::BackoffConfig::default().escalate_after,
        }
    }

    pub fn from_env() -> Result<Self> {
        let state_file = var("MIGRATION_AGENT_STATE_FILE")
            .context("MIGRATION_AGENT_STATE_FILE is required (the report collector reads it)")?
            .into();
        let retry_initial_ms: u64 = parse_var("MIGRATION_AGENT_RETRY_INITIAL_MS", "1000")?;
        let retry_max_ms: u64 = parse_var("MIGRATION_AGENT_RETRY_MAX_MS", "30000")?;
        validate_backoff_bounds(retry_initial_ms, retry_max_ms)?;
        Ok(Self {
            admin_port: parse_var("HOLOCHAIN_ADMIN_PORT", "8800")?,
            app_port: parse_var("HOLOCHAIN_APP_PORT", "30000")?,
            app_id: var("HOLOCHAIN_APP_ID").unwrap_or_else(|| "unyt".into()),
            role_name: var("HOLOCHAIN_ROLE_NAME").unwrap_or_else(|| "alliance".into()),
            request_timeout_secs: parse_var("HAM_REQUEST_TIMEOUT_SECS", "60")?,
            state_file,
            retry_initial: Duration::from_millis(retry_initial_ms),
            retry_max: Duration::from_millis(retry_max_ms),
            policy: PolicyOpts::from_env()?,
            // Optional here: only the close command requires it (validated there),
            // so open / verify / status — which also run `from_env` — don't fail
            // without it.
            to_dna: match var("MIGRATION_AGENT_TO_DNA") {
                Some(s) => Some(
                    s.parse()
                        .map_err(|e| anyhow::anyhow!("MIGRATION_AGENT_TO_DNA: {e}"))?,
                ),
                None => None,
            },
        })
    }
}

impl OpenConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            happ_path: var("MIGRATION_AGENT_HAPP_PATH")
                .context("MIGRATION_AGENT_HAPP_PATH is required for the open service")?
                .into(),
            joining_url: var("MIGRATION_AGENT_JOINING_URL")
                .context("MIGRATION_AGENT_JOINING_URL is required for the open service")?,
            network_seed: var("MIGRATION_AGENT_NETWORK_SEED"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::validate_backoff_bounds;

    #[test]
    fn valid_backoff_bounds_pass() {
        assert!(validate_backoff_bounds(1000, 30000).is_ok());
        // Equal bounds are fine (a flat backoff).
        assert!(validate_backoff_bounds(500, 500).is_ok());
    }

    #[test]
    fn zero_backoff_bounds_are_rejected() {
        assert!(validate_backoff_bounds(0, 30000).is_err());
        assert!(validate_backoff_bounds(1000, 0).is_err());
        assert!(validate_backoff_bounds(0, 0).is_err());
    }

    #[test]
    fn inverted_backoff_bounds_are_rejected() {
        let err = validate_backoff_bounds(30000, 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be"), "{err}");
    }
}
