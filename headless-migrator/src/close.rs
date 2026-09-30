//! The close service: a supervised loop that takes the old chain from open to
//! closed and exits 0 only once it is closed. Probe-first and idempotent, so a
//! restart (or reboot) re-enters from a fresh probe and never double-closes.
//! Transient failures back off and re-probe (no overall deadline — systemd
//! `Restart=on-failure` owns process death; this loop owns in-process
//! progress).

use std::time::Duration;

use anyhow::{Context, Result};
use holo_hash::{AgentPubKey, AgentPubKeyB64, DnaHash};
use rave_engine::types::entries::migration::v0_2::{
    CloseCheckRequest, CloseCheckResponse, PrepareCloseResponse, SummaryStatePayload,
};

use crate::conductor::Conductor;
use crate::config::Config;
use crate::policy::{self, CheckOutcome, Checker, PolicyError, Sleeper};
use crate::probe::{probe_close_state, CloseState, ProbeFailure};
use crate::state_file::{Phase, State, Step};

/// Outcome of one close attempt, before the supervised loop decides to exit or
/// retry.
enum CloseOutcome {
    /// The chain is closed (now or already). Exit 0.
    Closed,
    /// A fault no retry fixes. Exit nonzero; the operator must act.
    HardStop(String),
    /// A transient failure; back off and re-probe.
    Transient(anyhow::Error),
}

/// Run the close service to completion (or a hard stop). Returns `Ok(())` once
/// the chain is closed; `Err` only on a hard stop the operator must resolve.
pub async fn run(
    conductor: &dyn Conductor,
    cfg: &Config,
    shutdown: &mut ham::ShutdownRx,
) -> Result<()> {
    // The close binds to a configured successor (single-landing). Resolve it up
    // front so a missing/garbled MIGRATION_AGENT_TO_DNA fails the close service
    // immediately (not mid-loop); open/verify/status, which don't set it, are
    // unaffected (validated only here, the way OpenConfig is only by `open`).
    let target: DnaHash = cfg
        .to_dna
        .clone()
        .context("MIGRATION_AGENT_TO_DNA is required for the close service")?
        .into();
    let mut state = State::new(Phase::Close, Step::Probing, "");
    let backoff = cfg.loop_backoff();
    let mut attempts: u32 = 0;
    loop {
        if *shutdown.borrow() {
            return shutdown_before_complete();
        }
        match attempt(conductor, cfg, &target, &mut state).await {
            CloseOutcome::Closed => {
                persist(cfg, &mut state, |s| {
                    s.step = Step::Done;
                    s.old_chain_closed = true;
                    s.message = "old chain closed".into();
                });
                tracing::info!("close service complete: old chain closed");
                return Ok(());
            }
            CloseOutcome::HardStop(why) => {
                persist(cfg, &mut state, |s| {
                    s.step = Step::Failed;
                    s.message = format!("hard stop: {why}");
                });
                anyhow::bail!("close hard-stopped: {why}");
            }
            CloseOutcome::Transient(e) => {
                // Jittered backoff via ham's shared curve (de-synchronizes many
                // agents retrying after the same gossip blip).
                let delay = Duration::from_millis(ham::compute_delay_ms(attempts, &backoff));
                tracing::warn!(error = %format!("{e:#}"), delay_ms = delay.as_millis() as u64,
                    "transient close failure; backing off");
                persist(cfg, &mut state, |s| {
                    s.message = format!("transient failure, retrying: {e:#}");
                });
                if sleep_or_shutdown(delay, shutdown).await {
                    return shutdown_before_complete();
                }
                attempts = attempts.saturating_add(1);
            }
        }
    }
}

/// One probe → act pass.
async fn attempt(
    conductor: &dyn Conductor,
    cfg: &Config,
    target: &DnaHash,
    state: &mut State,
) -> CloseOutcome {
    persist(cfg, state, |s| {
        s.step = Step::Probing;
        s.message = "probing old-chain close state".into();
    });
    let close_state = match probe_close_state(conductor).await {
        Ok(s) => s,
        Err(ProbeFailure::HardStop(why)) => return CloseOutcome::HardStop(why),
        Err(ProbeFailure::Transient(e)) => {
            return CloseOutcome::Transient(e.context("probing close state"))
        }
    };

    match close_state {
        CloseState::Closed(committed) => {
            // The close carries no approvals, so only the agent can be
            // recovered from it.
            let agent_b64 =
                AgentPubKeyB64::from(committed.payload.agent_pubkey.clone()).to_string();
            persist(cfg, state, |s| {
                if s.agent.is_none() {
                    s.agent = Some(agent_b64);
                }
            });
            CloseOutcome::Closed
        }
        CloseState::Open => prepare_check_close(conductor, cfg, target, state).await,
    }
}

