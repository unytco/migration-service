//! The M-of-N pre-close check policy ([`PolicyOpts`] + [`collect_approvals`]),
//! pure over an injected checker + RNG so it is unit-testable without a
//! conductor. Approvals gate the close and are not carried in it.
//!
//! - Ask only **M** notaries, chosen **at random** from the N in the GD.
//! - A timeout, a failed call, `UnableToVerify` or `NotAClosingNotary`:
//!   substitute a random not-yet-asked notary.
//! - `StateMismatch` or `TargetNotApproved`: ask the **same** notary again after
//!   a backoff (its DHT view may be catching up), substituting only after
//!   `state_mismatch_retries` in a row.
//! - A merely slow notary is **never** substituted: only a `TimedOut` outcome
//!   (the caller applies the timeout) is.
//! - Too few notaries left to reach M: the attempt fails. Nothing was
//!   committed, so the caller probes and prepares afresh.
//! - `Warranted` → hard stop for the whole migration.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use holo_hash::AgentPubKey;
use rand::seq::SliceRandom;
use rand::Rng;

/// Tunable knobs for the collection policy — every open question the spec left
/// to the driver. Defaults are read from the environment so `automation/` can
/// tune a window without a rebuild.
#[derive(Debug, Clone)]
pub struct PolicyOpts {
    /// Per-request check timeout — "generous" so a slow-but-live notary is
    /// not mistaken for a dead one. A request exceeding this counts as failed.
    pub request_timeout: Duration,
    /// Consecutive `StateMismatch` responses from the SAME notary tolerated
    /// (retried with backoff) before that notary is substituted.
    pub state_mismatch_retries: u32,
    /// Initial backoff before a same-notary `StateMismatch` retry.
    pub retry_initial: Duration,
    /// Cap on the same-notary `StateMismatch` retry backoff.
    pub retry_max: Duration,
}

impl Default for PolicyOpts {
    fn default() -> Self {
        Self {
            // Generous: gossip + recompute on a loaded notary can be slow, and
            // wrongly timing out a live signer would churn substitutions.
            request_timeout: Duration::from_secs(120),
            state_mismatch_retries: 5,
            retry_initial: Duration::from_secs(2),
            retry_max: Duration::from_secs(30),
        }
    }
}

impl PolicyOpts {
    pub fn from_env() -> Result<Self> {
        fn dur_secs(key: &str, default: u64) -> Result<Duration> {
            let raw = std::env::var(key).ok().filter(|v| !v.is_empty());
            match raw {
                Some(v) => Ok(Duration::from_secs(
                    v.parse().map_err(|e| anyhow::anyhow!("{key}: {e}"))?,
                )),
                None => Ok(Duration::from_secs(default)),
            }
        }
        let d = PolicyOpts::default();
        Ok(Self {
            request_timeout: dur_secs("MIGRATION_AGENT_SIGN_TIMEOUT_SECS", 120)?,
            state_mismatch_retries: std::env::var("MIGRATION_AGENT_STATE_MISMATCH_RETRIES")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| v.parse())
                .transpose()
                .map_err(|e| anyhow::anyhow!("MIGRATION_AGENT_STATE_MISMATCH_RETRIES: {e}"))?
                .unwrap_or(d.state_mismatch_retries),
            retry_initial: dur_secs("MIGRATION_AGENT_SIGN_RETRY_INITIAL_SECS", 2)?,
            retry_max: dur_secs("MIGRATION_AGENT_SIGN_RETRY_MAX_SECS", 30)?,
        })
    }
}

/// The resolved outcome of asking one notary to check the payload, after the
/// caller has applied the per-request timeout: `CloseCheckResponse` plus the
/// timeout and transport cases.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    Approved,
    /// Ask the same notary again.
    StateMismatch,
    /// Ask the same notary again.
    TargetNotApproved,
    /// Substitute.
    UnableToVerify,
    /// Substitute.
    NotAClosingNotary,
    /// Substitute.
    TimedOut,
    /// Substitute.
    Errored,
}

