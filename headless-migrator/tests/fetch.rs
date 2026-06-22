//! Package-fetch classification + an end-to-end round-trip against a local
//! one-shot HTTP responder. The load-bearing rule: `no_close_found` (and the
//! transient codes) AFTER a known close means propagation lag → KeepWaiting,
//! never a hard stop and never a fresh-agent fallback; only a true client /
//! contract fault hard-stops.

mod support;

use headless_migrator::fetch::{self, is_hard_stop, is_retryable, FetchOutcome};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[test]
fn no_close_found_is_not_a_hard_stop() {
    // The crux: a headless restoring agent must keep waiting on no_close_found.
    assert!(!is_hard_stop("no_close_found"));
    assert!(is_retryable("no_close_found"));
    // The genuinely transient codes are also keep-waiting (recognized retryable).
    for code in [
        "unable_to_verify",
        "all_orgs_unhealthy",
        "internal",
        "auth_failed",
        "rate_limited",
    ] {
        assert!(!is_hard_stop(code), "{code} should be keep-waiting");
        assert!(
            is_retryable(code),
            "{code} should be a recognized retryable"
        );
    }
}

#[test]
fn contract_faults_are_hard_stops() {
    for code in [
        "warranted",
        "bad_request",
        "unknown_to_dna",
        "unknown_from_dna",
        "unknown_current_dna",
        "to_is_chain_root",
        "not_registered_predecessor",
    ] {
        assert!(is_hard_stop(code), "{code} should be a hard stop");
        assert!(!is_retryable(code), "{code} is not retryable");
    }
}

#[test]
fn unknown_code_is_neither_retryable_nor_a_known_hard_stop() {
    // A code this agent has never seen (wire-contract drift) is NOT retryable —
    // the caller treats it as a hard stop rather than retrying forever. The two
    // sets are an explicit allowlist each, so an unknown code falls through both.
    let code = "some_future_code_we_have_never_seen";
    assert!(!is_hard_stop(code));
    assert!(!is_retryable(code));
}

/// Serve exactly one HTTP request with the given status + JSON body, then close.
/// Returns the bound base URL.
async fn one_shot_server(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            // Drain the request (best-effort; we don't parse it).
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

fn dna_b64(seed: u8) -> holo_hash::DnaHashB64 {
    holo_hash::DnaHashB64::from(holo_hash::DnaHash::from_raw_36(vec![seed; 36]))
}

fn agent_b64() -> String {
    holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_36(vec![7; 36])).to_string()
}

#[tokio::test]
async fn no_close_found_response_keeps_waiting() {
    let base = one_shot_server(
        "404 Not Found",
        r#"{"error":{"code":"no_close_found","message":"close on the from-DNA first"}}"#,
    )
    .await;
    let client = fetch::http_client_for_status().unwrap();
    let outcome =
        fetch::fetch_package(&client, &base, &dna_b64(1), &dna_b64(2), &agent_b64()).await;
    match outcome {
        FetchOutcome::KeepWaiting(why) => assert!(why.contains("no_close_found")),
        other => panic!("no_close_found must keep waiting, got {}", describe(&other)),
    }
}

#[tokio::test]
async fn warranted_response_hard_stops() {
    let base = one_shot_server(
        "422 Unprocessable Entity",
        r#"{"error":{"code":"warranted","message":"chain carries warrants"}}"#,
    )
    .await;
    let client = fetch::http_client_for_status().unwrap();
    let outcome =
        fetch::fetch_package(&client, &base, &dna_b64(1), &dna_b64(2), &agent_b64()).await;
    assert!(
        matches!(outcome, FetchOutcome::HardStop(_)),
        "warranted must hard stop, got {}",
        describe(&outcome)
    );
}

#[tokio::test]
async fn unrecognized_error_code_response_hard_stops() {
    // A non-2xx carrying a code outside both allowlists (wire drift) must hard
    // stop, not spin forever as a keep-waiting.
    let base = one_shot_server(
        "418 I'm a teapot",
        r#"{"error":{"code":"brand_new_unforeseen_code","message":"the contract drifted"}}"#,
    )
    .await;
    let client = fetch::http_client_for_status().unwrap();
    let outcome =
        fetch::fetch_package(&client, &base, &dna_b64(1), &dna_b64(2), &agent_b64()).await;
    match outcome {
        FetchOutcome::HardStop(why) => assert!(why.contains("unrecognized error code")),
        other => panic!(
            "an unrecognized code must hard stop, got {}",
            describe(&other)
        ),
    }
}

#[tokio::test]
async fn package_response_decodes() {
    // A 200 carrying a valid MigrationInitRequest decodes to Package.
    let payload = support::payload(
        3,
        support::summary_state(
            support::unit_map(0, 5),
            rave_engine::types::ledger::CarryForwardUnits::new(),
            0,
        ),
    );
    let body = serde_json::json!({
        "payload": payload,
        "notary_signatures": [],
        "close_action": support::action_hash(6),
    })
    .to_string();
    // Leak so the &'static str the helper wants is satisfied (test-only).
    let body: &'static str = Box::leak(body.into_boxed_str());
    let base = one_shot_server("200 OK", body).await;
    let client = fetch::http_client_for_status().unwrap();
    let outcome =
        fetch::fetch_package(&client, &base, &dna_b64(1), &dna_b64(2), &agent_b64()).await;
    assert!(
        matches!(outcome, FetchOutcome::Package(_)),
        "a valid 200 must decode to a package, got {}",
        describe(&outcome)
    );
}

#[tokio::test]
async fn router_unreachable_keeps_waiting() {
    // No server listening on this port → transport error → KeepWaiting.
    let client = fetch::http_client_for_status().unwrap();
    let outcome = fetch::fetch_package(
        &client,
        "http://127.0.0.1:1", // unroutable
        &dna_b64(1),
        &dna_b64(2),
        &agent_b64(),
    )
    .await;
    assert!(
        matches!(outcome, FetchOutcome::KeepWaiting(_)),
        "an unreachable router must keep waiting, got {}",
        describe(&outcome)
    );
}

fn describe(o: &FetchOutcome) -> String {
    match o {
        FetchOutcome::Package(_) => "Package".into(),
        FetchOutcome::KeepWaiting(w) => format!("KeepWaiting({w})"),
        FetchOutcome::HardStop(w) => format!("HardStop({w})"),
    }
}
