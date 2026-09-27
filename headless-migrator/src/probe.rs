//! Chain/app state probing and the next-step mapping that makes both services
//! idempotent and restart-safe. Every service re-probes on (re)start and
//! resumes from the first incomplete step; this module is the pure decision
//! layer, exhaustively unit-tested against the mock conductor.

use anyhow::Result;
use rave_engine::types::entries::migration::v0_2::CommittedClose;

use crate::conductor::{AppPresence, Conductor};
use crate::dna_errors::CloseErrorClass;

/// The close-side state of the old chain, from `get_migration_close_state`.
///
/// `PartialEq` is by variant only: `CommittedClose` is `PartialEq`-free in
/// `rave_engine`, and callers branch on the state, not the payload.
#[derive(Debug, Clone)]
pub enum CloseState {
    /// No `ClosingStateSummary`: prepare, check, close.
    Open,
    /// Summary + `CloseChain`: nothing to do.
    Closed(Box<CommittedClose>),
}

impl PartialEq for CloseState {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (CloseState::Open, CloseState::Open) | (CloseState::Closed(_), CloseState::Closed(_))
        )
    }
}

/// Why the probe could not name a close state.
#[derive(Debug)]
pub enum ProbeFailure {
    /// Stop: a response this binary cannot decode, or a summary with no close.
    HardStop(String),
    /// Back off and probe again: anything the DNA did not answer with a state.
    Transient(anyhow::Error),
}

/// Classify the close state of the old chain WITHOUT writing to it, from
/// `get_migration_close_state`: a close ⇒ `Closed`; `MIG_NO_CLOSING_SUMMARY`
/// ⇒ `Open`; `MIG_NO_CLOSE_CHAIN_ACTION` ⇒ a hard stop.
pub async fn probe_close_state(
    conductor: &dyn Conductor,
) -> std::result::Result<CloseState, ProbeFailure> {
    let e = match conductor.get_migration_close_state().await {
        Ok(close) => return Ok(CloseState::Closed(Box::new(close))),
        Err(e) => e,
    };
    let rendered = format!("{e:#}");
    if crate::dna_errors::is_response_decode_failure(&rendered) {
        return Err(ProbeFailure::HardStop(
            crate::dna_errors::schema_mismatch_message("probing close state", &rendered),
        ));
    }
    match crate::dna_errors::classify_close_error(&rendered) {
        CloseErrorClass::Open => Ok(CloseState::Open),
        CloseErrorClass::SummaryWithoutClose => Err(ProbeFailure::HardStop(
            "the chain holds a closing summary with no close after it. Closing again would \
             commit a second summary, and notaries never attest a chain holding two, so it \
             needs an operator"
                .into(),
        )),
        CloseErrorClass::Unrecognized => Err(ProbeFailure::Transient(e)),
    }
}

/// The close-side answer to the `Status` report's "is the old chain closed?"
/// question — a TRI-STATE, because a status report must not conflate "the
/// conductor says the chain is still open" with "the conductor was unreachable".
/// (The close *service* needs no such distinction: it treats any non-closed
/// result as "open, re-probe", since prepare/collect/close are idempotent. Only
/// the report needs to tell a definitive answer from a missing one.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosedStatus {
    /// A committed close was read back — the old chain is closed.
    Closed,
    /// The conductor was reached and definitively reports no committed close yet
    /// (a recognized "no closing state summary" / "no CloseChain" response,
    /// i.e. a plain-open or partial-close chain).
    NotClosed,
    /// The close state could not be determined — a transport failure, a timeout,
    /// or any unrecognized error. NOT a definitive "not closed"; the report must
    /// surface this as unknown rather than as `old_chain_closed = false`.
    Unknown,
}

/// Probe the old chain's closed-ness for the `Status` report as a [`ClosedStatus`]
/// tri-state. `Ok` ⇒ `Closed`; an error whose text is a recognized close-state
/// response (no summary / no CloseChain) ⇒ `NotClosed`; any other error (transport,
/// timeout, unexpected) ⇒ `Unknown`. The recognized-response contract lives in
/// [`crate::dna_errors::is_recognized_close_state_response`] (one home for the
/// fragile DNA string match).
pub async fn probe_closed_status(conductor: &dyn Conductor) -> ClosedStatus {
    match conductor.get_migration_close_state().await {
        Ok(_) => ClosedStatus::Closed,
        Err(e) => {
            let rendered = format!("{e:#}");
            if crate::dna_errors::is_recognized_close_state_response(&rendered) {
                ClosedStatus::NotClosed
            } else {
                ClosedStatus::Unknown
            }
        }
    }
}

/// The open-side state of the new server — **presence only**. The open
/// service's pre-install probe connects admin-only (there is no app cell for
/// `ham` to attach to yet), so it can ask "is the app installed?" but not run
/// the `verify_if_migrated` zome call. Whether an installed cell has *opened*
/// (its `init` read the migration `init_properties` and committed the
/// `OpeningStateSummary`) is decided by the ham-connected action — calling
/// `verify_if_migrated` there both answers it and DRIVES `init` on a cell that
/// hasn't been called yet. So there is no separate "installed-but-unmigrated"
/// probe state: install + open are one step from the probe's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenState {
    /// No app installed yet — fetch the package, install for the carried key
    /// WITH it as the role's `init_properties`, then drive `init` + verify.
    NotInstalled,
    /// App installed — connect `ham`, drive `init` (`verify_if_migrated`, which
    /// reads the carried `init_properties` and opens the chain on the first
    /// call), then verify. Idempotent if the chain is already open.
    Installed,
}

/// Probe the open-side state — PRESENCE ONLY (admin-only safe). Whether an
/// installed cell has opened is decided by the ham-connected action's
/// `verify_if_migrated` (which drives `init`), not here.
pub async fn probe_open_state(conductor: &dyn Conductor, app_id: &str) -> Result<OpenState> {
    Ok(match conductor.app_presence(app_id).await? {
        AppPresence::Absent => OpenState::NotInstalled,
        AppPresence::Installed => OpenState::Installed,
    })
}
