//! Probe → next-step mapping for EVERY close- and open-side state, including the
//! partial close — the idempotency/resume contract both services depend on.

mod support;

use headless_migrator::conductor::AppPresence;
use headless_migrator::probe::{
    classify_close_error, probe_close_state, probe_closed_status, probe_open_state, CloseNext,
    CloseState, ClosedStatus, OpenNext, OpenState,
};
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;

// ── Close-side states ────────────────────────────────────────────────────

#[tokio::test]
async fn probe_open_chain_maps_to_prepare_collect_close() {
    // `get_migration_close_state` errors "No closing state summary found" → the
    // chain is plainly open → prepare + collect + close.
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("No closing state summary found")));
    let state = probe_close_state(&mock).await.unwrap();
    assert_eq!(state, CloseState::Open);
    assert_eq!(state.next(), CloseNext::PrepareCollectClose);
}

#[tokio::test]
async fn probe_partial_close_maps_to_finish_only() {
    // Summary committed but no CloseChain → `get_migration_close_state` errors
    // "no CloseChain action found on chain" → partial close → finish.
    let mock = MockConductor::default();
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Err(anyhow::anyhow!("no CloseChain action found on chain")));
    let state = probe_close_state(&mock).await.unwrap();
    assert_eq!(state, CloseState::PartialClose);
    assert_eq!(state.next(), CloseNext::FinishCloseOnly);
}

#[tokio::test]
async fn probe_closed_chain_maps_to_already_closed() {
    // A committed close reads back → fully closed → no-op.
    let mock = MockConductor::default();
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 0);
    mock.close_state
        .lock()
        .unwrap()
        .push_back(Ok(committed_close(3, closing)));
    let state = probe_close_state(&mock).await.unwrap();
    assert!(matches!(state, CloseState::Closed(_)));
    assert_eq!(state.next(), CloseNext::AlreadyClosed);
}

#[test]
fn classify_close_error_distinguishes_partial_from_open() {
    assert_eq!(
        classify_close_error("no CloseChain action found on chain"),
        CloseState::PartialClose
    );
    assert_eq!(
        classify_close_error("No closing state summary found"),
        CloseState::Open
    );
    // A transport error is treated as an open chain (the next supervised pass
    // re-probes; prepare/collect/close are idempotent).
    assert_eq!(
        classify_close_error("Websocket closed: ConnectionClosed"),
        CloseState::Open
    );
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
async fn probe_absent_app_maps_to_fetch_install_open() {
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Absent));
    let state = probe_open_state(&mock, "unyt").await.unwrap();
    assert_eq!(state, OpenState::NotInstalled);
    assert_eq!(state.next(), OpenNext::FetchInstallOpen);
}

#[tokio::test]
async fn probe_installed_not_migrated_maps_to_open_only() {
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.verify_migrated.lock().unwrap().push_back(Ok(false));
    let state = probe_open_state(&mock, "unyt").await.unwrap();
    assert_eq!(state, OpenState::InstalledNotMigrated);
    assert_eq!(state.next(), OpenNext::OpenOnly);
}

#[tokio::test]
async fn probe_migrated_maps_to_already_opened() {
    let mock = MockConductor::default();
    mock.presence
        .lock()
        .unwrap()
        .push_back(Ok(AppPresence::Installed));
    mock.verify_migrated.lock().unwrap().push_back(Ok(true));
    let state = probe_open_state(&mock, "unyt").await.unwrap();
    assert_eq!(state, OpenState::Migrated);
    assert_eq!(state.next(), OpenNext::AlreadyOpened);
}
