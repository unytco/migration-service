//! The close service: a supervised loop that takes the old chain from open to
//! closed and exits 0 only once it is closed. Probe-first and idempotent, so a
//! restart (or reboot) re-enters from a fresh probe and never double-closes.
//! Transient failures back off and re-probe (no overall deadline — systemd
//! `Restart=on-failure` owns process death; this loop owns in-process
//! progress).
//!
//! ## Partial-close note (a DNA-surface limitation, recorded as a decision)
//!
//! The spec asks a partial close (a `ClosingStateSummary` committed but
//! `close_chain` not yet issued) to be finished by `close_chain` ONLY, without
//! re-collecting. The alliance transactor exposes **no bare `close_chain` /
//! finish extern** — `close_agent_chain` is the only close path and it always
//! commits a fresh summary before `close_chain`. So the only safe finish with
//! today's externs is to re-run the full prepare → collect → close over the
//! *current* chain top: the orphaned first summary is harmless (the author-time
//! validator only checks the final summary that directly precedes the
//! `CloseChain`), and because the workload is quiesced before close the chain
//! top does not move between prepare and close, so the staleness pin holds.
//! The probe still distinguishes the partial-close state for the status report;
//! the *action* is identical. (Honoring "close_chain only" verbatim would need
//! a new DNA extern — out of scope for this milestone; flagged for the DNA
//! owner.)

use std::time::Duration;

use anyhow::Result;
use holo_hash::{AgentPubKey, AgentPubKeyB64};
use rave_engine::types::entries::migration::v0_1::{
    NotarySignature, PrepareCloseResponse, SignClosingResponse, SignRequest, SummaryStatePayload,
};
use zfuel::fuel::ZFuel;

use crate::conductor::Conductor;
use crate::config::Config;
use crate::policy::{self, PolicyError, SignOutcome, Signer, Sleeper};
use crate::probe::{probe_close_state, CloseNext, CloseState};
use crate::state_file::{Phase, State, Step};

