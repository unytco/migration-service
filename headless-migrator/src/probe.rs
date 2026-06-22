//! Chain/app state probing and the next-step mapping that makes both services
//! idempotent and restart-safe. Every service re-probes on (re)start and
//! resumes from the first incomplete step; this module is the pure decision
//! layer, exhaustively unit-tested against the mock conductor.

use anyhow::Result;
use rave_engine::types::entries::migration::v0_1::CommittedClose;

use crate::conductor::{AppPresence, Conductor};
use crate::dna_errors::CloseErrorClass;

/// The close-side state of the old chain, as the close service must resume from
/// it. Derived from `get_migration_close_state` (which returns the committed
/// package only when BOTH the `ClosingStateSummary` and the `CloseChain` action
/// are present — see the alliance `close.rs`), so its three outcomes map
/// one-to-one onto the resume decision.
///
/// `PartialEq` is by variant only (`CommittedClose` is `PartialEq`-free in
/// `rave_engine`, and the close payload content is not what callers branch on —
/// only which state the chain is in).
#[derive(Debug, Clone)]
pub enum CloseState {
    /// No `ClosingStateSummary` committed — a normal open chain. Resume by
    /// preparing + collecting M-of-N + closing.
    Open,
    /// `ClosingStateSummary` committed but `close_chain` not yet issued (a crash
    /// between the two). Resume by issuing `close_chain` ONLY — never re-prepare
    /// or re-collect over a committed summary.
    PartialClose,
    /// Fully closed: summary + `CloseChain` present. The committed package is
    /// readable. Nothing to do (the close is a no-op on a closed chain).
    Closed(Box<CommittedClose>),
}

impl PartialEq for CloseState {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (CloseState::Open, CloseState::Open)
                | (CloseState::PartialClose, CloseState::PartialClose)
                | (CloseState::Closed(_), CloseState::Closed(_))
        )
    }
}

/// The next action the close service should take, from a [`CloseState`].
#[derive(Debug, Clone, PartialEq)]
pub enum CloseNext {
    /// Prepare the summary, collect M-of-N, then close.
    PrepareCollectClose,
    /// Finish the interrupted close: `close_chain` only.
    FinishCloseOnly,
    /// Already closed — exit 0.
    AlreadyClosed,
}

impl CloseState {
    pub fn next(&self) -> CloseNext {
        match self {
            CloseState::Open => CloseNext::PrepareCollectClose,
            CloseState::PartialClose => CloseNext::FinishCloseOnly,
            CloseState::Closed(_) => CloseNext::AlreadyClosed,
        }
    }
}

/// Classify the close state of the old chain WITHOUT writing to it.
///
/// `get_migration_close_state` distinguishes the three cases by its result:
/// `Ok` ⇒ fully closed; an error whose chain mentions a *missing CloseChain*
/// ⇒ partial close (summary present, chain still open); any other "no closing
/// state summary" error ⇒ a plain open chain. The two error strings are part
/// of the alliance `close.rs` contract (`"No closing state summary found"` and
/// `"no CloseChain action found on chain"`); [`classify_close_error`] isolates
/// that match so it is a single, testable place to update if the DNA reworks
/// its messages.
pub async fn probe_close_state(conductor: &dyn Conductor) -> Result<CloseState> {
    match conductor.get_migration_close_state().await {
        Ok(close) => Ok(CloseState::Closed(Box::new(close))),
        Err(e) => Ok(classify_close_error(&format!("{e:#}"))),
    }
}

/// Map a `get_migration_close_state` error string onto the non-closed close
/// states. The substring contract itself lives in [`crate::dna_errors`] (one
/// home for every fragile DNA error-string match); this only lifts its
/// [`CloseErrorClass`] into the probe's [`CloseState`]. A transport failure
/// classifies as `Open` and simply re-probes on the next supervised pass, which
/// is safe: prepare/collect/close are each idempotent or abort atomically on a
/// stale/duplicate attempt.
pub fn classify_close_error(rendered: &str) -> CloseState {
    match crate::dna_errors::classify_close_error(rendered) {
        CloseErrorClass::PartialClose => CloseState::PartialClose,
        CloseErrorClass::Open => CloseState::Open,
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

/// The open-side state of the new server, as the open service must resume from
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenState {
    /// No app installed yet — fetch the package, install for the carried key,
    /// then migration_init.
    NotInstalled,
    /// App installed but `migration_init` not yet run (no `OpeningStateSummary`)
    /// — run migration_init (as the first zome call).
    InstalledNotMigrated,
    /// App installed and chain already opened (an `OpeningStateSummary` exists)
    /// — nothing to do; open is a no-op on an opened chain.
    Migrated,
}

/// The next action the open service should take, from an [`OpenState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenNext {
    /// Fetch package → install for carried key → migration_init.
    FetchInstallOpen,
    /// App is installed; just run migration_init (first zome call).
    OpenOnly,
    /// Already migrated — exit 0.
    AlreadyOpened,
}

impl OpenState {
    pub fn next(&self) -> OpenNext {
        match self {
            OpenState::NotInstalled => OpenNext::FetchInstallOpen,
            OpenState::InstalledNotMigrated => OpenNext::OpenOnly,
            OpenState::Migrated => OpenNext::AlreadyOpened,
        }
    }
}

/// Probe the open-side state. When the app is present we ask the cell whether it
/// has already migrated (`verify_if_migrated`), distinguishing
/// installed-but-fresh from already-opened.
pub async fn probe_open_state(conductor: &dyn Conductor, app_id: &str) -> Result<OpenState> {
    match conductor.app_presence(app_id).await? {
        AppPresence::Absent => Ok(OpenState::NotInstalled),
        AppPresence::Installed => {
            if conductor.verify_if_migrated().await? {
                Ok(OpenState::Migrated)
            } else {
                Ok(OpenState::InstalledNotMigrated)
            }
        }
    }
}
