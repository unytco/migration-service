//! The `Status` command: a one-shot probe + report of the migration questions —
//! is the old chain closed? is the package fetchable? is the new chain opened?
//! and is it safe to tear down the old server? — written to the state file and
//! printed. Read-only; never mutates the chain. Which probes apply depends on
//! which side this agent runs on, so each is attempted independently and a probe
//! that can't run (e.g. no app installed yet) reports `false` rather than
//! failing the whole report.
//!
//! `safe_to_teardown` carries the close+open+**verify** invariant the open
//! service enforces — but `status` **reports the PERSISTED signal**, it does not
//! re-run a live verify. Verify reads the OLD-side close via the router, so it
//! can only run *during* the migration while the old side is up; the open
//! service persists `safe_to_teardown = true` once, at verify success. A later
//! `status` (run after the operator tears the old side down) reads that
//! persisted value back — monotonic: once true, it stays true. Re-running a live
//! verify here would flip the signal false the moment the old side is gone,
//! exactly when an operator consults it to decide teardown is safe.

use anyhow::Result;
use holo_hash::DnaHashB64;

use crate::conductor::HamConductor;
use crate::config::Config;
use crate::fetch::{self, FetchOutcome};
use crate::probe::{probe_closed_status, ClosedStatus};
use crate::state_file::{Phase, State, Step};