/// Outcome of one close attempt, before the supervised loop decides to exit or
/// retry.
enum CloseOutcome {
    /// The chain is closed (now or already). Exit 0.
    Closed,
    /// A hard stop — warrants on the agent. Exit nonzero; the operator must act.
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
    // A single `State` carried across every pass: progress fields (`agent`,
    // `signatures_collected` / `signatures_threshold`) set during collection
    // must survive transient passes and persist INTO the final closed state, so
    // the report collector (`make migrate-status`) sees the agent + signature
    // progress even after a successful close — not the all-`None` a per-call
    // `State::new(...)` would re-stamp.
    let mut state = State::new(Phase::Close, Step::Probing, "");
    let backoff = cfg.loop_backoff();
    let mut attempts: u32 = 0;
    loop {
        if *shutdown.borrow() {
            return shutdown_before_complete();
        }
        match attempt(conductor, cfg, &mut state).await {
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
async fn attempt(conductor: &dyn Conductor, cfg: &Config, state: &mut State) -> CloseOutcome {
    persist(cfg, state, |s| {
        s.step = Step::Probing;
        s.message = "probing old-chain close state".into();
    });
    let close_state = match probe_close_state(conductor).await {
        Ok(s) => s,
        Err(e) => return CloseOutcome::Transient(e.context("probing close state")),
    };

    if let CloseState::PartialClose = close_state {
        tracing::warn!(
            "partial close detected (summary committed, chain open) — finishing via \
             prepare→collect→close over the current chain top (no bare close_chain extern exists)"
        );
    }

    // The already-closed restart path returns `Closed` straight from the probe,
    // WITHOUT running `prepare_collect_close` — so the agent / signature fields it
    // would otherwise set are still unpopulated. Recover them from the committed
    // close the probe already read (its signed payload names the agent and
    // carries the collected signatures), so the report (`make migrate-status`)
    // still shows attribution after a restart onto an already-closed chain. The
    // GD's M threshold is NOT carried in `CommittedClose`, so it stays unset.
    if let CloseState::Closed(committed) = &close_state {
        let agent_b64 = AgentPubKeyB64::from(committed.payload.agent_pubkey.clone()).to_string();
        let collected = committed.notary_signatures.len() as u32;
        persist(cfg, state, |s| {
            if s.agent.is_none() {
                s.agent = Some(agent_b64);
            }
            if s.signatures_collected.is_none() {
                s.signatures_collected = Some(collected);
            }
        });
    }

    match close_state.next() {
        CloseNext::AlreadyClosed => CloseOutcome::Closed,
        // Both an open chain and a partial close route through the same path —
        // see the module-level partial-close note for why.
        CloseNext::FinishCloseOnly | CloseNext::PrepareCollectClose => {
            prepare_collect_close(conductor, cfg, state).await
        }
    }
}

/// The full close path: fee-drop if owed → prepare → collect M-of-N → close.
async fn prepare_collect_close(
    conductor: &dyn Conductor,
    cfg: &Config,
    state: &mut State,
) -> CloseOutcome {
    // Fees owed? Drop them FIRST — a fee drop after signing voids the
    // signatures (the staleness pin), so it must precede prepare.
    match conductor.get_ledger().await {
        Ok(ledger) => {
            if ledger.fees_owed != ZFuel::zero() {
                persist(cfg, state, |s| {
                    s.step = Step::DroppingFees;
                    s.message = "fees owed — dropping before prepare".into();
                });
                if let Err(e) = conductor.drop_off_fees().await {
                    return CloseOutcome::Transient(e.context("drop_off_fees"));
                }
            }
        }
        Err(e) => return CloseOutcome::Transient(e.context("reading ledger for fee check")),
    }

    persist(cfg, state, |s| {
        s.step = Step::CollectingSignatures;
        s.message = "preparing closing summary".into();
    });
    let prepared: PrepareCloseResponse = match conductor.prepare_closing_summary().await {
        Ok(p) => p,
        Err(e) => return CloseOutcome::Transient(e.context("prepare_closing_summary")),
    };
    let agent_b64 = AgentPubKeyB64::from(prepared.payload.agent_pubkey.clone()).to_string();
    persist(cfg, state, |s| {
        s.agent = Some(agent_b64.clone());
        s.signatures_threshold = Some(prepared.closing_threshold);
        s.signatures_collected = Some(0);
        s.message = format!(
            "collecting {} of {} notary signatures",
            prepared.closing_threshold,
            prepared.closing_notaries.len()
        );
    });

    // Collect M-of-N via the parameterized policy.
    let signer = ConductorSigner {
        conductor,
        payload: prepared.payload.clone(),
        request_timeout: cfg.policy.request_timeout,
    };
    let sleeper = TokioSleeper;
    let mut rng = rand::thread_rng();
    let signatures = match policy::collect_signatures(
        prepared.closing_threshold,
        &prepared.closing_notaries,
        &cfg.policy,
        &signer,
        &sleeper,
        &mut rng,
    )
    .await
    {
        Ok(sigs) => sigs,
        Err(PolicyError::Warranted) => {
            return CloseOutcome::HardStop("agent carries warrants".into())
        }
        // Exhaustion is NOT a hard stop: more notaries may become reachable.
        // Back off and re-probe; nothing was committed, so the next attempt
        // re-prepares on the fresh chain top.
        Err(e @ PolicyError::Exhausted { .. }) => {
            return CloseOutcome::Transient(anyhow::anyhow!("{e}"))
        }
        Err(e @ PolicyError::Fatal(_)) => return CloseOutcome::Transient(anyhow::anyhow!("{e}")),
    };

    persist(cfg, state, |s| {
        s.step = Step::Closing;
        s.signatures_collected = Some(signatures.len() as u32);
        s.message = "committing close + close_chain".into();
    });
    match conductor
        .close_agent_chain(prepared.payload, signatures)
        .await
    {
        Ok(_) => CloseOutcome::Closed,
        Err(e) => CloseOutcome::Transient(e.context("close_agent_chain")),
    }
}

/// Bridges the policy's [`Signer`] to a live `request_closing_signature` zome
/// call, applying the per-request timeout and mapping the response. A
/// `Warranted` verdict is raised through the `Err` channel so the policy
/// hard-stops the whole migration rather than substituting.
struct ConductorSigner<'a> {
    conductor: &'a dyn Conductor,
    payload: SummaryStatePayload,
    request_timeout: Duration,
}

impl Signer for ConductorSigner<'_> {
    async fn sign(&self, notary: AgentPubKey) -> std::result::Result<SignOutcome, PolicyError> {
        let req = SignRequest {
            notary: notary.clone(),
            payload: self.payload.clone(),
        };
        let call = self.conductor.request_closing_signature(req);
        match tokio::time::timeout(self.request_timeout, call).await {
            Err(_elapsed) => Ok(SignOutcome::TimedOut),
            Ok(Err(e)) => {
                tracing::warn!(notary = %notary, error = %format!("{e:#}"),
                    "request_closing_signature errored");
                Ok(SignOutcome::Errored)
            }
            Ok(Ok(SignClosingResponse::Signed { signature })) => {
                Ok(SignOutcome::Signed(NotarySignature { notary, signature }))
            }
            Ok(Ok(SignClosingResponse::StateMismatch)) => Ok(SignOutcome::StateMismatch),
            Ok(Ok(SignClosingResponse::UnableToVerify)) => Ok(SignOutcome::UnableToVerify),
            Ok(Ok(SignClosingResponse::Warranted(_))) => Err(PolicyError::Warranted),
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
/// file here: leaving the last meaningful record (agent + signature attribution
/// a prior pass wrote) intact rather than clobbering it with this pass's
/// possibly-bare in-memory `State` (the top-of-loop bail can fire before any
/// `attempt` has populated it). The next restart's first probe rewrites it.
fn shutdown_before_complete() -> Result<()> {
    anyhow::bail!("shutdown before close completed (chain still open)")
}

/// Apply `f` to the carried `state` and persist it, swallowing (logging) a
/// write error — a failed status write must never abort the migration itself.
/// Mutating the carried `state` in place (rather than re-stamping a fresh
/// `State::new`) is what makes `agent` / `signatures_*` progress persist across
/// passes and into the final closed record.
fn persist(cfg: &Config, state: &mut State, f: impl FnOnce(&mut State)) {
    f(state);
    if let Err(e) = state.write(&cfg.state_file) {
        tracing::error!(error = %format!("{e:#}"), "failed writing state file");
    }
}
