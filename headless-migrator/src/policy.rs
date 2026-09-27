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
//! - Too few notaries left to reach M: the attempt fails, and the caller
//!   decides what that means.
//! - A threshold of 0, or fewer distinct notaries than M: misconfigured, since
//!   no attempt can succeed.
//! - `Warranted` → hard stop for the whole migration.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use holo_hash::AgentPubKey;
use rand::seq::SliceRandom;
use rand::Rng;

/// Tunable knobs for the check policy, read from the environment so
/// `automation/` can tune a window without a rebuild.
#[derive(Debug, Clone)]
pub struct PolicyOpts {
    /// Per-request check timeout, generous so a slow but live notary is not
    /// mistaken for a dead one. A request exceeding this counts as failed.
    pub request_timeout: Duration,
    /// Consecutive refusals (`StateMismatch` or `TargetNotApproved`) from the
    /// SAME notary tolerated before it is substituted.
    pub state_mismatch_retries: u32,
    /// Initial backoff before asking the same notary again.
    pub retry_initial: Duration,
    /// Cap on that backoff.
    pub retry_max: Duration,
}

impl Default for PolicyOpts {
    fn default() -> Self {
        Self {
            // Generous: gossip + recompute on a loaded notary can be slow, and
            // wrongly timing out a live notary would churn substitutions.
            request_timeout: Duration::from_secs(120),
            state_mismatch_retries: 5,
            retry_initial: Duration::from_secs(2),
            retry_max: Duration::from_secs(30),
        }
    }
}

impl PolicyOpts {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(crate::config::var)
    }

    /// Read the knobs through `lookup`, so the names and defaults are testable
    /// without touching the process environment.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let parse = |key: &str, default: u64| -> Result<u64> {
            lookup(key)
                .map(|v| v.parse().map_err(|e| anyhow::anyhow!("{key}: {e}")))
                .transpose()
                .map(|v| v.unwrap_or(default))
        };
        let d = PolicyOpts::default();
        // The names the release tooling renders, so they keep SIGN.
        Ok(Self {
            request_timeout: Duration::from_secs(parse(
                "MIGRATION_AGENT_SIGN_TIMEOUT_SECS",
                d.request_timeout.as_secs(),
            )?),
            state_mismatch_retries: parse(
                "MIGRATION_AGENT_STATE_MISMATCH_RETRIES",
                d.state_mismatch_retries.into(),
            )?
            .try_into()
            .map_err(|e| anyhow::anyhow!("MIGRATION_AGENT_STATE_MISMATCH_RETRIES: {e}"))?,
            retry_initial: Duration::from_secs(parse(
                "MIGRATION_AGENT_SIGN_RETRY_INITIAL_SECS",
                d.retry_initial.as_secs(),
            )?),
            retry_max: Duration::from_secs(parse(
                "MIGRATION_AGENT_SIGN_RETRY_MAX_SECS",
                d.retry_max.as_secs(),
            )?),
        })
    }
}

/// The resolved outcome of asking one notary to check the payload, after the
/// caller has applied the per-request timeout: `CloseCheckResponse` plus the
/// timeout and transport cases.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    Approved,
    StateMismatch,
    TargetNotApproved,
    UnableToVerify,
    NotAClosingNotary,
    TimedOut,
    Errored,
}

/// Why the check did not reach M approvals.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyError {
    /// The agent carries warrants: a hard stop for the whole migration.
    Warranted,
    /// Every notary was asked before M approved.
    Exhausted { collected: usize, threshold: u32 },
    /// No attempt can reach M: a threshold of 0, or fewer distinct notaries.
    Misconfigured(String),
    /// The injected checker returned a hard error the policy can't classify.
    Fatal(String),
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PolicyError::Warranted => write!(f, "agent carries warrants: migration hard-stopped"),
            PolicyError::Exhausted {
                collected,
                threshold,
            } => write!(
                f,
                "notary list exhausted with {collected}/{threshold} approvals"
            ),
            PolicyError::Misconfigured(why) => write!(f, "{why}"),
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

/// A pause primitive the policy uses for the same-notary backoff: real code
/// sleeps, tests pass a no-op so the state machine runs instantly.
pub trait Sleeper {
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;
}

/// Exponential backoff for same-notary retries, from `ham::compute_delay_ms`,
/// whose ~10% jitter keeps slots refused at the same gossip moment from
/// retrying in lockstep.
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
        return Err(PolicyError::Misconfigured(
            "the network's definition sets closing_threshold 0, so it closes no chain".into(),
        ));
    }

    // Distinct keys first: a GD list carrying a duplicate would put two working
    // slots on one notary, and its one approval would count twice.
    let mut seen = HashSet::new();
    let distinct: Vec<AgentPubKey> = notaries
        .iter()
        .filter(|n| seen.insert((*n).clone()))
        .cloned()
        .collect();
    if distinct.len() < m {
        return Err(PolicyError::Misconfigured(format!(
            "the network's definition lists {} distinct closing notaries, fewer than its \
             closing_threshold {m}",
            distinct.len()
        )));
    }

    // Random order over the distinct N: the working set is the first M, the rest
    // the substitution reserve, so substitution is random too.
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
