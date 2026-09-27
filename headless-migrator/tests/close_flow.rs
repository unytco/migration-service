//! Close-service flow against the mock conductor: fees-owed → `drop_off_fees`
//! precedes `prepare_closing_summary`; close is a no-op on an already-closed
//! chain; a warranted notary hard-stops; a happy path drives prepare → M notary
//! checks → close in order. Drives the real `close::run` loop with the injected
//! mock (no live conductor), so the ordering + idempotency contract is proven.

mod support;

use std::time::Duration;

use headless_migrator::close;
use headless_migrator::config::Config;
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{Phase, State, Step};
use holo_hash::AgentPubKey;
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

/// A shutdown receiver whose sender is already gone, so the loop's first
/// backoff ends the run: a test reaching it proves the failure was transient.
fn never_shutdown() -> ham::ShutdownRx {
    let (_tx, rx) = tokio::sync::watch::channel(false);
    rx
}

const OPEN: &str = "[MIGERR:MIG_NO_CLOSING_SUMMARY] No closing state summary found";

/// An open chain owing `fees_owed`, prepared for agent seed 3 over `notaries`
/// with threshold `m`, whose notaries answer the check with `checks` in order.
fn open_chain_owing(
    fees_owed: UnitMap,
    notaries: Vec<AgentPubKey>,
    m: u32,
    checks: Vec<CloseCheckResponse>,
) -> MockConductor {
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(OPEN)));
    *mock.ledger.lock().unwrap() = Some(Ok(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        fees_owed,
    )));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.prepare
        .lock()
        .unwrap()
        .push_back(Ok(prepare_response(3, closing, notaries, m)));
    for check in checks {
        mock.check_responses.lock().unwrap().push_back(Ok(check));
    }
    mock
}

fn open_chain(
    notaries: Vec<AgentPubKey>,
    m: u32,
    checks: Vec<CloseCheckResponse>,
) -> MockConductor {
    open_chain_owing(UnitMap::new(), notaries, m, checks)
}

struct Run {
    result: Result<(), String>,
    calls: Vec<Call>,
    state: Option<State>,
}

async fn run_close(name: &str, mock: &MockConductor) -> Run {
    let tmp = tmp_state(name);
    let mut sd = never_shutdown();
    let result = close::run(mock, &cfg(&tmp), &mut sd)
        .await
        .map_err(|e| format!("{e:#}"));
    let state = State::read(&tmp).ok();
    let _ = std::fs::remove_file(&tmp);
    Run {
        result,
        calls: mock.calls(),
        state,
    }
}

fn agent_b64(seed: u8) -> String {
    holo_hash::AgentPubKeyB64::from(agent(seed)).to_string()
}

fn position(calls: &[Call], wanted: impl Fn(&Call) -> bool) -> Option<usize> {
    calls.iter().position(wanted)
}

fn is_prepare(c: &Call) -> bool {
    matches!(c, Call::PrepareClosingSummary { .. })
}

/// Drive one close over an open chain owing `fees_owed`, returning the calls
/// the conductor saw. Everything but the fee gate is held fixed.
async fn close_with_fees_owed(name: &str, fees_owed: UnitMap) -> Vec<Call> {
    let mock = open_chain_owing(
        fees_owed,
        vec![agent(70)],
        1,
        vec![CloseCheckResponse::Approved],
    );
    let run = run_close(name, &mock).await;
    run.result.expect("the chain closes");
    run.calls
}

#[tokio::test]
async fn no_op_on_already_closed_chain_retains_agent_attribution() {
    let mock = MockConductor::default();
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));

    let run = run_close("closed-noop", &mock).await;
    run.result.expect("closed-chain run is Ok");
    assert_eq!(
        run.calls,
        vec![Call::GetMigrationCloseState],
        "never prepares, checks or closes an already-closed chain"
    );
    let state = run.state.unwrap();
    assert!(state.old_chain_closed);
    assert_eq!(state.step, Step::Done);
    assert_eq!(state.agent.as_deref(), Some(agent_b64(3).as_str()));
    assert_eq!(state.approvals_collected, None);
    assert_eq!(state.approvals_threshold, None);
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
    // A target not in the source GD's upgrade_targets makes
    // `prepare_closing_summary` error with the rejection string; the close
    // service must HARD-STOP, not loop forever.
    let mock = open_chain(vec![agent(70)], 1, vec![]);
    *mock.prepare.lock().unwrap() = [Err(anyhow::anyhow!(
        "prepare_closing_summary zome call failed: target DNA DnaHash(uhC0k) \
         is not in this network's upgrade_targets"
    ))]
    .into();
    let err = run_close("bad-to-dna", &mock).await.result.unwrap_err();
    assert!(
        err.contains("hard-stopped") && err.contains("upgrade_targets"),
        "a misconfigured target must hard-stop, not loop: {err}"
    );
}