async fn prepare_check_close(
    conductor: &dyn Conductor,
    cfg: &Config,
    target: &DnaHash,
    state: &mut State,
) -> CloseOutcome {
    // Fees owed? Drop them FIRST: prepare pins the chain top, and a fee drop
    // after it would void the prepared payload.
    match conductor.get_ledger().await {
        Ok(ledger) => {
            if !ledger.fees_owed.is_zero() {
                persist(cfg, state, |s| {
                    s.step = Step::DroppingFees;
                    s.message = "fees owed — dropping before prepare".into();
                });
                if let Err(e) = conductor.drop_off_fees().await {
                    return CloseOutcome::Transient(e.context("drop_off_fees"));
                }
            }
        }
        Err(e) => return classify_close_read_failure(e, "reading ledger for fee check"),
    }

    persist(cfg, state, |s| {
        s.step = Step::CollectingApprovals;
        s.message = "preparing closing summary".into();
    });
    let prepared: PrepareCloseResponse =
        match conductor.prepare_closing_summary(target.clone()).await {
            Ok(p) => p,
            Err(e) => return classify_close_read_failure(e, "prepare_closing_summary"),
        };
    let agent_b64 = AgentPubKeyB64::from(prepared.payload.agent_pubkey.clone()).to_string();
    persist(cfg, state, |s| {
        s.agent = Some(agent_b64.clone());
        s.approvals_threshold = Some(prepared.closing_threshold);
        s.approvals_collected = Some(0);
        s.message = format!(
            "asking {} of {} notaries to check the close",
            prepared.closing_threshold,
            prepared.closing_notaries.len()
        );
    });

    let checker = ConductorChecker {
        conductor,
        payload: prepared.payload.clone(),
        request_timeout: cfg.policy.request_timeout,
    };
    let sleeper = TokioSleeper;
    let mut rng = rand::thread_rng();
    let approvals = match policy::collect_approvals(
        prepared.closing_threshold,
        &prepared.closing_notaries,
        &cfg.policy,
        &checker,
        &sleeper,
        &mut rng,
    )
    .await
    {
        Ok(approvals) => approvals,
        Err(PolicyError::Warranted) => {
            return CloseOutcome::HardStop("agent carries warrants".into())
        }
        // Exhaustion is NOT a hard stop: more notaries may become reachable.
        // Back off and re-probe; nothing was committed, so the next attempt
        // re-prepares on the fresh chain top.
        Err(e @ PolicyError::Exhausted { .. }) => {
            return CloseOutcome::Transient(anyhow::anyhow!("{e}"))
        }
        Err(PolicyError::Misconfigured(why) | PolicyError::Fatal(why)) => {
            return CloseOutcome::HardStop(why)
        }
    };

    persist(cfg, state, |s| {
        s.step = Step::Closing;
        s.approvals_collected = Some(approvals.len() as u32);
        s.message = "committing close + close_chain".into();
    });
    match conductor.close_agent_chain(prepared.payload).await {
        Ok(_) => CloseOutcome::Closed,
        Err(e) => classify_close_failure(e, "close_agent_chain"),
    }
}

/// Classify a close-side zome-call failure. A target-binding fault (the configured
/// `to_dna` is not in this DNA's `upgrade_targets`) is a HARD stop: retrying can
/// never fix a misconfigured target. Everything else (gossip lag, a websocket
/// blip) stays transient.
fn classify_close_failure(e: anyhow::Error, ctx: &'static str) -> CloseOutcome {
    if crate::dna_errors::is_close_target_hard_failure(&format!("{e:#}")) {
        CloseOutcome::HardStop(format!("{ctx}: {e:#}"))
    } else {
        CloseOutcome::Transient(e.context(ctx))
    }
}

