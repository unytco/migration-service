//! The `Status` command's teardown gate: `safe_to_teardown` requires close +
//! open + **verify** all green, not just close + open. An opened-but-unverified
//! (or opened-but-mismatched) chain must NOT be reported safe to tear down,
//! since that would let an operator destroy the old droplet — the source of
//! truth — over a migration that never proved out.
//!
//! Plus the persisted-signal contract: `status` REPORTS the `safe_to_teardown`
//! the open service persisted at verify success (monotonic — true survives the
//! old side going down), and never re-runs a live verify; and the one-shot
//! connect is bounded so a down conductor degrades to a `false` report quickly
//! instead of looping forever.

mod support;

use std::time::Duration;

use headless_migrator::conductor::AppPresence;
use headless_migrator::config::Config;
use headless_migrator::open::probe_for_status;
use headless_migrator::policy::PolicyOpts;
use headless_migrator::probe::ClosedStatus;
use headless_migrator::state_file::{Phase, State, Step, VerifyReport};
use headless_migrator::status::{
    apply_closed_status, derive_old_chain_closed_if_new_server,
    reconcile_old_chain_closed_with_teardown, safe_to_teardown, StatusParams,
};
use headless_migrator::verify::fetch_and_compare;
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zfuel::fuel::ZFuel;