#[tokio::test]
async fn fees_owed_drops_before_prepare() {
    let calls = close_with_fees_owed("fees-before-prepare", unit_map(0, 5)).await;
    let drop_idx = position(&calls, |c| *c == Call::DropOffFees);
    let prep_idx = position(&calls, is_prepare);
    assert!(drop_idx.is_some(), "fees were dropped: {calls:?}");
    assert!(prep_idx.is_some(), "summary was prepared: {calls:?}");
    assert!(
        drop_idx < prep_idx,
        "drop_off_fees must precede prepare_closing_summary: {calls:?}"
    );
    assert!(
        matches!(&calls[prep_idx.unwrap()], Call::PrepareClosingSummary { target } if *target == dna(2)),
        "prepare_closing_summary must bind to the configured to_dna dna(2): {calls:?}"
    );
    assert!(calls.contains(&Call::CloseAgentChain), "{calls:?}");
}

#[tokio::test]
async fn fees_owed_on_a_non_base_unit_also_drops_before_prepare() {
    // Under a single `ZFuel` a service-unit debt was unrepresentable, so a chain
    // owing only those would have prepared its summary with the fees still
    // outstanding.
    let calls = close_with_fees_owed("fees-non-base-unit", unit_map(3, 5)).await;
    let drop_idx = position(&calls, |c| *c == Call::DropOffFees);
    let prep_idx = position(&calls, is_prepare);
    assert!(drop_idx.is_some(), "the debt must be dropped: {calls:?}");
    assert!(
        drop_idx < prep_idx,
        "drop_off_fees must precede prepare_closing_summary: {calls:?}"
    );
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
async fn undecodable_ledger_hard_stops_instead_of_looping() {
    let mock = open_chain(vec![agent(70)], 1, vec![]);
    // A second probe, so a regression that loops fails on the assertion below
    // rather than on the mock running out of script.
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(OPEN)));
    *mock.ledger.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "get_ledger zome call failed: Failed to deserialize response: \
         invalid type: string \"5\", expected a map"
    )));

    let run = run_close("undecodable-ledger", &mock).await;
    let err = run.result.unwrap_err();
    assert!(
        err.contains("hard-stopped") && err.contains("reading ledger for fee check"),
        "a schema mismatch must hard-stop, not loop: {err}"
    );
    assert!(
        err.contains("Rebuild the migrator"),
        "the hard stop names the only remedy: {err}"
    );
    assert!(
        position(&run.calls, is_prepare).is_none(),
        "nothing is prepared once the ledger cannot be read: {:?}",
        run.calls
    );
}

#[tokio::test]
async fn a_transient_ledger_failure_still_retries() {
    // Only a DECODE failure is terminal. A websocket blip must keep backing
    // off, or an ordinary hiccup mid-window aborts the close.
    let mock = open_chain(vec![agent(70)], 1, vec![]);
    *mock.ledger.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "get_ledger zome call failed: Failed to call zome: Websocket error: \
         Websocket closed: No connection"
    )));
    let err = run_close("transient-ledger", &mock)
        .await
        .result
        .unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "a transport blip must back off, not hard-stop: {err}"
    );
}

#[tokio::test]
async fn undecodable_close_state_hard_stops_instead_of_looping() {
    // A probe response that will not decode leaves the chain's real state
    // unknowable, so it must not be driven at.
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
    let run = run_close("undecodable-close-state", &mock).await;
    let err = run.result.unwrap_err();
    assert!(
        err.contains("hard-stopped") && err.contains("probing close state"),
        "an undecodable close state must hard-stop, not loop: {err}"
    );
    assert_eq!(run.calls, vec![Call::GetMigrationCloseState]);
}

