//! The migration-package fetch against the router (`POST /v1/migrate`). The
//! router returns the closing-summary package `{ payload, notary_signatures,
//! close_action }` verbatim, or the shared error envelope. For a headless
//! restoring agent, a `no_close_found` AFTER a known close can only be
//! propagation lag — so it (and the genuinely transient codes) maps to
//! `KeepWaiting`, NEVER a hard stop and NEVER a fresh-agent fallback. A true
//! client/contract fault (`warranted`, `bad_request`, an unreachable target)
//! is a hard stop — and so is an UNRECOGNIZED code: the retryable set is an
//! explicit allowlist (see [`crate::dna_errors`]), so a drifted wire contract
//! fails loud rather than retrying a possibly-permanent fault forever.

use std::time::Duration;

use anyhow::{Context, Result};
use holo_hash::DnaHashB64;
use rave_engine::types::entries::migration::v0_1::MigrationInitRequest;
use serde::Deserialize;

/// The outcome of one package fetch.
pub enum FetchOutcome {
    /// The package — ready to install + `migration_init`. Boxed so the large
    /// `MigrationInitRequest` doesn't bloat every `FetchOutcome` (the other
    /// variants hold only a `String`) — clears `clippy::large_enum_variant`.
    Package(Box<MigrationInitRequest>),
    /// Not yet available; the supervised loop should back off and retry. Holds
    /// a short reason for the log/state file.
    KeepWaiting(String),
    /// A non-recoverable fault — exit nonzero.
    HardStop(String),
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    #[serde(default)]
    message: String,
}

/// A `reqwest` client with a sane timeout for router calls (the `Status`
/// command's fetchability probe; the open service builds its own via
/// [`crate::joining::http_client`]).
pub fn http_client_for_status() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building router HTTP client")
}

/// Whether a router error `code` is a genuine hard stop for the migration. The
/// rest — crucially including `no_close_found` (which, after a known close, can
/// only be propagation lag for a headless restoring agent) — are "keep
/// waiting". The wire-code table itself lives in [`crate::dna_errors`] (one home
/// for every fragile error-string contract); this re-export keeps the call site
/// and the unit test (`fetch::is_hard_stop`) ergonomic.
pub use crate::dna_errors::router_code_is_hard_stop as is_hard_stop;
pub use crate::dna_errors::router_code_is_retryable as is_retryable;

/// Fetch the package for `agent_b64` migrating `from_dna` → `to_dna` via the
/// router at `router_url`.
pub async fn fetch_package(
    client: &reqwest::Client,
    router_url: &str,
    from_dna: &DnaHashB64,
    to_dna: &DnaHashB64,
    agent_b64: &str,
) -> FetchOutcome {
    let base = router_url.trim_end_matches('/');
    let body = serde_json::json!({
        "from_dna_hash": from_dna.to_string(),
        "to_dna_hash": to_dna.to_string(),
        "agent_pubkey": agent_b64,
    });
    let resp = match client
        .post(format!("{base}/v1/migrate"))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        // Transport failure reaching the router — keep waiting.
        Err(e) => return FetchOutcome::KeepWaiting(format!("router unreachable: {e}")),
    };

    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return FetchOutcome::KeepWaiting(format!("reading router response: {e}")),
    };

    if status.is_success() {
        return match serde_json::from_str::<MigrationInitRequest>(&text) {
            Ok(pkg) => FetchOutcome::Package(Box::new(pkg)),
            // A 200 that won't decode is our-side drift — surface it, but as a
            // transient so a flaky body doesn't kill the migration outright.
            Err(e) => FetchOutcome::KeepWaiting(format!("router 200 did not decode: {e}")),
        };
    }

    // Non-2xx: classify by the envelope's code.
    match serde_json::from_str::<ErrorEnvelope>(&text) {
        Ok(env) => {
            let code = env.error.code;
            let msg = if env.error.message.is_empty() {
                code.clone()
            } else {
                env.error.message
            };
            if is_hard_stop(&code) {
                FetchOutcome::HardStop(format!("router {code}: {msg}"))
            } else if is_retryable(&code) {
                // no_close_found / unable_to_verify / all_orgs_unhealthy /
                // internal / auth_failed / rate_limited → keep waiting.
                FetchOutcome::KeepWaiting(format!("router {code}: {msg}"))
            } else {
                // An UNRECOGNIZED code — the wire contract drifted (a code added
                // router-side this agent has never seen). Surface it as a hard
                // stop rather than retrying a possibly-permanent fault forever;
                // `error.code` is a fixed enum whose distinctions are load-bearing,
                // so an unknown one is treated as non-recoverable, not transient.
                FetchOutcome::HardStop(format!("router unrecognized error code {code}: {msg}"))
            }
        }
        // An unparseable error body — treat as transient.
        Err(_) => FetchOutcome::KeepWaiting(format!("router HTTP {status}: {text}")),
    }
}
