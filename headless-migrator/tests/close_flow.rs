//! Close-service flow against the mock conductor: fees-owed → `drop_off_fees`
//! precedes `prepare_closing_summary`; close is a no-op on an already-closed
//! chain; a warranted notary hard-stops; a happy path drives prepare → M notary
//! checks → close in order. Drives the real `close::run` loop with the injected mock
//! (no live conductor), so the ordering + idempotency contract is proven.

mod support;

use std::time::Duration;

use headless_migrator::close;
use headless_migrator::config::Config;
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{Phase, State, Step};
use rave_engine::types::entries::migration::v0_2::CloseCheckResponse;
use rave_engine::types::ledger::CarryForwardUnits;
use rave_engine::types::units::UnitMap;
use support::*;

/// A `Config` pointing at a unique temp state file, with snappy retries so the
/// supervised loop's backoff doesn't slow the test.
fn cfg(tmp: &std::path::Path) -> Config {
    Config {
        admin_port: 8800,
        app_port: 30000,
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        request_timeout_secs: 5,
        state_file: tmp.to_path_buf(),
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(2),
        policy: PolicyOpts {
            request_timeout: Duration::from_secs(1),
            state_mismatch_retries: 2,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(2),
        },
        signing: support::signing(),
        to_dna: Some(dna(2).into()),
    }
}

fn tmp_state(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-test-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// A shutdown receiver that never fires (the loop exits on its own terminal
/// state).
fn never_shutdown() -> ham::ShutdownRx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    rx
}

/// Drive one close over an open chain whose ledger owes `fees_owed`, returning
/// the calls the conductor saw. Everything but the fee gate is held fixed.
async fn close_with_fees_owed(name: &str, fees_owed: UnitMap) -> Vec<Call> {
    let tmp = tmp_state(name);
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        fees_owed,
    )));
    *mock.drop_fees.lock().unwrap() = Some(Ok("Fees dropped off".into()));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    mock.check_responses
        .lock()
        .unwrap()
        .push_back(Ok(CloseCheckResponse::Approved));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("close run Ok");
    let _ = std::fs::remove_file(&tmp);
    mock.calls()
}

