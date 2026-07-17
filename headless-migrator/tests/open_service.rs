//! Fleet-free rail test for the open service: with the conductor injected
//! (B2's `open::run_with` + `MockConnector`), drive the whole supervised loop to
//! GD-wait exhaustion against a mock — no live conductor — and assert the
//! persisted state + returned error carry the actionable CONFIG-FAULT diagnosis
//! (B1), NOT a raw genesis error.

mod support;

use std::sync::Arc;
use std::time::Duration;

use headless_migrator::conductor::AppPresence;
use headless_migrator::config::{Config, OpenConfig};
use headless_migrator::open::{self, OpenParams};
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{State, Step};
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve exactly one HTTP request with `status_line` + JSON `body`, then close.
/// Returns the bound base URL. (Mirrors `tests/fetch.rs`'s helper; each test
/// crate is standalone, so it carries its own.)
async fn one_shot_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    format!("http://{addr}")
}

fn tmp_state(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-open-service-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// A `Config` with snappy retries; conductor ports are irrelevant (the mock
/// connector is injected, so nothing dials them).
fn cfg(state_file: std::path::PathBuf) -> Config {
    Config {
        admin_port: 1,
        app_port: 1,
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        request_timeout_secs: 1,
        state_file,
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(2),
        policy: PolicyOpts {
            request_timeout: Duration::from_secs(1),
            state_mismatch_retries: 1,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(2),
        },
        to_dna: None,
    }
}

fn never_shutdown() -> ham::ShutdownRx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    rx
}

/// A valid `MigrationInitRequest` body so the router fetch succeeds (letting the
/// loop reach the init/verify call). Built the same way `tests/fetch.rs` does —
/// the type is wire-decoded, so the JSON is assembled by field.
fn package_body() -> &'static str {
    let body = serde_json::json!({
        "payload": payload(3, summary_state(unit_map(0, 5), CarryForwardUnits::new(), 0)),
        "notary_signatures": [],
        "close_action": action_hash(6),
    })
    .to_string();
    Box::leak(body.into_boxed_str())
}

#[tokio::test]
async fn gd_wait_exhaustion_reports_a_config_fault_not_a_raw_genesis_error() {
    let state_file = tmp_state("gd-exhaust");

    // The new server already has the app installed but not yet verified, so the
    // loop skips install (no joining-service call) and goes straight to fetch →
    // connect ham → drive init. `init` reports the successor GD is not in effect
    // on every pass.
    let mock = Arc::new(MockConductor::default());
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.verify_migrated
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(
            "wasm error: No Global Definition found"
        )));

    // The router hands back a valid package so the one fetch succeeds.
    let router = one_shot_server("200 OK", package_body()).await;

    // A happ bundle must exist for `assert_happ_path` (its contents don't matter
    // — install is skipped on the Installed path).
    let happ = tmp_state("dummy-happ");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let connector = MockConnector::shared(mock.clone());
    let cfg = cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: "http://127.0.0.1:1".into(),
        network_seed: None,
        // Zero budget: the FIRST too-early exhausts immediately (single pass,
        // single fetch), so the one-shot router suffices.
        gd_wait_timeout: Duration::ZERO,
    };
    let params = OpenParams {
        router_url: router,
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_key: agent(3),
        lair_url: "unix:///nonexistent".into(),
        lair_passphrase: "x".into(),
    };

    let mut sd = never_shutdown();
    let err = open::run_with(&connector, &cfg, &open_cfg, &params, &mut sd)
        .await
        .expect_err("an exhausted GD wait must fail the open service")
        .to_string();

    // The returned error is the config-fault diagnosis, carrying both DNAs and
    // the raw init cause as a trailing detail — NOT the old raw-genesis bail.
    assert!(
        err.contains("check the successor DNA hash / registry"),
        "error leads with the config-fault diagnosis: {err}"
    );
    assert!(
        err.contains(&params.from_dna.to_string()) && err.contains(&params.to_dna.to_string()),
        "error carries both DNAs: {err}"
    );
    assert!(
        err.contains("No Global Definition found"),
        "error keeps the raw init cause: {err}"
    );
    assert!(
        !err.contains("gave up waiting for the successor GD"),
        "must not be the old raw-genesis bail: {err}"
    );

    // The persisted state file (which the automation rail cats out) carries the
    // same diagnosis, at Step::Failed.
    let state = State::read(&state_file).unwrap();
    assert_eq!(state.step, Step::Failed);
    assert!(
        state
            .message
            .contains("check the successor DNA hash / registry"),
        "persisted message is the config-fault diagnosis: {}",
        state.message
    );
    assert!(
        state.message.contains("No Global Definition found"),
        "persisted message keeps the raw init cause: {}",
        state.message
    );

    // The init error surfaced via the mock (drove init), confirming the loop
    // reached the verify call rather than failing earlier.
    assert!(
        mock.calls().contains(&Call::VerifyIfMigrated),
        "the loop drove init via verify_if_migrated: {:?}",
        mock.calls()
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}
