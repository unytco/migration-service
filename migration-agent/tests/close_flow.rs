//! Close-service flow against the mock conductor: fees-owed → `drop_off_fees`
//! precedes `prepare_closing_summary`; close is a no-op on an already-closed
//! chain; a warranted notary hard-stops; a happy path drives prepare → collect
//! → close in order. Drives the real `close::run` loop with the injected mock
//! (no live conductor), so the ordering + idempotency contract is proven.

mod support;

use std::time::Duration;

use migration_agent::close;
use migration_agent::config::Config;
use migration_agent::policy::PolicyOpts;
use migration_agent::state_file::{State, Step};
use rave_engine::types::entries::migration::v0_1::SignClosingResponse;
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use zfuel::fuel::ZFuel;

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
    }
}

fn tmp_state(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "migration-agent-test-{}-{}.json",
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
        !calls.contains(&Call::PrepareClosingSummary),
        "never prepares on an already-closed chain: {calls:?}"
    );
    assert!(!calls.contains(&Call::CloseAgentChain), "never re-closes");
    let state = State::read(&tmp).unwrap();
    assert!(state.old_chain_closed);
    assert_eq!(state.step, Step::Done);
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn already_closed_restart_retains_agent_attribution() {
    // Restart onto an already-closed chain: `attempt` returns Closed straight
    // from the probe (no prepare/collect), but the persisted record must still
    // carry the agent + the collected-signature count, recovered from the
    // committed close the probe read — so the report shows attribution after a
    // restart, not the all-None gap. The committed-close fixture carries one
    // notary signature for agent seed 3.
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
    assert!(!calls.contains(&Call::PrepareClosingSummary), "{calls:?}");
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
    assert_eq!(
        state.signatures_collected,
        Some(1),
        "the collected-signature count is recovered from the committed close"
    );
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
    *mock.ledger.lock().unwrap() = Some(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        ZFuel::from(5i64),
    ));
    *mock.drop_fees.lock().unwrap() = Some(Ok("Fees dropped off".into()));
    // Prepare: one notary, threshold 1.
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    // The single notary signs.
    mock.sign_responses
        .lock()
        .unwrap()
        .push_back(Ok(SignClosingResponse::Signed {
            signature: hdi::prelude::Signature([2u8; 64]),
        }));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("close run Ok");

    let calls = mock.calls();
    let drop_idx = calls.iter().position(|c| *c == Call::DropOffFees);
    let prep_idx = calls.iter().position(|c| *c == Call::PrepareClosingSummary);
    assert!(drop_idx.is_some(), "fees were dropped: {calls:?}");
    assert!(prep_idx.is_some(), "summary was prepared: {calls:?}");
    assert!(
        drop_idx < prep_idx,
        "drop_off_fees must precede prepare_closing_summary: {calls:?}"
    );
    assert!(
        calls.contains(&Call::CloseAgentChain),
        "the chain is closed after collection"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn no_fee_drop_when_none_owed() {
    let tmp = tmp_state("no-fee-drop");
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    // Zero fees → no drop.
    *mock.ledger.lock().unwrap() = Some(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        ZFuel::zero(),
    ));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    mock.sign_responses
        .lock()
        .unwrap()
        .push_back(Ok(SignClosingResponse::Signed {
            signature: hdi::prelude::Signature([2u8; 64]),
        }));

    let mut sd = never_shutdown();
    close::run(&mock, &cfg(&tmp), &mut sd)
        .await
        .expect("close run Ok");
    assert!(
        !mock.calls().contains(&Call::DropOffFees),
        "no fee drop when none owed: {:?}",
        mock.calls()
    );
    let _ = std::fs::remove_file(&tmp);
}

#[tokio::test]
async fn closed_state_retains_agent_and_signature_progress() {
    // After a successful close the persisted state must still carry the agent
    // and the signatures_collected/threshold set during collection — the report
    // collector (`make migrate-status`) reads these. A per-call `State::new`
    // would re-stamp them to None on the final write; the carried `State` keeps
    // them.
    let tmp = tmp_state("closed-retains-progress");
    let mock = MockConductor::default();
    // Open chain → prepare with threshold 2 over two notaries, both sign.
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    *mock.ledger.lock().unwrap() = Some(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        ZFuel::zero(),
    ));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(
        7,
        closing,
        vec![agent(70), agent(71)],
        2,
    )));
    for _ in 0..2 {
        mock.sign_responses
            .lock()
            .unwrap()
            .push_back(Ok(SignClosingResponse::Signed {
                signature: hdi::prelude::Signature([2u8; 64]),
            }));
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
        state.signatures_threshold,
        Some(2),
        "the threshold persists into the final closed state"
    );
    assert_eq!(
        state.signatures_collected,
        Some(2),
        "the collected count persists into the final closed state"
    );
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
    *mock.ledger.lock().unwrap() = Some(ledger(
        unit_map(0, 10),
        CarryForwardUnits::new(),
        ZFuel::zero(),
    ));
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    *mock.prepare.lock().unwrap() = Some(Ok(prepare_response(3, closing, vec![agent(70)], 1)));
    // The notary returns Warranted → the whole migration hard-stops.
    mock.sign_responses
        .lock()
        .unwrap()
        .push_back(Ok(SignClosingResponse::Warranted(vec![])));

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