#[tokio::test]
async fn no_op_on_already_closed_chain() {
    let tmp = tmp_state("closed-noop");
    let mock = MockConductor::default();
    // Probe reads a committed close → already closed.
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("closed-chain run is Ok");

    let calls = mock.calls();
    assert!(
        calls.contains(&Call::GetMigrationCloseState),
        "probes the close state"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, Call::PrepareClosingSummary { .. })),
        "never prepares on an already-closed chain: {calls:?}"
    );
    assert!(!calls.contains(&Call::CloseAgentChain), "never re-closes");
    let state = State::read(&tmp).unwrap();
    assert!(state.old_chain_closed);
    assert_eq!(state.step, Step::Done);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn missing_to_dna_fails_the_close_service() {
    // The close binds to a configured successor; an unset MIGRATION_AGENT_TO_DNA
    // (cfg.to_dna == None) fails the close service up front, before any probe.
    let tmp = tmp_state("missing-to-dna");
    let mock = MockConductor::default();
    let mut c = cfg(&tmp);
    c.to_dna = None;

    let mut sd = never_shutdown();
    let err = close::run(&mock, &c, &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("MIGRATION_AGENT_TO_DNA is required"), "{err}");
    assert!(
        mock.calls().is_empty(),
        "fails before touching the conductor: {:?}",
        mock.calls()
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn bad_to_dna_hard_stops_instead_of_looping() {
    // A target not in the source GD's upgrade_targets makes `prepare_closing_summary`
    // error with the rejection string; the close service must HARD-STOP (exit
    // nonzero), not classify it transient and loop forever.
    let tmp = tmp_state("bad-to-dna");
    let mock = MockConductor::default();
    // Probe: open chain (no committed summary yet).
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    // Ledger: no fees owed.
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 0),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    // Prepare errors with the extern's target pre-check rejection.
    *mock.prepare.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "prepare_closing_summary zome call failed: target DNA DnaHash(uhC0k) \
         is not in this network's upgrade_targets"
    )));

    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hard-stopped") && err.contains("upgrade_targets"),
        "a misconfigured target must hard-stop, not loop: {err}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn already_closed_restart_retains_agent_attribution() {
    // Restart onto an already-closed chain: `attempt` returns Closed straight
    // from the probe (no prepare/check), but the persisted record must still
    // carry the agent, recovered from the committed close the probe read. The
    // close carries no approvals, so their counts stay unset.
    let tmp = tmp_state("closed-restart-attribution");
    let mock = MockConductor::default();
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("closed-chain restart is Ok");

    // Never re-prepares / re-closes on the already-closed path.
    let calls = mock.calls();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, Call::PrepareClosingSummary { .. })),
        "{calls:?}"
    );
    assert!(!calls.contains(&Call::CloseAgentChain), "{calls:?}");

    let state = State::read(&tmp).unwrap();
    assert_eq!(state.step, Step::Done);
    assert!(state.old_chain_closed);
    let expected_agent =
        holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_36(vec![3; 36]))
            .to_string();
    assert_eq!(
        state.agent.as_deref(),
        Some(expected_agent.as_str()),
        "the agent is recovered from the committed close on the restart path"
    );
    assert_eq!(state.approvals_collected, None);
    assert_eq!(state.approvals_threshold, None);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn fees_owed_drops_before_prepare() {
    let tmp = tmp_state("fees-before-prepare");
    let mock = MockConductor::default();
    // Probe: open chain (no summary yet).
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    // Ledger: fees owed → must drop first.
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        unit_map(0, 5),
    )));
    *mock.drop_fees.lock().unwrap() = Some(Ok("Fees dropped off".into()));
    // Prepare: one notary, threshold 1.
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    // The single notary approves.
    mock.check_responses
        .lock()
        .unwrap()
        .push_back(Ok(CloseCheckResponse::Approved));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("close run Ok");

    let calls = mock.calls();
    let drop_idx = calls.iter().position(|c| *c == Call::DropOffFees);
    let prep_idx = calls
        .iter()
        .position(|c| matches!(c, Call::PrepareClosingSummary { .. }));
    assert!(drop_idx.is_some(), "fees were dropped: {calls:?}");
    assert!(prep_idx.is_some(), "summary was prepared: {calls:?}");
    assert!(
        drop_idx < prep_idx,
        "drop_off_fees must precede prepare_closing_summary: {calls:?}"
    );
    // The close binds to the configured to_dna (cfg() sets dna(2)).
    assert!(
        matches!(&calls[prep_idx.unwrap()], Call::PrepareClosingSummary { target } if *target == dna(2)),
        "prepare_closing_summary must bind to the configured to_dna dna(2): {calls:?}"
    );
    assert!(
        calls.contains(&Call::CloseAgentChain),
        "the chain is closed after the check"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn fees_owed_on_a_non_base_unit_also_drops_before_prepare() {
    // The behavioural gain of the per-unit shape: under a single `ZFuel` a
    // service-unit debt was unrepresentable, so a chain owing only those would
    // have prepared its summary with the fees still outstanding.
    let calls = close_with_fees_owed("fees-non-base-unit", unit_map(3, 5)).await;
    let drop_idx = calls.iter().position(|c| *c == Call::DropOffFees);
    let prep_idx = calls
        .iter()
        .position(|c| matches!(c, Call::PrepareClosingSummary { .. }));
    assert!(drop_idx.is_some(), "the debt must be dropped: {calls:?}");
    assert!(prep_idx.is_some(), "the summary is prepared: {calls:?}");
    assert!(
        drop_idx < prep_idx,
        "drop_off_fees must precede prepare_closing_summary: {calls:?}"
    );
}

#[tokio::test]
async fn undecodable_ledger_hard_stops_instead_of_looping() {
    let tmp = tmp_state("undecodable-ledger");
    let mock = MockConductor::default();
    // Two scripted probes, so a regression that loops fails on the assertion
    // below rather than on the mock running out of script.
    for _ in 0..2 {
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    }
    *mock.ledger.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "get_ledger zome call failed: Failed to deserialize response: \
         invalid type: string \"5\", expected a map"
    )));

    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hard-stopped") && err.contains("reading ledger for fee check"),
        "a schema mismatch must hard-stop, not loop: {err}"
    );
    assert!(
        err.contains("Rebuild the migrator"),
        "the hard stop names the only remedy: {err}"
    );
    assert!(
        !mock
            .calls()
            .contains(&Call::PrepareClosingSummary { target: dna(2) }),
        "nothing is prepared once the ledger cannot be read: {:?}",
        mock.calls()
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn a_transient_ledger_failure_still_retries() {
    // The other half of the gate: only a DECODE failure is terminal. A websocket
    // blip must keep backing off, or an ordinary hiccup mid-window aborts the
    // close and pages an operator.
    let tmp = tmp_state("transient-ledger");
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "get_ledger zome call failed: Failed to call zome: Websocket error: \
         Websocket closed: No connection"
    )));

    // `never_shutdown`'s sender is already dropped, so the first backoff ends the
    // run: reaching it at all proves the failure was classed transient.
    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "a transport blip must back off, not hard-stop: {err}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn undecodable_close_state_hard_stops_instead_of_looping() {
    // The same rule one step earlier: a probe response that will not decode
    // leaves the chain's real state unknowable, so it must not be folded into
    // "open, re-prepare" and driven at.
    let tmp = tmp_state("undecodable-close-state");
    let mock = MockConductor::default();
    for _ in 0..2 {
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!(
                "get_migration_close_state zome call failed: Failed to deserialize \
                 response: missing field `agreement_carry_forward`"
            )));
    }

    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hard-stopped") && err.contains("probing close state"),
        "an undecodable close state must hard-stop, not loop: {err}"
    );
    assert!(
        !mock.calls().contains(&Call::GetLedger),
        "the close never proceeds past an unreadable probe: {:?}",
        mock.calls()
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn undecodable_check_response_hard_stops_without_blaming_the_notaries() {
    // Reachable when `PrepareCloseResponse` decodes and `CloseCheckResponse`
    // does not, e.g. upgraded notaries answering with a variant this binary does
    // not know. Collapsed into a per-notary error it substitutes across the whole
    // list and reports an exhausted N-list, sending the operator to check notary
    // health for a fault in the binary they are running.
    let tmp = tmp_state("undecodable-check");
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(
        3,
        closing,
        vec![agent(70), agent(71)],
        1,
    )));
    // One per notary, so a regression that substitutes fails on the assertion.
    for _ in 0..2 {
        mock.check_responses
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!(
                "request_close_check zome call failed: Failed to deserialize \
                 response: unknown variant `Deferred`"
            )));
    }

    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hard-stopped") && err.contains("Rebuild the migrator"),
        "a schema mismatch must hard-stop with its remedy: {err}"
    );
    assert!(
        !err.contains("notary list exhausted"),
        "the notaries must not be blamed for this binary's schema mismatch: {err}"
    );
    assert_eq!(
        mock.calls()
            .iter()
            .filter(|c| **c == Call::RequestCloseCheck)
            .count(),
        1,
        "no substitution: {:?}",
        mock.calls()
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn an_undecodable_write_response_still_retries() {
    // The read/write split: a WRITE whose response did not decode says nothing
    // about whether the write landed, and the next pass's probe reads that back.
    // Hard-stopping here would report a close that may well have succeeded as a
    // failure.
    let tmp = tmp_state("undecodable-write");
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    mock.check_responses
        .lock()
        .unwrap()
        .push_back(Ok(CloseCheckResponse::Approved));
    *mock.close_result.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "close_agent_chain zome call failed: Failed to deserialize response: \
         invalid length 32, expected 39"
    )));

    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "an undecodable write response must re-probe, not hard-stop: {err}"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn no_fee_drop_when_none_owed() {
    // Both shapes of "owes nothing": the empty map the DNA keeps (it strips zero
    // entries), and an explicit zero, so the gate does not rest on that invariant.
    for (name, owed) in [
        ("no-fee-drop-empty", UnitMap::new()),
        ("no-fee-drop-zero", unit_map(0, 0)),
    ] {
        let calls = close_with_fees_owed(name, owed).await;
        assert!(
            !calls.contains(&Call::DropOffFees),
            "{name}: no fee drop when none owed: {calls:?}"
        );
        assert!(
            calls.contains(&Call::CloseAgentChain),
            "{name}: the close still runs: {calls:?}"
        );
    }
}