/// Why a collection attempt did not reach M signatures.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyError {
    /// The agent carries warrants — a hard stop for the whole migration.
    Warranted,
    /// The N-list was exhausted (or could never reach M) before M notaries
    /// approved. Nothing was committed; the agent re-runs later.
    Exhausted { collected: usize, threshold: u32 },
    /// The injected checker returned a hard error the policy can't classify.
    Fatal(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::Warranted => write!(f, "agent carries warrants — migration hard-stopped"),
            PolicyError::Exhausted {
                collected,
                threshold,
            } => write!(
                f,
                "notary list exhausted with {collected}/{threshold} approvals"
            ),
            PolicyError::Fatal(e) => write!(f, "fatal error collecting approvals: {e}"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// The checker the policy drives: ask `notary` to check the payload, returning
/// a resolved [`CheckOutcome`] (the timeout is the caller's to apply).
/// `Warranted` is surfaced as `Err(PolicyError::Warranted)` because it stops
/// the whole migration, not just this notary.
pub trait Checker {
    fn check(
        &self,
        notary: AgentPubKey,
    ) -> impl Future<Output = std::result::Result<CheckOutcome, PolicyError>> + Send;
}

/// A pause primitive the policy uses for same-notary `StateMismatch` backoff —
/// real code sleeps; tests pass a no-op so the state machine runs instantly.
pub trait Sleeper {
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;
}

/// Exponential backoff for same-notary retries — delegated to
/// `ham::compute_delay_ms` (the dep's pub-exported, jittered backoff) so all
/// slots that hit `StateMismatch` at the same gossip moment do NOT retry in
/// lockstep: its ~10% wall-clock jitter de-synchronizes them. A hand-rolled
/// copy would drop that jitter, so reuse the one source of truth.
fn backoff(attempt: u32, opts: &PolicyOpts) -> Duration {
    let cfg = ham::BackoffConfig {
        initial_ms: opts.retry_initial.as_millis().min(u64::MAX as u128) as u64,
        max_ms: opts.retry_max.as_millis().min(u64::MAX as u128) as u64,
        // The policy substitutes a stuck notary after `state_mismatch_retries`,
        // so this log-escalation knob is never actually reached; mirror ham's
        // default rather than invent a meaning for it.
        escalate_after: ham::BackoffConfig::default().escalate_after,
    };
    Duration::from_millis(ham::compute_delay_ms(attempt, &cfg))
}

/// Collect `threshold` (M) distinct approving notaries from `notaries` (N) per
/// the policy. Pure over `checker` + `sleeper` + `rng`; no I/O of its own.
///
/// Selection: shuffle N once, draw the first M as the working set, keep the
/// rest as the substitution reserve. On a substitutable failure, or a notary
/// that exhausts its same-notary retries, draw the next reserve notary.
/// Running out of reserves below M is `Exhausted`.
pub async fn collect_approvals<C, P, R>(
    threshold: u32,
    notaries: &[AgentPubKey],
    opts: &PolicyOpts,
    checker: &C,
    sleeper: &P,
    rng: &mut R,
) -> std::result::Result<Vec<AgentPubKey>, PolicyError>
where
    C: Checker,
    P: Sleeper,
    R: Rng,
{
    let m = threshold as usize;
    if m == 0 {
        return Ok(vec![]);
    }

    // Distinct keys first: a GD list carrying a duplicate would put two working
    // slots on one notary, and the close would never reach M.
    let mut seen = HashSet::new();
    let distinct: Vec<AgentPubKey> = notaries
        .iter()
        .filter(|n| seen.insert((*n).clone()))
        .cloned()
        .collect();
    if distinct.len() < m {
        return Err(PolicyError::Exhausted {
            collected: 0,
            threshold,
        });
    }

    // Random order over the distinct N; the working set is the first M, the rest
    // are the substitution reserve — so substitution is also random.
    let mut order: Vec<AgentPubKey> = distinct;
    order.shuffle(rng);
    let mut reserve = order.split_off(m); // `order` now holds exactly M.

    let mut approved: Vec<AgentPubKey> = Vec::with_capacity(m);

    // Each working slot drives one notary to a terminal verdict (Approved, or
    // substituted), pulling a replacement from the reserve when it fails.
    for slot in order {
        let mut current = slot;
        loop {
            let mut mismatch_attempts: u32 = 0;
            let approved_now = loop {
                match checker.check(current.clone()).await? {
                    CheckOutcome::Approved => break true,
                    outcome @ (CheckOutcome::StateMismatch | CheckOutcome::TargetNotApproved) => {
                        if mismatch_attempts >= opts.state_mismatch_retries {
                            tracing::warn!(
                                notary = %current,
                                attempts = mismatch_attempts,
                                ?outcome,
                                "notary still refusing; substituting"
                            );
                            break false;
                        }
                        let delay = backoff(mismatch_attempts, opts);
                        tracing::info!(
                            notary = %current,
                            attempt = mismatch_attempts,
                            delay_ms = delay.as_millis() as u64,
                            ?outcome,
                            "asking the same notary again after backoff"
                        );
                        sleeper.sleep(delay).await;
                        mismatch_attempts += 1;
                    }
                    other => {
                        tracing::warn!(
                            notary = %current,
                            outcome = ?other,
                            "notary failed; substituting"
                        );
                        break false;
                    }
                }
            };

            if approved_now {
                approved.push(current);
                break;
            }
            match reserve.pop() {
                Some(next) => current = next,
                None => {
                    return Err(PolicyError::Exhausted {
                        collected: approved.len(),
                        threshold,
                    })
                }
            }
        }
    }

    Ok(approved)
}
