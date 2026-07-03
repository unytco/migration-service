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
//!
//! `new_chain_opened` is likewise the PERSISTED signal (stamped by the open
//! service the moment `init` has driven the open), gated on a bounded
//! admin-only presence probe — never a live zome call. On a cell installed with
//! migration `init_properties` the FIRST zome call drives `init` and opens the
//! chain, so "probing" the new server via any zome call would perform the open
//! outside the supervised open service's bounded/verified flow. A new-server
//! `status` therefore makes no zome call at all; only the close side probes its
//! (long-inited) old cell live.

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

/// Reconcile `old_chain_closed` with the (already-resolved) `safe_to_teardown`
/// latch so the report can never render a self-contradictory row. The two are
/// derived independently — `safe_to_teardown` from the persisted monotonic latch,
/// `old_chain_closed[_unknown]` from a live probe — so a close-side `status` run
/// whose conductor is down (probe ⇒ UNKNOWN) but whose latch was persisted `true`
/// by an earlier verified open would otherwise print
/// `old_chain_closed=unknown safe_to_teardown=true`, which is impossible:
/// teardown-safe REQUIRES the old chain to have closed (the open service only
/// latches `true` once close + open + verify are all green). So a latched
/// `safe_to_teardown` resolves the old-chain question to a definitive closed,
/// clearing the `unknown` flag. Pure + presentation-only — it tightens the
/// rendered facts, not the latch/decision logic. A no-op unless the latch is up.
pub fn reconcile_old_chain_closed_with_teardown(state: &mut State) {
    if state.safe_to_teardown {
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
    // The persisted open signal feeds the read-only new-chain probe below. The
    // teardown latch subsumes it (it only latches once close + open + verify are
    // all green), so a record carrying only the latch still reports opened.
    let persisted_new_chain_opened = prior
        .as_ref()
        .map(|s| s.new_chain_opened || s.safe_to_teardown)
        .unwrap_or(false);
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

    let budget = status_connect_budget();
    if new_server_context {
        // NEW-server status makes NO zome call at all: on a cell installed with
        // migration `init_properties` the FIRST zome call — any zome call, the
        // old-chain probe's `get_migration_close_state` included — drives `init`
        // and opens the chain, which only the supervised open service may do
        // (bounded + verified). So the new-chain question is answered read-only
        // by a BOUNDED admin-only presence probe reporting the open service's
        // persisted signal ([`crate::open::probe_for_status`]), and the
        // old-chain question is not probed here either: the new cell holds no
        // committed close (the probe could only ever read `NotClosed`), and the
        // derivation below supplies the implied-true once the chain is open.
        match tokio::time::timeout(budget, HamConductor::connect_admin_only(cfg)).await {
            Ok(Ok(admin)) => {
                match crate::open::probe_for_status(&admin, &cfg.app_id, persisted_new_chain_opened)
                    .await
                {
                    Ok(opened) => state.new_chain_opened = opened,
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "new-chain probe failed"),
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"),
                    "admin connect for the new-chain probe failed; reporting defaults");
            }
            Err(_elapsed) => {
                tracing::warn!(
                    budget_ms = budget.as_millis() as u64,
                    "status admin connect timed out; reporting defaults"
                );
            }
        }
    } else {
        // CLOSE-side status: the old-chain probe is a zome call on the OLD cell
        // (safe — that chain's `init` ran long ago), which needs the full ham
        // connect. BOUNDED, since `HamConductor::connect` retries forever until
        // shutdown and would hang the report collector on a down conductor; a
        // timeout reads UNKNOWN, never a definitive "not closed".
        let mut shutdown = ham::install_shutdown_handler();
        let connect = HamConductor::connect(cfg, &mut shutdown);
        let connected = match tokio::time::timeout(budget, connect).await {
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
            // Old-chain probe — TRI-STATE, so an unreachable / errored conductor
            // reads UNKNOWN, never a definitive "not closed". Close-side this is
            // the authoritative answer. The new-chain probe never runs here: the
            // conductor is the OLD conductor, and if its old DNA itself arrived
            // via a prior migration a live "migrated?" read would return true and
            // falsely flip `new_chain_opened` (and, via the derivation, force
            // `old_chain_closed = true` mid-close).
            Some(conductor) => {
                apply_closed_status(&mut state, probe_closed_status(&conductor).await)
            }
            None => state.old_chain_closed_unknown = true,
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
    // Reconcile the independently-derived fields BEFORE rendering: a latched
    // `safe_to_teardown` implies the old chain closed, so the row can never show
    // `old_chain_closed=unknown` alongside `safe_to_teardown=true` (B46).
    reconcile_old_chain_closed_with_teardown(&mut state);
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