/// [`classify_close_failure`] for a call whose RESPONSE VALUE the close needs.
/// A response that did not decode is a second hard stop here and nowhere else:
/// for a read the state cannot be learned, while for a write it says nothing
/// about whether the write landed, which the next pass's probe reads back.
fn classify_close_read_failure(e: anyhow::Error, ctx: &'static str) -> CloseOutcome {
    let rendered = format!("{e:#}");
    if crate::dna_errors::is_response_decode_failure(&rendered) {
        CloseOutcome::HardStop(crate::dna_errors::schema_mismatch_message(ctx, &rendered))
    } else {
        classify_close_failure(e, ctx)
    }
}

/// Bridges the policy's [`Checker`] to a live `request_close_check` zome call,
/// applying the per-request timeout and mapping the response. `Warranted` is
/// raised through the `Err` channel so the policy hard-stops the whole
/// migration rather than substituting.
struct ConductorChecker<'a> {
    conductor: &'a dyn Conductor,
    payload: SummaryStatePayload,
    request_timeout: Duration,
}

impl Checker for ConductorChecker<'_> {
    async fn check(&self, notary: AgentPubKey) -> std::result::Result<CheckOutcome, PolicyError> {
        let req = CloseCheckRequest {
            notary: notary.clone(),
            payload: self.payload.clone(),
        };
        let call = self.conductor.request_close_check(req);
        match tokio::time::timeout(self.request_timeout, call).await {
            Err(_elapsed) => Ok(CheckOutcome::TimedOut),
            Ok(Err(e)) => {
                let rendered = format!("{e:#}");
                // Not the notary's fault: substituting reports an exhausted
                // N-list for this binary's schema mismatch.
                if crate::dna_errors::is_response_decode_failure(&rendered) {
                    return Err(PolicyError::Fatal(
                        crate::dna_errors::schema_mismatch_message(
                            "request_close_check",
                            &rendered,
                        ),
                    ));
                }
                tracing::warn!(notary = %notary, error = %rendered,
                    "request_close_check errored");
                Ok(CheckOutcome::Errored)
            }
            Ok(Ok(CloseCheckResponse::Approved)) => Ok(CheckOutcome::Approved),
            Ok(Ok(CloseCheckResponse::StateMismatch)) => Ok(CheckOutcome::StateMismatch),
            Ok(Ok(CloseCheckResponse::TargetNotApproved)) => Ok(CheckOutcome::TargetNotApproved),
            Ok(Ok(CloseCheckResponse::UnableToVerify)) => Ok(CheckOutcome::UnableToVerify),
            Ok(Ok(CloseCheckResponse::NotAClosingNotary)) => Ok(CheckOutcome::NotAClosingNotary),
            Ok(Ok(CloseCheckResponse::Warranted(_))) => Err(PolicyError::Warranted),
        }
    }
}

/// Real sleeper for the policy backoff.
struct TokioSleeper;

impl Sleeper for TokioSleeper {
    async fn sleep(&self, dur: Duration) {
        tokio::time::sleep(dur).await;
    }
}

/// Sleep up to `dur`, returning `true` if shutdown fired first.
async fn sleep_or_shutdown(dur: Duration, shutdown: &mut ham::ShutdownRx) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = shutdown.changed() => true,
    }
}

/// Shutdown fired before the chain was closed: the migration is INCOMPLETE, so
/// the service must exit nonzero (not `Ok`). A supervised one-shot exits 0 only
/// on success — exiting 0 here would let systemd's `Restart=on-failure` treat an
/// interrupted (e.g. reboot mid-close) run as done and never resume it. The
/// in-progress step is the operator-must-intervene `Step::Failed`'s opposite —
/// a restart re-probes and resumes — so we deliberately do NOT touch the state
/// file here: leaving the last meaningful record (agent + approval attribution
/// a prior pass wrote) intact rather than clobbering it with this pass's
/// possibly-bare in-memory `State` (the top-of-loop bail can fire before any
/// `attempt` has populated it). The next restart's first probe rewrites it.
fn shutdown_before_complete() -> Result<()> {
    anyhow::bail!("shutdown before close completed (chain still open)")
}

/// Apply `f` to the carried `state` and persist it, swallowing (logging) a
/// write error — a failed status write must never abort the migration itself.
/// Mutating the carried `state` in place (rather than re-stamping a fresh
/// `State::new`) is what makes `agent` / `approvals_*` progress persist across
/// passes and into the final closed record.
fn persist(cfg: &Config, state: &mut State, f: impl FnOnce(&mut State)) {
    f(state);
    if let Err(e) = state.write(&cfg.state_file) {
        tracing::error!(error = %format!("{e:#}"), "failed writing state file");
    }
}