/// A `Config` whose conductor ports point nowhere (no conductor) and whose state
/// file is the given temp path, with a tiny status connect budget so the bounded
/// connect (fix 5) returns fast instead of looping on connection-refused.
fn down_cfg(tmp: &std::path::Path) -> Config {
    std::env::set_var("MIGRATION_AGENT_STATUS_CONNECT_BUDGET_MS", "150");
    Config {
        admin_port: 1, // unroutable — no conductor listening
        app_port: 1,
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        request_timeout_secs: 1,
        state_file: tmp.to_path_buf(),
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

fn tmp_state(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-status-test-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Pre-seed the state file as the open service would after a verified migration:
/// `safe_to_teardown = true`, new chain opened, a passing verify report.
fn seed_verified_open(path: &std::path::Path) {
    let mut s = State::new(Phase::Open, Step::Done, "new chain opened + verified");
    s.new_chain_opened = true;
    s.old_chain_closed = true;
    s.safe_to_teardown = true;
    s.verify = Some(VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        mismatches: vec![],
    });
    s.write(path).unwrap();
}

fn status_params() -> StatusParams {
    StatusParams {
        router_url: "http://127.0.0.1:1".into(), // unreachable router (old side down)
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_b64: holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_36(vec![
            3;
            36
        ]))
        .to_string(),
    }
}

#[test]
fn opened_but_unverified_is_not_safe_to_teardown() {
    // close + open green, but verify not yet green → NOT safe to tear down.
    assert!(
        !safe_to_teardown(true, true, false),
        "opened but unverified must not be safe to tear down"
    );
    // The full close+open+verify green is the ONLY combination that is safe.
    assert!(safe_to_teardown(true, true, true));
    // Any missing leg blocks it.
    assert!(!safe_to_teardown(false, true, true), "old not closed");
    assert!(!safe_to_teardown(true, false, true), "new not opened");
}

// ── Close-side report scoping (fix 3) ────────────────────────────────────

/// Fix 3b: the `new_chain_opened ⇒ old_chain_closed` derivation is NEW-server
/// only. On the close side (no router/new-DNA context) it must NOT fire — even a
/// `new_chain_opened = true` left in the record (e.g. a stale `OpeningStateSummary`
/// from the old DNA's own prior migration) must not force `old_chain_closed = true`
/// while the close is still in progress. On the new server the same input DOES
/// derive the implied-true.
#[test]
fn close_side_never_derives_old_chain_closed_from_opened() {
    // Close side (new_server = false): an opened flag must NOT derive closed.
    let mut close_side = State::new(Phase::Status, Step::Probing, "");
    close_side.new_chain_opened = true; // a stale/prior-migration opened read
    close_side.old_chain_closed = false;
    derive_old_chain_closed_if_new_server(&mut close_side, false);
    assert!(
        !close_side.old_chain_closed,
        "the close side must not derive old_chain_closed from a (possibly stale) opened read"
    );

    // New server (new_server = true): the derivation supplies the implied-true.
    let mut new_side = State::new(Phase::Status, Step::Probing, "");
    new_side.new_chain_opened = true;
    derive_old_chain_closed_if_new_server(&mut new_side, true);
    assert!(
        new_side.old_chain_closed,
        "the new server derives old_chain_closed=true from an open new chain"
    );
    assert!(!new_side.old_chain_closed_unknown);
}

/// Fix 3a: a close-side old-chain probe FAILURE reads UNKNOWN, not a definitive
/// `false`. `apply_closed_status` folds the tri-state: `Unknown` ⇒ `old_chain_closed
/// = false` BUT `old_chain_closed_unknown = true`, so a report reader can tell
/// "couldn't reach the conductor" from "reached it, chain still open"
/// (`NotClosed`).
#[test]
fn close_side_probe_failure_reads_unknown_not_false() {
    let mut unknown = State::new(Phase::Status, Step::Probing, "");
    apply_closed_status(&mut unknown, ClosedStatus::Unknown);
    assert!(
        !unknown.old_chain_closed,
        "unknown is not a definitive closed"
    );
    assert!(
        unknown.old_chain_closed_unknown,
        "a probe failure is flagged UNKNOWN, distinct from a definitive not-closed"
    );

    // A definitive not-closed (the conductor answered) is NOT flagged unknown.
    let mut not_closed = State::new(Phase::Status, Step::Probing, "");
    apply_closed_status(&mut not_closed, ClosedStatus::NotClosed);
    assert!(!not_closed.old_chain_closed);
    assert!(
        !not_closed.old_chain_closed_unknown,
        "a definitive not-closed is NOT unknown"
    );

    // Closed clears the unknown flag too.
    let mut closed = State::new(Phase::Status, Step::Probing, "");
    closed.old_chain_closed_unknown = true;
    apply_closed_status(&mut closed, ClosedStatus::Closed);
    assert!(closed.old_chain_closed);
    assert!(!closed.old_chain_closed_unknown);
}

/// Fix 3a end-to-end: a CLOSE-side `status::run` (no router params) against a down
/// conductor reports `old_chain_closed` as UNKNOWN (the probe couldn't run), never
/// a misleading definitive `false`, and does not run the new-side open probe.
#[tokio::test]
async fn close_side_status_run_reports_unknown_when_conductor_down() {
    let tmp = tmp_state("close-side-unknown");
    // Unroutable ports + tiny budget; NO router params → the close-side path.
    let cfg = down_cfg(&tmp);
    let state = headless_migrator::status::run(&cfg, None)
        .await
        .expect("close-side status run is Ok even with the conductor down");
    assert!(
        !state.old_chain_closed,
        "a down conductor is not a definitive closed"
    );
    assert!(
        state.old_chain_closed_unknown,
        "a close-side probe that couldn't reach the conductor reads UNKNOWN, not false"
    );
    assert!(
        !state.new_chain_opened,
        "the new-side open probe never runs on the close side"
    );
    assert!(state.message.contains("old_chain_closed=unknown"));
    // With no persisted latch the `unknown` stands — and that is still a
    // consistent row (`safe_to_teardown=false`), never the B46 contradiction.
    assert!(!state.safe_to_teardown);
    assert_no_contradictory_row(&state.message);
    let _ = std::fs::remove_file(&tmp);
}

// ── No self-contradictory status row (B46) ───────────────────────────────

/// Assert the rendered status line never shows the impossible pair
/// `safe_to_teardown=true` alongside `old_chain_closed=unknown`. The single home
/// for "the row is internally consistent", reused by the unit + end-to-end tests.
fn assert_no_contradictory_row(message: &str) {
    let contradictory =
        message.contains("safe_to_teardown=true") && message.contains("old_chain_closed=unknown");
    assert!(
        !contradictory,
        "status row contradicts itself (teardown-safe implies the old chain closed): {message:?}"
    );
}

/// B46 (the reconciliation predicate): a latched `safe_to_teardown` resolves the
/// old-chain question to a definitive closed — teardown-safe REQUIRES the old
/// chain to have closed — so the `unknown` flag is cleared and the row can't
/// contradict itself. A no-op when the latch is down (the probe's answer stands).
#[test]
fn reconcile_clears_unknown_when_teardown_is_latched() {
    // Latched true + an UNKNOWN old-chain probe (close-side conductor down) — the
    // exact pair the renderer must not emit.
    let mut latched = State::new(Phase::Status, Step::Probing, "");
    latched.safe_to_teardown = true;
    latched.old_chain_closed = false;
    latched.old_chain_closed_unknown = true;
    reconcile_old_chain_closed_with_teardown(&mut latched);
    assert!(
        latched.old_chain_closed,
        "a latched teardown-safe implies the old chain closed"
    );
    assert!(
        !latched.old_chain_closed_unknown,
        "the impossible `unknown` companion is cleared once the latch is up"
    );

    // Latch down → the probe's UNKNOWN answer is left exactly as-is (no over-reach).
    let mut not_latched = State::new(Phase::Status, Step::Probing, "");
    not_latched.safe_to_teardown = false;
    not_latched.old_chain_closed = false;
    not_latched.old_chain_closed_unknown = true;
    reconcile_old_chain_closed_with_teardown(&mut not_latched);
    assert!(
        not_latched.old_chain_closed_unknown,
        "with the latch down the probe's UNKNOWN must stand — reconciliation is a no-op"
    );
}

/// B46 end-to-end: a CLOSE-side `status::run` (no router params) against a DOWN
/// conductor whose state file already carries a verified-open latch is exactly
/// the case that used to render `old_chain_closed=unknown safe_to_teardown=true`
/// — the probe reads UNKNOWN (conductor unreachable) while the persisted latch is
/// `true`. The reconciled run must report a consistent row: `safe_to_teardown=true`
/// with `old_chain_closed=true`, never the contradictory pair.
#[tokio::test]
async fn close_side_status_with_latched_teardown_renders_no_contradictory_row() {
    let tmp = tmp_state("latched-teardown-down-conductor");
    seed_verified_open(&tmp); // persists safe_to_teardown = true
    let cfg = down_cfg(&tmp); // unroutable conductor → close-side probe reads UNKNOWN

    // Close side: no router params.
    let state = headless_migrator::status::run(&cfg, None)
        .await
        .expect("close-side status run is Ok even with the conductor down");

    assert!(
        state.safe_to_teardown,
        "the persisted monotonic latch is reported back (conductor down can't lower it)"
    );
    assert!(
        state.old_chain_closed,
        "teardown-safe implies the old chain closed — reconciled to a definitive closed"
    );
    assert!(
        !state.old_chain_closed_unknown,
        "the `unknown` probe result is reconciled away once the latch is up"
    );
    assert!(
        state.message.contains("old_chain_closed=true")
            && state.message.contains("safe_to_teardown=true"),
        "the row reads consistently: {:?}",
        state.message
    );
    assert_no_contradictory_row(&state.message);
    let _ = std::fs::remove_file(&tmp);
}

// ── Read-only new-chain probe (status must never drive `init`) ───────────

/// The new-chain probe makes NO zome call. On a cell installed with migration
/// `init_properties` the FIRST zome call drives `init` and opens the chain —
/// only the supervised open service may do that — so a diagnostic `status` must
/// answer from app presence + the open service's persisted signal alone. The
/// mock panics on any unscripted zome call, and the recorded calls prove
/// presence is the only conductor interaction.
#[tokio::test]
async fn new_chain_probe_is_read_only_and_reports_the_persisted_signal() {
    // Installed but not yet opened (persisted signal false): reports false
    // WITHOUT touching the chain — the exact cell a zome-call probe would have
    // opened as a side effect.
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    let opened = probe_for_status(&mock, "unyt", false).await.unwrap();
    assert!(
        !opened,
        "installed-but-unopened reports false from the persisted signal"
    );
    assert_eq!(
        mock.calls(),
        vec![Call::AppPresence],
        "presence is the ONLY conductor interaction — no zome call may run"
    );

    // Installed and the open service has stamped the open → true, still
    // presence-only.
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    let opened = probe_for_status(&mock, "unyt", true).await.unwrap();
    assert!(opened);
    assert_eq!(mock.calls(), vec![Call::AppPresence]);
}

/// Presence gates the persisted signal: a stale record (e.g. surviving an
/// uninstall) must not report an open chain that is not there.
#[tokio::test]
async fn new_chain_probe_reports_false_when_app_absent_despite_persisted_open() {
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Absent));
    let opened = probe_for_status(&mock, "unyt", true).await.unwrap();
    assert!(
        !opened,
        "no installed app ⇒ no open chain, whatever the record says"
    );
    assert_eq!(mock.calls(), vec![Call::AppPresence]);
}

