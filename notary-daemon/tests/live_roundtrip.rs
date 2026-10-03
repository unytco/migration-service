//! Gated live round-trip: the REAL daemon (axum server + real `ham`) against a
//! locally running conductor whose cell is a closing notary on its DNA, with one
//! agent whose chain has closed. Locks the serde round-trip the mocked tests
//! bypass: the served package must decode with the same `rave_engine` types the
//! app consumes, carrying this notary's one signature.
//!
//! Ignored by default. Stand the fixture up with the unyt repo's sweettest
//! tooling (its migration scenario builds closing notaries and closes an agent),
//! exposing that notary conductor's admin and app interfaces, then run from
//! `notary-daemon/`:
//!
//! ```bash
//! MIGRATION_NOTARY_BEARER_TOKEN=test-token \
//! HOLOCHAIN_ADMIN_PORT=<admin-port> \
//! HOLOCHAIN_APP_PORT=<app-port> \
//! HOLOCHAIN_APP_ID=<installed-app-id> \
//! HOLOCHAIN_ROLE_NAME=alliance \
//! MIGRATION_NOTARY_BIND_PORT=8790 \
//! MIGRATION_NOTARY_LAIR_URL=<the conductor's keystore.connection_url> \
//! MIGRATION_NOTARY_LAIR_PASSPHRASE=<its passphrase> \
//! LIVE_CLOSED_AGENT_B64=<uhCAk... of the closed agent> \
//! cargo test --test live_roundtrip -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;

use migration_notary::conductor::{Conductor, HamConductor};
use migration_notary::config::Config;
use migration_notary::serve_with_conductor;

use rave_engine::types::entries::migration::v0_2::MigrationInitRequest;

#[tokio::test]
#[ignore = "needs a live conductor + closed-agent fixture; see the file header for the run command"]
async fn live_healthz_and_attest_close() -> anyhow::Result<()> {
    let cfg = Config::from_env().context("daemon env vars (see file header)")?;
    let agent_b64 = std::env::var("LIVE_CLOSED_AGENT_B64")
        .context("LIVE_CLOSED_AGENT_B64 is required (a closed agent on the served DNA)")?;
    let base = format!("http://{}:{}", cfg.bind_addr, cfg.bind_port);
    let token = cfg.bearer_token.clone();

    // Real ham connection to the live conductor, then the real HTTP server.
    let mut shutdown = ham::install_shutdown_handler();
    let conductor = HamConductor::connect(&cfg, &mut shutdown)
        .await
        .context("conductor never became reachable")?;
    let notary = conductor.whoami().await.context("the notary's own agent")?;
    let server = tokio::spawn(serve_with_conductor(cfg, Arc::new(conductor), shutdown));

    // Wait for the listener to come up.
    let client = reqwest::Client::new();
    let mut healthz = None;
    for _ in 0..50 {
        match client.get(format!("{base}/healthz")).send().await {
            Ok(resp) => {
                healthz = Some(resp);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let healthz = healthz.context("daemon HTTP server never came up")?;

    // /healthz: both checks green against the live conductor + cell.
    assert_eq!(
        healthz.status(),
        200,
        "healthz must be 200 against a live cell"
    );
    let health: serde_json::Value = healthz.json().await?;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["api_versions"], serde_json::json!(["v2"]));
    assert_eq!(health["protocol_versions"], serde_json::json!(["v0_2"]));

    let resp = client
        .post(format!("{base}/v2/attest-close"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "agent_pubkey": agent_b64 }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    assert_eq!(
        status, 200,
        "attest-close must attest the closed agent's chain, got {status}: {body}"
    );

    let package: MigrationInitRequest =
        serde_json::from_str(&body).context("package must decode with rave_engine types")?;
    assert_eq!(
        package
            .notary_signatures
            .iter()
            .map(|s| s.notary.clone())
            .collect::<Vec<_>>(),
        vec![notary],
        "the package carries exactly this notary's signature"
    );
    let requested: holo_hash::AgentPubKey = holo_hash::AgentPubKeyB64::from_b64_str(&agent_b64)
        .context("agent b64")?
        .into();
    assert_eq!(
        package.payload.agent_pubkey, requested,
        "the package is the requested agent's close"
    );

    server.abort();
    Ok(())
}