#[tokio::test]
async fn closed_state_retains_agent_and_approval_progress() {
    // After a successful close the persisted state must still carry the agent
    // and the approvals_collected/threshold set during the check: the report
    // collector (`make migrate-status`) reads these.
    let tmp = tmp_state("closed-retains-progress");
    let mock = MockConductor::default();
    // Open chain → prepare with threshold 2 over two notaries, both approve.
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(
        7,
        closing,
        vec![agent(70), agent(71)],
        2,
    )));
    for _ in 0..2 {
        mock.check_responses
            .lock()
            .unwrap()
            .push_back(Ok(CloseCheckResponse::Approved));
    }

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("close run Ok");

    let state = State::read(&tmp).unwrap();
    assert_eq!(state.step, Step::Done);
    assert!(state.old_chain_closed);
    // The agent prepared over (seed 7) must survive into the closed record.
    let expected_agent =
        holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_36(vec![7; 36]))
            .to_string();
    assert_eq!(
        state.agent.as_deref(),
        Some(expected_agent.as_str()),
        "the agent persists into the final closed state"
    );
    assert_eq!(
        state.approvals_threshold,
        Some(2),
        "the threshold persists into the final closed state"
    );
    assert_eq!(
        state.approvals_collected,
        Some(2),
        "the collected count persists into the final closed state"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn shutdown_before_close_exits_nonzero_and_preserves_prior_report() {
    // A supervised one-shot exits 0 only on success: a shutdown fired before the
    // chain is closed must return `Err` (nonzero exit), so systemd's
    // `Restart=on-failure` resumes the loop rather than treating the interrupted
    // run as done. The shutdown is pre-fired, so the loop bails on its very first
    // top-of-loop check before any probe — and must NOT clobber the richer report
    // a prior pass wrote (agent + approval attribution), since a fresh process
    // starts from an all-`None` in-memory `State`.
    let tmp = tmp_state("shutdown-before-close");
    let mock = MockConductor::default();

    // A prior pass left a report with attribution on disk (mid-collection).
    let mut prior = State::new(Phase::Close, Step::CollectingApprovals, "collecting");
    prior.agent = Some("uhCAk-prior-agent".into());
    prior.approvals_collected = Some(2);
    prior.approvals_threshold = Some(3);
    prior.write(&tmp).unwrap();

    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap();
    let mut sd = rx;

    let result = close::run(&mock, &cfg(&tmp), &mut sd).await;
    assert!(
        result.is_err(),
        "an incomplete close interrupted by shutdown exits nonzero (not Ok)"
    );
    assert!(
        mock.calls().is_empty(),
        "a pre-fired shutdown bails before probing: {:?}",
        mock.calls()
    );

    // The prior report survives untouched: the bail does NOT write the bare
    // in-memory `State` over the attribution a prior pass recorded. (Not `Failed`
    // either — a shutdown is an interruption a restart resumes, not a hard stop.)
    let state = State::read(&tmp).unwrap();
    assert_eq!(
        state.step,
        Step::CollectingApprovals,
        "prior step preserved"
    );
    assert_eq!(state.agent.as_deref(), Some("uhCAk-prior-agent"));
    assert_eq!(state.approvals_collected, Some(2));
    assert_ne!(state.step, Step::Failed);
    assert!(!state.old_chain_closed);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn warranted_notary_hard_stops_the_close() {
    let tmp = tmp_state("warranted-hardstop");
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    // The notary returns Warranted → the whole migration hard-stops.
    mock.check_responses
        .lock()
        .unwrap()
        .push_back(Ok(CloseCheckResponse::Warranted(vec![])));

    let mut sd = never_shutdown();
    let result = close::run(&mock, &cfg(&tmp), &mut sd).await;
    assert!(result.is_err(), "warranted must hard-stop (nonzero exit)");
    assert!(
        !mock.calls().contains(&Call::CloseAgentChain),
        "never closes on a warranted hard stop"
    );
    let state = State::read(&tmp).unwrap();
    assert_eq!(state.step, Step::Failed);
    let _ = std::fs::remove_file(&tmp);
}

/// An open chain owing nothing, prepared over `notaries` with threshold `m`,
/// whose notaries answer the check with `checks` in order.
fn open_chain(
    notaries: Vec<holo_hash::AgentPubKey>,
    m: u32,
    checks: Vec<CloseCheckResponse>,
) -> MockConductor {
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(
            "[MIGERR:MIG_NO_CLOSING_SUMMARY] No closing state summary found"
        )));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        UnitMap::new(),
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, notaries, m)));
    for check in checks {
        mock.check_responses.lock().unwrap().push_back(Ok(check));
    }
    mock
}

