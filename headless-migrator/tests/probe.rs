//! Probe outcomes for every close- and open-side state: the idempotency and
//! resume contract both services depend on.

mod support;

use headless_migrator::conductor::AppPresence;
use headless_migrator::probe::{
    probe_close_state, probe_closed_status, probe_open_state, CloseState, ClosedStatus, OpenState,
    ProbeFailure,
};
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;

// ── Close-side states ────────────────────────────────────────────────────

#[tokio::test]
async fn probe_open_chain_reads_open() {
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    let state = probe_close_state(&mock).await.unwrap();
    assert_eq!(state, CloseState::Open);
}

#[tokio::test]
async fn a_summary_with_no_close_is_a_hard_stop() {
    for rendered in [
        "[MIGERR:MIG_NO_CLOSE_CHAIN_ACTION] no CloseChain action found on chain",
        "no CloseChain action found on chain",
    ] {
        let mock = MockConductor::default();
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("{rendered}")));
        assert!(
            matches!(
                probe_close_state(&mock).await,
                Err(ProbeFailure::HardStop(why)) if why.contains("second summary")
            ),
            "{rendered}"
        );
    }
}

#[tokio::test]
async fn probe_closed_chain_reads_closed() {
    // A committed close reads back → fully closed → no-op.
    let mock = MockConductor::default();
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));
    let state = probe_close_state(&mock).await.unwrap();
    assert!(matches!(state, CloseState::Closed(_)));
}

#[tokio::test]
async fn an_answer_that_names_no_close_state_is_transient() {
    for rendered in [
        "Websocket closed: ConnectionClosed",
        "[MIGERR:MIG_STALE_CLOSE] not a close state",
    ] {
        let mock = MockConductor::default();
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("{rendered}")));
        assert!(
            matches!(
                probe_close_state(&mock).await,
                Err(ProbeFailure::Transient(_))
            ),
            "{rendered}"
        );
    }
}

#[tokio::test]
async fn an_undecodable_close_state_is_a_hard_stop() {
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!(
            "get_migration_close_state zome call failed: Failed to deserialize response: \
         missing field `close_action`"
        )));
    assert!(matches!(
        probe_close_state(&mock).await,
        Err(ProbeFailure::HardStop(why)) if why.contains("Rebuild the migrator")
    ));
}

// ── Close-side status tri-state (fix 3a) ─────────────────────────────────

#[tokio::test]
async fn closed_status_reads_closed_when_committed() {
    let mock = MockConductor::default();
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));
    assert_eq!(probe_closed_status(&mock).await, ClosedStatus::Closed);
}

#[tokio::test]
async fn closed_status_reads_not_closed_on_recognized_open_response() {
    // A recognized DNA "no summary" / "no CloseChain" response means the conductor
    // was reached and the chain is DEFINITIVELY not closed yet — not unknown.
    for msg in [
        "No closing state summary found",
        "no CloseChain action found on chain",
    ] {
        let mock = MockConductor::default();
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("{msg}")));
        assert_eq!(
            probe_closed_status(&mock).await,
            ClosedStatus::NotClosed,
            "{msg} ⇒ definitively not closed"
        );
    }
}

#[tokio::test]
async fn closed_status_reads_unknown_on_transport_error() {
    // A transport / unexpected error (the conductor unreachable, a timeout) is
    // UNKNOWN — the report must NOT present it as a definitive `not closed`. This
    // is the close-side conflation fix 3 closes: a probe FAILURE ≠ "chain open".
    for msg in [
        "Websocket closed: ConnectionClosed",
        "Connection refused (os error 111)",
        "request timed out",
        "some unrelated host failure",
    ] {
        let mock = MockConductor::default();
        mock.close_state
            .lock()
            .unwrap()
            .push_back(Err(anyhow::anyhow!("{msg}")));
        assert_eq!(
            probe_closed_status(&mock).await,
            ClosedStatus::Unknown,
            "{msg} ⇒ unknown, not a definitive not-closed"
        );
    }
}

// ── Open-side states ─────────────────────────────────────────────────────

#[tokio::test]
async fn probe_absent_app_is_not_installed() {
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Absent));
    let state = probe_open_state(&mock, "unyt").await.unwrap();
    assert_eq!(state, OpenState::NotInstalled);
}

#[tokio::test]
async fn probe_present_app_is_installed_without_a_zome_call() {
    // The pre-install probe is admin-only: it asks presence, NOT
    // verify_if_migrated (which needs ham and drives init). So an installed app
    // maps to `Installed` regardless of migrated state — whether it opened is
    // decided by the ham-connected drive step, not the probe. No verify_migrated
    // response is scripted: if the probe called it, the mock's `pop` would panic.
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    let state = probe_open_state(&mock, "unyt").await.unwrap();
    assert_eq!(state, OpenState::Installed);
}