/// How long the one-shot `Status` connect is allowed to take before degrading to
/// the documented `false` report. `HamConductor::connect` retries forever (until
/// shutdown) via `ham::connect_with_backoff`, which would hang the report
/// collector on a down / old-side conductor; `status` bounds it so an
/// unreachable conductor reports `false` quickly instead of looping. Overridable
/// via `MIGRATION_AGENT_STATUS_CONNECT_BUDGET_MS` (the report collector can
/// shorten it; tests use a tiny value to prove the bound without a real wait).
fn status_connect_budget() -> std::time::Duration {
    std::env::var("MIGRATION_AGENT_STATUS_CONNECT_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or_else(|| std::time::Duration::from_secs(10))
}

/// Optional router coordinates so `Status` can also report package fetchability
/// (the open side). Omitted on the close side.
pub struct StatusParams {
    pub router_url: String,
    pub from_dna: DnaHashB64,
    pub to_dna: DnaHashB64,
    pub agent_b64: String,
}

/// The teardown gate, isolated as a pure predicate so the close+open+**verify**
/// invariant has one testable home. All three legs must be green — an opened
/// chain whose verify has not passed (`verify_ok == false`) is never safe to
/// tear down. `status` supplies `verify_ok` from the PERSISTED `safe_to_teardown`
/// signal (written by the open service at verify success), never a live verify.
pub fn safe_to_teardown(old_chain_closed: bool, new_chain_opened: bool, verify_ok: bool) -> bool {
    old_chain_closed && new_chain_opened && verify_ok
}

/// Fold a close-side [`ClosedStatus`] tri-state into the report's `old_chain_closed`
/// bool plus its `old_chain_closed_unknown` companion: `Closed` ⇒ `true`;
/// `NotClosed` ⇒ a DEFINITIVE `false`; `Unknown` ⇒ `false` flagged unknown (the
/// conductor couldn't be reached), so a reader never mistakes "couldn't check" for
/// "checked, still open".
pub fn apply_closed_status(state: &mut State, status: ClosedStatus) {
    match status {
        ClosedStatus::Closed => {
            state.old_chain_closed = true;
            state.old_chain_closed_unknown = false;
        }
        ClosedStatus::NotClosed => {
            state.old_chain_closed = false;
            state.old_chain_closed_unknown = false;
        }
        ClosedStatus::Unknown => {
            state.old_chain_closed = false;
            state.old_chain_closed_unknown = true;
            tracing::warn!("old-chain close state UNKNOWN (conductor unreachable / errored)");
        }
    }
}

/// The `new_chain_opened ⇒ old_chain_closed` derivation, scoped to the NEW-server
/// context (fix 3b). On the new server the old cell (on the old DNA) is not
/// probeable, so the direct old-chain probe reads `NotClosed` even though the old
/// chain MUST be closed — a chain cannot open without its predecessor's close — so
/// an open new chain implies the old one is closed. On the CLOSE side there is no
/// router/new-DNA context (`new_server == false`): `old_chain_closed` is reported
/// from the old-chain probe alone and is NEVER derived from an opened-summary read,
/// which on the close side could be a STALE `OpeningStateSummary` left by the old
/// DNA's own prior migration — deriving from it would force `old_chain_closed = true`
/// while the close is still in progress. A no-op unless both conditions hold.
pub fn derive_old_chain_closed_if_new_server(state: &mut State, new_server: bool) {
    if new_server && state.new_chain_opened {
        state.old_chain_closed = true;
        state.old_chain_closed_unknown = false;
    }
}

/// Probe and report. Returns the assembled [`State`] (also written + logged).
pub async fn run(cfg: &Config, params: Option<&StatusParams>) -> Result<State> {
    // Read the prior persisted record ONCE, BEFORE we overwrite the state file
    // with this status record (one read, not two — `persisted_safe_to_teardown`
    // used to re-read internally on top of this). The open service wrote
    // `safe_to_teardown` (and the verify report) at verify success; `status`
    // reports the authoritative teardown signal back rather than re-running a live
    // verify (which needs the old side and so can't run after teardown). Both
    // default to false / None if the file is absent — a standalone status that has
    // never verified.
    let prior = State::read(&cfg.state_file).ok();
    let persisted_teardown = prior.as_ref().map(|s| s.safe_to_teardown).unwrap_or(false);
    let prior_verify = prior.and_then(|s| s.verify);

    // The router coordinates (`params`) are present ONLY in the new-server
    // context — they are what `migrate-fleet.sh` passes to a new droplet's status,
    // and the close side runs with none. That distinction scopes the probes below:
    // the `new_chain_opened ⇒ old_chain_closed` derivation and the new-side open
    // probe apply ONLY on the new server, never on the close side (where an old
    // DNA that itself arrived via a PRIOR migration would otherwise read as
    // "opened" and falsely derive `old_chain_closed = true` mid-close).
    let new_server_context = params.is_some();

    let mut state = State::new(Phase::Status, Step::Probing, "probing migration status");
    if let Some(p) = params {
        state.agent = Some(p.agent_b64.clone());
    }
    state.verify = prior_verify;

    // Old-chain + new-chain probes: need the app cell. Admin-only can't make
    // zome calls, so we attempt a full ham connect — but BOUNDED, since
    // `HamConductor::connect` retries forever until shutdown and would hang the
    // report collector on a down conductor. A timeout (or no app cell) degrades
    // to the documented `false` report via the admin-only presence fallback.
    let mut shutdown = ham::install_shutdown_handler();
    let budget = status_connect_budget();
    let connected =
        match tokio::time::timeout(budget, HamConductor::connect(cfg, &mut shutdown)).await {
            Ok(c) => c,
            Err(_elapsed) => {
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "status conductor connect timed out; reporting from the probes that could run"
                );
                None
            }
        };
    match connected {
        Some(conductor) => {
            // Old-chain probe — TRI-STATE, so an unreachable / errored conductor
            // reads UNKNOWN, never a definitive "not closed". On the close side
            // this is the authoritative answer; on the new server the cell is on
            // the new DNA (no committed close), so it reads `NotClosed` and the
            // derivation below supplies the implied-true.
            apply_closed_status(&mut state, probe_closed_status(&conductor).await);

            // New-chain probe — ONLY in the new-server context. On the close side
            // the conductor is the OLD conductor; if its old DNA itself arrived via
            // a prior migration, `verify_if_migrated` returns true and would
            // falsely flip `new_chain_opened` (and, via the derivation, force
            // `old_chain_closed = true` mid-close). So it never runs close-side.
            if new_server_context {
                match crate::open::probe_for_status(&conductor, &cfg.app_id).await {
                    Ok(opened) => state.new_chain_opened = opened,
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "new-chain probe failed"),
                }
            }
        }
        None => {
            // No reachable app cell (timed out, or no app yet). The old-chain probe
            // could not run → UNKNOWN on the close side. On the new server, fall
            // back to a bounded admin-only presence probe for the new-chain
            // question (the open side before / after install).
            if !new_server_context {
                state.old_chain_closed_unknown = true;
            }
            if new_server_context {
                if let Ok(Ok(admin)) =
                    tokio::time::timeout(budget, HamConductor::connect_admin_only(cfg)).await
                {
                    if let Ok(opened) = crate::open::probe_for_status(&admin, &cfg.app_id).await {
                        state.new_chain_opened = opened;
                    }
                }
            }
        }
    }

    derive_old_chain_closed_if_new_server(&mut state, new_server_context);

    // Package fetchability (open side, if router coordinates were given) — ONE
    // probe with ONE client (`status` no longer fetches for a live verify, so
    // this is the single fetchability probe). A fetch failure here is just "not
    // fetchable", never fatal to the report.
    if let Some(p) = params {
        let client = fetch::http_client_for_status()?;
        match fetch::fetch_package(&client, &p.router_url, &p.from_dna, &p.to_dna, &p.agent_b64)
            .await
        {
            FetchOutcome::Package(_) => state.package_fetchable = true,
            FetchOutcome::KeepWaiting(why) => {
                tracing::info!("package not yet fetchable: {why}");
            }
            FetchOutcome::HardStop(why) => {
                tracing::warn!("package fetch hard stop: {why}");
            }
        }
    }

    // The teardown decision is the PERSISTED, monotonic signal — not a live
    // recomputation. The open service writes `safe_to_teardown = true` only after
    // it reaches Done, which it can only do once close + open + VERIFY are all
    // green (the same invariant [`safe_to_teardown`] encodes); `status` reports
    // that persisted bit, so once true it stays true even after the old side is
    // gone (a live verify can't run then), and stays false until the open service
    // has actually verified.
    state.safe_to_teardown = persisted_teardown;
    state.step = if state.safe_to_teardown {
        Step::Done
    } else {
        Step::Probing
    };
    // Render `old_chain_closed` as the human-readable tri-state — `unknown` when
    // the close-side probe couldn't reach the conductor, not a misleading `false`.
    let old_chain_closed_str = if state.old_chain_closed_unknown {
        "unknown".to_string()
    } else {
        state.old_chain_closed.to_string()
    };
    state.message = format!(
        "old_chain_closed={} package_fetchable={} new_chain_opened={} safe_to_teardown={}",
        old_chain_closed_str,
        state.package_fetchable,
        state.new_chain_opened,
        state.safe_to_teardown
    );

    state.write(&cfg.state_file)?;
    tracing::info!(
        old_chain_closed = %old_chain_closed_str,
        package_fetchable = state.package_fetchable,
        new_chain_opened = state.new_chain_opened,
        safe_to_teardown = state.safe_to_teardown,
        "migration status"
    );
    Ok(state)
}