async fn run_close(name: &str, mock: &MockConductor) -> (Result<(), String>, Vec<Call>) {
    let tmp = tmp_state(name);
    let mut sd = never_shutdown();
    let result = close::run(mock, &cfg(&tmp), &mut sd)
        .await
        .map_err(|e| format!("{e:#}"));
    let _ = std::fs::remove_file(&tmp);
    (result, mock.calls())
}

#[tokio::test]
async fn m_notaries_approve_before_the_close() {
    let mock = open_chain(
        vec![agent(70), agent(71), agent(72)],
        2,
        vec![CloseCheckResponse::Approved, CloseCheckResponse::Approved],
    );
    let (result, calls) = run_close("m-approve-then-close", &mock).await;
    result.expect("the chain closes");
    let checks: Vec<usize> = calls
        .iter()
        .enumerate()
        .filter(|(_, c)| **c == Call::RequestCloseCheck)
        .map(|(i, _)| i)
        .collect();
    let close = calls
        .iter()
        .position(|c| *c == Call::CloseAgentChain)
        .unwrap();
    assert_eq!(checks.len(), 2, "exactly M notaries asked: {calls:?}");
    assert!(
        checks.iter().all(|i| *i < close),
        "checks precede the close: {calls:?}"
    );
}

#[tokio::test]
async fn a_refusing_notary_is_asked_again_then_counts() {
    for refusal in [
        CloseCheckResponse::StateMismatch,
        CloseCheckResponse::TargetNotApproved,
    ] {
        let mock = open_chain(
            vec![agent(70)],
            1,
            vec![refusal.clone(), CloseCheckResponse::Approved],
        );
        let (result, calls) = run_close("refusal-then-approve", &mock).await;
        result.unwrap_or_else(|e| panic!("{refusal:?}: {e}"));
        assert_eq!(
            calls
                .iter()
                .filter(|c| **c == Call::RequestCloseCheck)
                .count(),
            2,
            "{refusal:?}: the same notary is asked again: {calls:?}"
        );
        assert!(calls.contains(&Call::CloseAgentChain));
    }
}