/// Serve exactly one HTTP 200 with `body`, then close. Returns the base URL.
async fn one_shot_ok(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// The verify gate the status probe runs: a fetched package whose closing
/// balance does NOT match the live new-chain ledger yields a FAILING report —
/// so `verify_ok` is false and the teardown gate stays closed. Drives the same
/// `verify::fetch_and_compare` the open service and `status` share, with a
/// mock conductor for the ledger read and a one-shot router for the package.
#[tokio::test]
async fn verify_gate_fails_when_opened_chain_ledger_mismatches() {
    // Package the router serves: close summary with balance 100.
    let pkg = migration_init_request(
        3,
        summary_state(unit_map(0, 100), CarryForwardUnits::new(), 2),
    );
    let body: &'static str = Box::leak(serde_json::to_string(&pkg).unwrap().into_boxed_str());
    let base = one_shot_ok(body).await;

    // New-chain ledger DISAGREES (balance 1, not 100) — an unverified open.
    let mock = MockConductor::default();
    *mock.ledger.lock().unwrap() = Some(ledger(
        unit_map(0, 1),
        CarryForwardUnits::new(),
        ZFuel::zero(),
    ));

    let client = headless_migrator::fetch::http_client_for_status().unwrap();
    let report = fetch_and_compare(&mock, &client, &base, &dna_b64(1), &dna_b64(2), "uhCAk")
        .await
        .expect("router reachable, ledger read ok")
        .expect("package fetchable → a report is produced");
    assert!(
        !report.passed(),
        "a mismatched new-chain ledger must fail verify: {:?}",
        report.mismatches
    );
    // The gate the status probe applies → not safe to tear down.
    assert!(
        !safe_to_teardown(true, true, report.passed()),
        "an opened chain whose verify fails is not safe to tear down"
    );
}

// ── Persisted-signal + bounded-connect contract (driving `status::run`) ──────
//
// These run `status::run` against a conductor that ISN'T there (ports point at
// an unroutable address) with a tiny connect budget, so the bounded connect
// returns quickly and the report is assembled from the persisted state file +
// the (failing) probes — exactly the "old side / conductor down" scenario.

/// Fix 2 (monotonic teardown): after a verified migration persisted
/// `safe_to_teardown = true`, a LATER `status` run with the old side (and the
/// conductor) down still reports `safe_to_teardown = true` — it reads the
/// persisted signal rather than re-running a live verify (which can't run once
/// the old side is gone). The prior verify detail is preserved too.
#[tokio::test]
async fn status_reports_persisted_safe_to_teardown_after_teardown() {
    let tmp = tmp_state("persisted-teardown-monotonic");
    seed_verified_open(&tmp);

    let cfg = down_cfg(&tmp);
    let params = status_params();
    let state = headless_migrator::status::run(&cfg, Some(&params))
        .await
        .expect("status run is Ok even with everything down");

    assert!(
        state.safe_to_teardown,
        "a verified migration stays safe_to_teardown after the old side is gone (monotonic)"
    );
    assert_eq!(state.step, Step::Done);
    // The verify detail from the open service survives into the status report.
    assert!(
        state.verify.as_ref().map(|v| v.passed()).unwrap_or(false),
        "the persisted passing verify report is carried into the status record"
    );
    // Re-reading the freshly written status file still shows the monotonic bit.
    assert!(State::persisted_safe_to_teardown(&tmp));
    let _ = std::fs::remove_file(&tmp);
}

/// Fix 2 (no false positive): a standalone `status` with NO persisted verify
/// (no prior open-service success) reports `safe_to_teardown = false`, never a
/// live-verify guess.
#[tokio::test]
async fn status_without_persisted_verify_is_not_safe_to_teardown() {
    let tmp = tmp_state("no-persisted-verify");
    // No seed: the state file does not exist yet.
    let cfg = down_cfg(&tmp);
    let params = status_params();
    let state = headless_migrator::status::run(&cfg, Some(&params))
        .await
        .expect("status run is Ok");

    assert!(
        !state.safe_to_teardown,
        "no persisted verify ⇒ not safe to tear down"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// Fix 5 (bounded connect): `status::run` against a down conductor returns
/// promptly (within a small multiple of the tiny budget) instead of looping in
/// `ham::connect_with_backoff` forever.
#[tokio::test]
async fn status_connect_is_bounded_on_a_down_conductor() {
    let tmp = tmp_state("bounded-connect");
    let cfg = down_cfg(&tmp); // sets a 150ms budget
    let params = status_params();

    let started = std::time::Instant::now();
    let _ = headless_migrator::status::run(&cfg, Some(&params))
        .await
        .expect("status run is Ok");
    let elapsed = started.elapsed();
    // One bounded admin-only connect (the new-server path makes no zome call,
    // so it never attempts the full ham attach) + a router probe, each capped
    // near the 150ms budget — comfortably under the forever-loop this guards.
    assert!(
        elapsed < Duration::from_secs(5),
        "status must not loop on a down conductor; took {elapsed:?}"
    );
    let _ = std::fs::remove_file(&tmp);
}

/// An interleaved `status` run must not erase the open service's persisted
/// first-too-early stamp: the GD-wait budget is measured from that stamp
/// across supervised restarts, so dropping it would renew the full budget on
/// the next restart — the unbounded-retry class the bounded deadline closes.
/// The full scenario: open persists the stamp mid-wait → an operator/report
/// collector runs `status` → the restarted open service still seeds the
/// ORIGINAL stamp.
#[tokio::test]
async fn status_run_preserves_the_gd_wait_stamp_for_the_next_open_restart() {
    let tmp = tmp_state("gd-wait-stamp-preserved");

    // The open service hit a too-early successor GD and persisted the FIRST
    // too-early timestamp mid-wait.
    let mut open_state = State::new(
        Phase::Open,
        Step::OpeningChain,
        "waiting for the successor GD to come into effect",
    );
    open_state.gd_wait_started_us = Some(1_234_567);
    open_state.write(&tmp).unwrap();

    // An interleaved status report (new-server context, conductor down — the
    // report collector's usual mid-migration read).
    let cfg = down_cfg(&tmp);
    let params = status_params();
    let state = headless_migrator::status::run(&cfg, Some(&params))
        .await
        .expect("status run is Ok");
    assert_eq!(
        state.gd_wait_started_us,
        Some(1_234_567),
        "the status record carries the stamp through its rewrite"
    );

    // The subsequent open-service restart seeds the ORIGINAL stamp — the
    // budget resumes, it does not renew.
    let mut restarted = State::new(Phase::Open, Step::Probing, "");
    restarted.seed_from_persisted(&tmp);
    assert_eq!(
        restarted.gd_wait_started_us,
        Some(1_234_567),
        "a post-status open restart must resume the SAME GD-wait budget"
    );
    let _ = std::fs::remove_file(&tmp);
}
