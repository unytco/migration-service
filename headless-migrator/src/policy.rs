//! The driver-side M-of-N closing-signature collection policy — one
//! parameterized implementation ([`PolicyOpts`] + [`collect_signatures`]),
//! kept pure over an injected signer + RNG so it is exhaustively unit-testable
//! without a conductor (the spec's mocked-seam pattern).
//!
//! The rules it encodes (unyt-dna.md § Signature-collection policy):
//!
//! - Request from only **M** notaries, chosen **at random** from the N in the
//!   GD — never all N.
//! - A notary that does not respond within the per-request timeout counts as
//!   **failed**; substitute a random not-yet-tried notary.
//! - `UnableToVerify` is likewise transient → substitute.
//! - `StateMismatch` → retry the **same** notary first (its DHT view is
//!   catching up), with backoff, substituting only after `state_mismatch_retries`
//!   consecutive mismatches.
//! - A merely-slow signer is **never** substituted: slowness only shows up as a
//!   `TimedOut` outcome (the caller applies the timeout); a signer that
//!   eventually returns `Signed` is honored, never replaced.
//! - List exhausted below M → the attempt fails (nothing was committed; the
//!   agent re-runs later). No overall deadline.
//! - `Warranted` → hard stop for the whole migration.

use std::collections::HashSet;
use std::future::Future;
use std::time::Duration;

use anyhow::Result;
use holo_hash::AgentPubKey;
use rand::seq::SliceRandom;
use rand::Rng;
use rave_engine::types::entries::migration::v0_1::NotarySignature;

/// Tunable knobs for the collection policy — every open question the spec left
/// to the driver. Defaults are read from the environment so `automation/` can
/// tune a window without a rebuild.
#[derive(Debug, Clone)]
pub struct PolicyOpts {
    /// Per-request signing timeout — "generous" so a slow-but-live notary is
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

/// The resolved outcome of asking one notary to sign, after the caller has
/// applied the per-request timeout. Mirrors `SignClosingResponse` plus the
/// timeout / transport cases the policy must react to. Decoupling the policy
/// from the live call site is what makes it unit-testable.
#[derive(Debug, Clone, PartialEq)]
pub enum SignOutcome {
    /// The notary signed the payload.
    Signed(NotarySignature),
    /// Chain top / recompute mismatch — retry the SAME notary (DHT lag).
    StateMismatch,
    /// Transient notary read/decode failure — substitute.
    UnableToVerify,
    /// The request exceeded the per-request timeout — substitute. A signer that
    /// is merely slow but eventually answers returns `Signed`, not this.
    TimedOut,
    /// Transport/transient error reaching the notary (the request errored) —
    /// substitute. Same handling as a timeout for collection purposes.
    Errored,
}

/// Why a collection attempt did not reach M signatures.
#[derive(Debug, Clone, PartialEq)]
pub enum PolicyError {
    /// The agent carries warrants — a hard stop for the whole migration.
    Warranted,
    /// The N-list was exhausted (or could never reach M) before M signatures
    /// were collected. Nothing was committed; the agent re-runs later.
    Exhausted { collected: usize, threshold: u32 },
    /// The injected signer returned a hard error the policy can't classify.
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
                "notary list exhausted with {collected}/{threshold} signatures collected"
            ),
            PolicyError::Fatal(e) => write!(f, "fatal error collecting signatures: {e}"),
        }
    }
}

impl std::error::Error for PolicyError {}

/// The signer the policy drives: ask `notary` to sign, returning a resolved
/// [`SignOutcome`] (the timeout is the caller's to apply). `Warranted` is
/// surfaced out of band as `Err(PolicyError::Warranted)` because it stops the
/// whole migration, not just this notary.
pub trait Signer {
    fn sign(
        &self,
        notary: AgentPubKey,
    ) -> impl Future<Output = std::result::Result<SignOutcome, PolicyError>> + Send;
}

/// A pause primitive the policy uses for same-notary `StateMismatch` backoff —
/// real code sleeps; tests pass a no-op so the state machine runs instantly.
pub trait Sleeper {
    fn sleep(&self, dur: Duration) -> impl Future<Output = ()> + Send;
}

/// Exponential backoff for same-notary `StateMismatch` retries — delegated to
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

