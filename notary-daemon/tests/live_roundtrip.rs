//! Gated live round-trip: the REAL daemon (axum server + real `ham`) against a
//! locally running conductor hosting the alliance app with one already-closed
//! agent. Locks the serde round-trip the mocked tests bypass: the served
//! package must decode with the same `rave_engine` types the app consumes.
//!
//! Ignored by default. Stand the fixture up with the unyt repo's test tooling
//! (a conductor whose chain has completed a close — e.g. pause a sweettest
//! migration scenario after the close, or use `make launch-tauri` + the close
//! flow), then run from `notary-daemon/`:
//!
//! ```bash
//! MIGRATION_NOTARY_BEARER_TOKEN=test-token \
//! HOLOCHAIN_ADMIN_PORT=<admin-port> \
//! HOLOCHAIN_APP_PORT=<app-port> \
//! HOLOCHAIN_APP_ID=<installed-app-id> \
//! HOLOCHAIN_ROLE_NAME=alliance \
//! MIGRATION_NOTARY_BIND_PORT=8790 \
//! LIVE_CLOSED_AGENT_B64=<uhCAk... of the closed agent> \
//! cargo test --test live_roundtrip -- --ignored --nocapture
//! ```

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;

use migration_notary::conductor::HamConductor;
use migration_notary::config::Config;
use migration_notary::serve_with_conductor;

use rave_engine::types::entries::migration::v0_1::{
    MigrationInitRequest, NotarySignature, SummaryStatePayload,
};

/// The three-field package exactly as the app consumes it — decoding into
/// these types IS the assertion the mocked suite cannot make.
#[derive(Deserialize)]
struct Package {
    payload: SummaryStatePayload,
    notary_signatures: Vec<NotarySignature>,
    close_action: holo_hash::ActionHash,
}

#[tokio::test]
#[ignore = "needs a live conductor + closed-agent fixture; see the file header for the run command"]
async fn live_healthz_and_fetch_close() -> anyhow::Result<()> {
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
    assert_eq!(health["api_versions"], serde_json::json!(["v1"]));
    assert_eq!(health["protocol_versions"], serde_json::json!(["v0_1"]));

    // /v1/fetch-close: the closed agent's package over real HTTP + real zome calls.
    let resp = client
        .post(format!("{base}/v1/fetch-close"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "agent_pubkey": agent_b64 }))
        .send()
        .await?;
    let status = resp.status();
    let body = resp.text().await?;
    assert_eq!(
        status, 200,
        "fetch-close must serve the closed agent's package, got {status}: {body}"
    );

    // Decode with the app's own types — the serde round-trip lock.
    let package: Package =
        serde_json::from_str(&body).context("package must decode with rave_engine types")?;
    assert!(
        !package.notary_signatures.is_empty(),
        "a committed close carries its collected notary signatures"
    );
    let requested: holo_hash::AgentPubKey = holo_hash::AgentPubKeyB64::from_b64_str(&agent_b64)
        .context("agent b64")?
        .into();
    assert_eq!(
        package.payload.agent_pubkey, requested,
        "the package is the requested agent's close"
    );

    // And it assembles into exactly what the app submits on the new DNA.
    let _init = MigrationInitRequest {
        payload: package.payload,
        notary_signatures: package.notary_signatures,
        close_action: package.close_action,
    };

    server.abort();
    Ok(())
}