#[tokio::test]
async fn undecodable_check_response_hard_stops_without_blaming_the_notaries() {
    // Reachable when `PrepareCloseResponse` decodes and `CloseCheckResponse`
    // does not, e.g. upgraded notaries answering with a variant this binary does
    // not know. Collapsed into a per-notary error it would substitute across the
    // whole list and report an exhausted N-list.
    let mock = open_chain(vec![agent(70), agent(71)], 1, vec![]);
    for _ in 0..2 {
        mock.check_responses
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!(
                "request_close_check zome call failed: Failed to deserialize \
                 response: unknown variant `Deferred`"
            )));
    }
    let run = run_close("undecodable-check", &mock).await;
    let err = run.result.unwrap_err();
    assert!(
        err.contains("hard-stopped") && err.contains("Rebuild the migrator"),
        "a schema mismatch must hard-stop with its remedy: {err}"
    );
    assert!(
        !err.contains("notary list exhausted"),
        "the notaries must not be blamed for this binary's schema mismatch: {err}"
    );
    assert_eq!(
        run.calls
            .iter()
            .filter(|c| **c == Call::RequestCloseCheck)
            .count(),
        1,
        "no substitution: {:?}",
        run.calls
    );
}

#[tokio::test]
async fn an_undecodable_write_response_still_retries() {
    // A WRITE whose response did not decode says nothing about whether the write
    // landed, and the next pass's probe reads that back.
    let mock = open_chain(vec![agent(70)], 1, vec![CloseCheckResponse::Approved]);
    *mock.close_result.lock().unwrap() = Some(Err(anyhow::anyhow!(
        "close_agent_chain zome call failed: Failed to deserialize response: \
         invalid length 32, expected 39"
    )));
    let err = run_close("undecodable-write", &mock)
        .await
        .result
        .unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "an undecodable write response must re-probe, not hard-stop: {err}"
    );
}

#[tokio::test]
async fn closed_state_retains_agent_and_approval_progress() {
    // The report collector (`make migrate-status`) reads the agent and the
    // approval counts from the final closed record.
    let mock = open_chain(
        vec![agent(70), agent(71)],
        2,
        vec![CloseCheckResponse::Approved, CloseCheckResponse::Approved],
    );
    let run = run_close("closed-retains-progress", &mock).await;
    run.result.expect("close run Ok");
    let state = run.state.unwrap();
    assert_eq!(state.step, Step::Done);
    assert!(state.old_chain_closed);
    assert_eq!(state.agent.as_deref(), Some(agent_b64(3).as_str()));
    assert_eq!(state.approvals_threshold, Some(2));
    assert_eq!(state.approvals_collected, Some(2));
}

#[tokio::test]
async fn shutdown_before_close_exits_nonzero_and_preserves_prior_report() {
    // A shutdown fired before the chain is closed exits nonzero, so systemd's
    // `Restart=on-failure` resumes the loop, and the bail leaves the report a
    // prior pass wrote untouched.
    let tmp = tmp_state("shutdown-before-close");
    let mock = MockConductor::default();

    let mut prior = State::new(Phase::Close, Step::CollectingApprovals, "collecting");
    prior.agent = Some("uhCAk-prior-agent".into());
    prior.approvals_collected = Some(2);
    prior.approvals_threshold = Some(3);
    prior.write(&tmp).unwrap();

    let (tx, rx) = tokio::sync::watch::channel(false);
    tx.send(true).unwrap();
    let mut sd = rx;

    let result = close::run(&mock, &cfg(&tmp), &mut sd).await;
    assert!(result.is_err());
    assert!(mock.calls().is_empty(), "{:?}", mock.calls());

    let state = State::read(&tmp).unwrap();
    assert_eq!(
        state.step,
        Step::CollectingApprovals,
        "prior step preserved"
    );
    assert_eq!(state.agent.as_deref(), Some("uhCAk-prior-agent"));
    assert_eq!(state.approvals_collected, Some(2));
    assert!(!state.old_chain_closed);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn warranted_notary_hard_stops_the_close() {
    let mock = open_chain(
        vec![agent(70)],
        1,
        vec![CloseCheckResponse::Warranted(vec![])],
    );
    let run = run_close("warranted-hardstop", &mock).await;
    assert!(run.result.is_err(), "warranted must hard-stop");
    assert!(!run.calls.contains(&Call::CloseAgentChain));
    assert_eq!(run.state.unwrap().step, Step::Failed);
}

#[tokio::test]
async fn m_notaries_check_the_prepared_payload_before_the_close() {
    let notaries = vec![agent(70), agent(71), agent(72)];
    let mock = open_chain(
        notaries.clone(),
        2,
        vec![CloseCheckResponse::Approved, CloseCheckResponse::Approved],
    );
    let run = run_close("m-approve-then-close", &mock).await;
    run.result.expect("the chain closes");
    let prepared = payload(
        3,
        summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0),
    );

    let checks = mock.checks.lock().unwrap().clone();
    assert_eq!(checks.len(), 2, "exactly M notaries asked");
    assert_ne!(checks[0].notary, checks[1].notary);
    for check in &checks {
        assert!(notaries.contains(&check.notary));
        assert_eq!(check.payload, prepared);
    }
    assert_eq!(
        *mock.closed_with.lock().unwrap(),
        vec![prepared],
        "the close commits the payload the notaries checked"
    );
    let close = position(&run.calls, |c| *c == Call::CloseAgentChain).unwrap();
    assert!(run
        .calls
        .iter()
        .enumerate()
        .all(|(i, c)| *c != Call::RequestCloseCheck || i < close));
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
        let run = run_close("refusal-then-approve", &mock).await;
        run.result.unwrap_or_else(|e| panic!("{refusal:?}: {e}"));
        let asked: Vec<_> = mock
            .checks
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.notary.clone())
            .collect();
        assert_eq!(asked, vec![agent(70), agent(70)], "{refusal:?}");
        assert!(run.calls.contains(&Call::CloseAgentChain));
    }
}