/// Collect ≥ `threshold` (M) distinct notary signatures from `notaries` (N) per
/// the policy. Pure over `signer` + `sleeper` + `rng`; no I/O of its own.
///
/// Selection: shuffle N once, draw the first M as the working set, keep the
/// rest as the substitution reserve. On a substitutable failure (timeout /
/// errored / `UnableToVerify`, or a notary that exhausts its `StateMismatch`
/// retries) draw the next reserve notary. Running out of reserves below M is
/// `Exhausted`.
pub async fn collect_signatures<S, P, R>(
    threshold: u32,
    notaries: &[AgentPubKey],
    opts: &PolicyOpts,
    signer: &S,
    sleeper: &P,
    rng: &mut R,
) -> std::result::Result<Vec<NotarySignature>, PolicyError>
where
    S: Signer,
    P: Sleeper,
    R: Rng,
{
    let m = threshold as usize;
    if m == 0 {
        return Ok(vec![]);
    }

    // Dedup to DISTINCT notary keys first. `closing_notaries` is the old DNA's
    // GD list verbatim; if it carries a duplicate AgentPubKey, two working slots
    // could target the same notary, the second `signed.insert` would drop the
    // signature, yet the slot still finishes "satisfied" — so `collected.len()`
    // would finish at M-1 and the close would never reach M (Exhausted → retried
    // forever). Counting only distinct keys makes a non-distinct GD list unable
    // to under-count, mirroring the validator (which counts DISTINCT signers).
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

    let mut collected: Vec<NotarySignature> = Vec::with_capacity(m);
    let mut signed: HashSet<AgentPubKey> = HashSet::new();

    // Each working slot drives one notary to a terminal verdict (Signed, or
    // substituted), pulling a replacement from the reserve when it fails.
    for slot in order {
        let mut current = slot;
        loop {
            let mut mismatch_attempts: u32 = 0;
            let verdict = loop {
                match signer.sign(current.clone()).await? {
                    SignOutcome::Signed(sig) => break Some(sig),
                    SignOutcome::StateMismatch => {
                        // Retry the SAME notary — its DHT view is catching up.
                        if mismatch_attempts >= opts.state_mismatch_retries {
                            tracing::warn!(
                                notary = %current,
                                attempts = mismatch_attempts,
                                "notary stuck on StateMismatch; substituting"
                            );
                            break None;
                        }
                        let delay = backoff(mismatch_attempts, opts);
                        tracing::info!(
                            notary = %current,
                            attempt = mismatch_attempts,
                            delay_ms = delay.as_millis() as u64,
                            "StateMismatch — retrying same notary after backoff"
                        );
                        sleeper.sleep(delay).await;
                        mismatch_attempts += 1;
                    }
                    other => {
                        // Timeout / errored / unable-to-verify — substitute.
                        // (A merely-slow signer never lands here: slowness is a
                        // `Signed` that simply arrives late.)
                        tracing::warn!(
                            notary = %current,
                            outcome = ?other,
                            "notary failed; substituting"
                        );
                        break None;
                    }
                }
            };

            match verdict {
                Some(sig) => {
                    // Guard against a duplicate signer sneaking in (the close
                    // validator counts DISTINCT signers only).
                    if signed.insert(sig.notary.clone()) {
                        collected.push(sig);
                    }
                    break;
                }
                None => {
                    // Substitute from the reserve, skipping any already-signed.
                    match next_reserve(&mut reserve, &signed) {
                        Some(next) => current = next,
                        None => {
                            return Err(PolicyError::Exhausted {
                                collected: collected.len(),
                                threshold,
                            })
                        }
                    }
                }
            }
        }
    }

    if collected.len() >= m {
        Ok(collected)
    } else {
        Err(PolicyError::Exhausted {
            collected: collected.len(),
            threshold,
        })
    }
}

/// Pop the next reserve notary that has not already signed.
fn next_reserve(
    reserve: &mut Vec<AgentPubKey>,
    signed: &HashSet<AgentPubKey>,
) -> Option<AgentPubKey> {
    while let Some(candidate) = reserve.pop() {
        if !signed.contains(&candidate) {
            return Some(candidate);
        }
    }
    None
}
