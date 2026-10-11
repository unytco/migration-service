//! The close service: a supervised loop that takes the old chain from open to
//! closed and exits 0 only once it is closed. Probe-first and idempotent, so a
//! restart (or reboot) re-enters from a fresh probe and never double-closes.
//! Transient failures back off and re-probe, with no overall deadline. A lost
//! conductor or lair connection exits instead: a closed `ham` connection never
//! reopens, so systemd `Restart=on-failure` resumes the service on a fresh one.

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
    /// A failure a later pass can clear.
    Transient(anyhow::Error),
}

/// Run the close service until the chain is closed (`Ok`). Every other end is an
/// `Err`.
pub async fn run(
    conductor: &dyn Conductor,
    cfg: &Config,
    shutdown: &mut ham::ShutdownRx,
) -> Result<()> {
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
            CloseOutcome::Transient(e) if is_connection_lost(&e) => {
                let e = e.context("conductor or lair connection lost, exiting to reconnect");
                persist(cfg, &mut state, |s| {
                    s.step = Step::Probing;
                    s.message = format!("{e:#}");
                });
                return Err(e);
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

/// `ham`'s `"io error"` needle also matches a response that failed to decode, and
/// a response means the socket answered.
fn is_connection_lost(e: &anyhow::Error) -> bool {
    ham::is_connection_error(e) && !crate::dna_errors::is_response_decode_failure(&format!("{e:#}"))
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

/// Exits nonzero: exiting 0 would leave the unit looking finished, and
/// `Restart=on-failure` would not resume a run stopped from outside systemd. The
/// state file is left alone, so a stopped service keeps showing the agent and
/// approvals a prior pass recorded; the in-memory `State` can still be bare here.
fn shutdown_before_complete() -> Result<()> {
    anyhow::bail!("shutdown before close completed (chain still open)")
}

/// A failed write must never abort the migration.
fn persist(cfg: &Config, state: &mut State, f: impl FnOnce(&mut State)) {
    f(state);
    if let Err(e) = state.write(&cfg.state_file) {
        tracing::error!(error = %format!("{e:#}"), "failed writing state file");
    }
}