#[tokio::test]
async fn an_unavailable_notary_is_substituted() {
    for failure in [
        Ok(CloseCheckResponse::UnableToVerify),
        Ok(CloseCheckResponse::NotAClosingNotary),
        Err("request_close_check zome call failed: Failed to call zome: notary call failed"),
    ] {
        let mock = open_chain(vec![agent(70), agent(71)], 1, vec![]);
        mock.check_responses
            .lock()
            .unwrap()
            .push_back(match &failure {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            });
        mock.check_responses
            .lock()
            .unwrap()
            .push_back(Ok(CloseCheckResponse::Approved));
        let run = run_close("unavailable-substituted", &mock).await;
        run.result.unwrap_or_else(|e| panic!("{failure:?}: {e}"));
        let asked: Vec<_> = mock
            .checks
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.notary.clone())
            .collect();
        assert_eq!(asked.len(), 2, "{failure:?}");
        assert_ne!(
            asked[0], asked[1],
            "{failure:?}: a substitute, not the same notary"
        );
        assert!(run.calls.contains(&Call::CloseAgentChain), "{failure:?}");
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
    let run = run_close("too-few-approvals", &mock).await;
    let err = run.result.unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "too few approvals is not a hard stop: {err}"
    );
    assert!(!run.calls.contains(&Call::CloseAgentChain));
}

#[tokio::test]
async fn after_too_few_approvals_the_next_pass_prepares_afresh_and_closes() {
    let tmp = tmp_state("second-pass");
    let mock = open_chain(
        vec![agent(70)],
        1,
        vec![
            CloseCheckResponse::UnableToVerify,
            CloseCheckResponse::Approved,
        ],
    );
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(OPEN)));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.prepare
        .lock()
        .unwrap()
        .push_back(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    // Held, so the first pass's backoff waits rather than ending the run.
    let (_tx, mut rx) = tokio::sync::watch::channel(false);
    close::run(&mock, &cfg(&tmp), &mut rx)
        .await
        .expect("the second pass closes the chain");
    let calls = mock.calls();
    assert_eq!(
        calls.iter().filter(|c| is_prepare(c)).count(),
        2,
        "{calls:?}"
    );
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == Call::CloseAgentChain)
            .count(),
        1
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn a_threshold_no_check_can_meet_hard_stops() {
    for (m, notaries) in [(0, vec![agent(70)]), (2, vec![agent(70), agent(70)])] {
        let mock = open_chain(notaries, m, vec![]);
        let run = run_close("impossible-threshold", &mock).await;
        let err = run.result.unwrap_err();
        assert!(err.contains("hard-stopped"), "M={m}: {err}");
        assert!(mock.checks.lock().unwrap().is_empty(), "M={m}");
        assert!(!run.calls.contains(&Call::CloseAgentChain), "M={m}");
    }
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
    let run = run_close("unrecognized-probe", &mock).await;
    let err = run.result.unwrap_err();
    assert!(
        err.contains("shutdown before close completed") && !err.contains("hard-stopped"),
        "{err}"
    );
    assert_eq!(
        run.calls,
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
    let err = run_close("close-target-hard-stop", &mock)
        .await
        .result
        .unwrap_err();
    assert!(err.contains("hard-stopped"), "{err}");
}

#[tokio::test]
async fn a_summary_with_no_close_hard_stops_without_closing_again() {
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
    let run = run_close("summary-without-close", &mock).await;
    let err = run.result.unwrap_err();
    assert!(err.contains("hard-stopped"), "{err}");
    assert_eq!(
        run.calls,
        vec![Call::GetMigrationCloseState],
        "nothing is prepared, checked or closed"
    );
    assert_eq!(run.state.unwrap().step, Step::Failed);
}