#[tokio::test]
async fn an_unavailable_notary_is_substituted() {
    for failure in [
        CloseCheckResponse::UnableToVerify,
        CloseCheckResponse::NotAClosingNotary,
    ] {
        let mock = open_chain(
            vec![agent(70), agent(71)],
            1,
            vec![failure.clone(), CloseCheckResponse::Approved],
        );
        let (result, calls) = run_close("unavailable-substituted", &mock).await;
        result.unwrap_or_else(|e| panic!("{failure:?}: {e}"));
        assert!(
            calls.contains(&Call::CloseAgentChain),
            "{failure:?}: {calls:?}"
        );
    }
}

#[tokio::test]
async fn too_few_approvals_backs_off_without_closing() {
    let mock = open_chain(
        vec![agent(70), agent(71)],
        2,
        vec![
            CloseCheckResponse::UnableToVerify,
            CloseCheckResponse::Approved,
        ],
    );
    let (result, calls) = run_close("too-few-approvals", &mock).await;
    let err = result.unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "too few approvals is not a hard stop: {err}"
    );
    assert!(!calls.contains(&Call::CloseAgentChain), "{calls:?}");
}

#[tokio::test]
async fn an_unrecognized_probe_answer_backs_off_without_preparing() {
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(
            "get_migration_close_state zome call failed: Failed to call zome: \
             Websocket error: Websocket closed: No connection"
        )));
    let (result, calls) = run_close("unrecognized-probe", &mock).await;
    let err = result.unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "{err}"
    );
    assert_eq!(
        calls,
        vec![Call::GetMigrationCloseState],
        "nothing past the probe"
    );
}

#[tokio::test]
async fn a_close_to_an_unapproved_target_hard_stops() {
    let mock = open_chain(vec![agent(70)], 1, vec![CloseCheckResponse::Approved]);
    *mock.close_result.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "close_agent_chain zome call failed: [MIGERR:MIG_CLOSE_TARGET_NOT_UPGRADE_TARGET] \
         Close target is not in this DNA's upgrade_targets"
    )));
    let (result, _) = run_close("close-target-hard-stop", &mock).await;
    let err = result.unwrap_err();
    assert!(err.contains("hard-stopped"), "{err}");
}

#[tokio::test]
async fn a_summary_with_no_close_hard_stops_without_closing_again() {
    let tmp = tmp_state("summary-without-close");
    let mock = MockConductor::default();
    for _ in 0..2 {
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!(
                "get_migration_close_state zome call failed: \
             [MIGERR:MIG_NO_CLOSE_CHAIN_ACTION] no CloseChain action found on chain"
            )));
    }
    let mut sd = never_shutdown();
    let err = close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("hard-stopped"), "{err}");
    assert_eq!(
        mock.calls(),
        vec![Call::GetMigrationCloseState],
        "nothing is prepared, checked or closed"
    );
    assert_eq!(State::read(&tmp).unwrap().step, Step::Failed);
    let _ = std::fs::remove_file(&tmp);
}
