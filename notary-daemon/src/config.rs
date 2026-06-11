//! Daemon configuration, read from the environment (mirrors pricing_oracle's
//! `HolochainConfig::from_env`).

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct Config {
    /// Holochain conductor admin websocket port (local).
    pub admin_port: u16,
    /// Holochain conductor app websocket port (local).
    pub app_port: u16,
    /// Installed app id holding the alliance cell this daemon serves.
    pub app_id: String,
    /// DNA role name within the app (default `alliance`).
    pub role_name: String,
    /// Address this daemon's HTTP server binds (default `127.0.0.1` — fronted by
    /// a Cloudflare Tunnel, so it never needs a public bind).
    pub bind_addr: String,
    /// Port this daemon's HTTP server binds.
    pub bind_port: u16,
    /// Bearer token the router must present on `/v1/fetch-close`.
    pub bearer_token: String,
    /// `ham` per-request timeout (seconds).
    pub request_timeout_secs: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        fn var(key: &str) -> Option<String> {
            std::env::var(key).ok().filter(|v| !v.is_empty())
        }
        Ok(Self {
            admin_port: var("HOLOCHAIN_ADMIN_PORT")
                .unwrap_or_else(|| "8800".into())
                .parse()
                .context("HOLOCHAIN_ADMIN_PORT")?,
            app_port: var("HOLOCHAIN_APP_PORT")
                .unwrap_or_else(|| "30000".into())
                .parse()
                .context("HOLOCHAIN_APP_PORT")?,
            app_id: var("HOLOCHAIN_APP_ID").unwrap_or_else(|| "unyt".into()),
            role_name: var("HOLOCHAIN_ROLE_NAME").unwrap_or_else(|| "alliance".into()),
            bind_addr: var("MIGRATION_NOTARY_BIND_ADDR").unwrap_or_else(|| "127.0.0.1".into()),
            bind_port: var("MIGRATION_NOTARY_BIND_PORT")
                .unwrap_or_else(|| "8790".into())
                .parse()
                .context("MIGRATION_NOTARY_BIND_PORT")?,
            bearer_token: var("MIGRATION_NOTARY_BEARER_TOKEN")
                .context("MIGRATION_NOTARY_BEARER_TOKEN is required")?,
            request_timeout_secs: var("HAM_REQUEST_TIMEOUT_SECS")
                .unwrap_or_else(|| "30".into())
                .parse()
                .context("HAM_REQUEST_TIMEOUT_SECS")?,
        })
    }
}
