//! Pre-close check policy tests: the close service's M-of-N logic, driven over
//! an injected checker + seeded RNG so every branch is deterministic without a
//! conductor.
//!
//! Covers: asks exactly M; substitutes on timeout / errored / UnableToVerify /
//! NotAClosingNotary but NEVER a merely-slow notary; same-notary retry with
//! backoff on StateMismatch and TargetNotApproved, then substitution; exhaustion
//! fails; Warranted hard-stops.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use headless_migrator::policy::{
    collect_approvals, CheckOutcome, Checker, PolicyError, PolicyOpts, Sleeper,
};
use holo_hash::AgentPubKey;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

/// N distinct notary keys, deterministic (`raw_36` with the index as a byte).
fn notaries(n: u8) -> Vec<AgentPubKey> {
    (0..n)
        .map(|i| AgentPubKey::from_raw_36(vec![i + 1; 36]))
        .collect()
}

/// A scripted checker: a per-notary queue of outcomes consumed in order; once a
/// notary's script is empty it approves. Records the call order so tests can
/// assert which notaries were asked and how many times.
struct ScriptedChecker {
    scripts: Mutex<HashMap<AgentPubKey, Vec<CheckOutcome>>>,
    calls: Mutex<Vec<AgentPubKey>>,
}

impl ScriptedChecker {
    fn new(scripts: HashMap<AgentPubKey, Vec<CheckOutcome>>) -> Self {
        Self {
            scripts: Mutex::new(scripts),
            calls: Mutex::new(vec![]),
        }
    }

    fn empty() -> Self {
        Self::new(HashMap::new())
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn calls_to(&self, notary: &AgentPubKey) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == notary)
            .count()
    }
}

impl Checker for ScriptedChecker {
    async fn check(&self, notary: AgentPubKey) -> Result<CheckOutcome, PolicyError> {
        self.calls.lock().unwrap().push(notary.clone());
        let mut scripts = self.scripts.lock().unwrap();
        let next = scripts.get_mut(&notary).and_then(|q| {
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });
        Ok(next.unwrap_or(CheckOutcome::Approved))
    }
}

/// A no-op sleeper so backoff is instant under test.
struct NoSleep;
impl Sleeper for NoSleep {
    async fn sleep(&self, _dur: Duration) {}
}

fn opts() -> PolicyOpts {
    PolicyOpts {
        request_timeout: Duration::from_secs(1),
        state_mismatch_retries: 3,
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(4),
    }
}

fn rng() -> ChaCha8Rng {
    ChaCha8Rng::seed_from_u64(42)
}

#[tokio::test]
async fn collects_exactly_m_and_no_more() {
    // 5 notaries, threshold 3, all approve → exactly 3 approvals, exactly 3 calls
    // (M working slots, no substitution, the other 2 are never touched).
    let checker = ScriptedChecker::empty();
    let approvals = collect_approvals(3, &notaries(5), &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("collection succeeds");
    assert_eq!(approvals.len(), 3, "collects exactly M approvals");
    assert_eq!(
        checker.call_count(),
        3,
        "asks exactly M notaries when all approve first try"
    );
    // The approvers are distinct.
    let mut approvers: Vec<_> = approvals.to_vec();
    approvers.sort();
    approvers.dedup();
    assert_eq!(approvers.len(), 3, "M distinct approvers");
}

#[tokio::test]
async fn substitutes_on_timeout() {
    // Two notaries time out once → they are substituted; we still reach M, and a
    // timed-out notary is asked at most once (substituted, never retried). 6
    // notaries / threshold 2 guarantees enough reserve for both substitutions
    // regardless of the shuffle.
    let ns = notaries(6);
    let mut scripts = HashMap::new();
    scripts.insert(ns[0].clone(), vec![CheckOutcome::TimedOut]);
    scripts.insert(ns[1].clone(), vec![CheckOutcome::TimedOut]);
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(2, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("collection succeeds despite timeouts");
    assert_eq!(approvals.len(), 2, "still collects M after substitution");
    // A timed-out notary is asked at most once (substituted, not retried).
    assert!(checker.calls_to(&ns[0]) <= 1);
    assert!(checker.calls_to(&ns[1]) <= 1);
    // No approval is from a notary that only timed out.
    assert!(!approvals
        .iter()
        .any(|s| *s == ns[0] && checker.calls_to(&ns[0]) == 1));
    assert!(!approvals
        .iter()
        .any(|s| *s == ns[1] && checker.calls_to(&ns[1]) == 1));
}

#[tokio::test]
async fn never_substitutes_a_slow_but_approving_notary() {
    // A slow notary is modeled as one that simply approves: its slowness is
    // absorbed by the per-request timeout the CALLER applies. So every
    // working-slot notary is asked exactly once; no substitution happens.
    let ns = notaries(4);
    let checker = ScriptedChecker::empty();
    let approvals = collect_approvals(4, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("all four approve");
    assert_eq!(approvals.len(), 4);
    assert_eq!(
        checker.call_count(),
        4,
        "an approving (even if slow) notary is asked once and never substituted"
    );
    for n in &ns {
        assert_eq!(checker.calls_to(n), 1, "each notary asked exactly once");
    }
}

#[tokio::test]
async fn retries_same_notary_on_state_mismatch_then_succeeds() {
    // One notary returns StateMismatch twice (DHT lag) then approves → it is
    // retried on the SAME key (with backoff) and ultimately counts, with no
    // substitution.
    let ns = notaries(3);
    let mut scripts = HashMap::new();
    scripts.insert(
        ns[0].clone(),
        vec![CheckOutcome::StateMismatch, CheckOutcome::StateMismatch],
    );
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(3, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("succeeds after same-notary retries");
    assert_eq!(approvals.len(), 3);
    // ns[0] was asked 3 times (2 mismatches + 1 success) — retried, not replaced.
    assert_eq!(
        checker.calls_to(&ns[0]),
        3,
        "the mismatching notary is retried on the same key, not substituted"
    );
}

#[tokio::test]
async fn substitutes_after_exhausting_state_mismatch_retries() {
    // A notary that NEVER clears its StateMismatch is substituted after
    // `state_mismatch_retries` attempts. With 4 notaries / threshold 3 there is
    // a reserve to substitute from, so collection still succeeds.
    let ns = notaries(4);
    let mut scripts = HashMap::new();
    // 1 initial + state_mismatch_retries(3) more = 4 mismatches, never approves.
    scripts.insert(
        ns[0].clone(),
        vec![CheckOutcome::StateMismatch; 8], // more than enough to exhaust
    );
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(3, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("succeeds via substitution");
    assert_eq!(approvals.len(), 3);
    // ns[0] asked exactly retries+1 times then substituted.
    let calls = checker.calls_to(&ns[0]);
    assert!(
        calls == 0 || calls == 4,
        "a stuck-mismatch notary is asked 0 (never picked) or retries+1 times: was {calls}"
    );
    assert!(!approvals.iter().any(|s| *s == ns[0]));
}

#[tokio::test]
async fn exhaustion_below_threshold_fails() {
    // 3 notaries, threshold 3, but ALL time out → no reserve to substitute from,
    // so collection is Exhausted (nothing committed; the agent re-runs later).
    let ns = notaries(3);
    let mut scripts = HashMap::new();
    for n in &ns {
        scripts.insert(n.clone(), vec![CheckOutcome::TimedOut; 4]);
    }
    let checker = ScriptedChecker::new(scripts);
    let err = collect_approvals(3, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect_err("must fail when N can't reach M");
    match err {
        PolicyError::Exhausted {
            collected,
            threshold,
        } => {
            assert_eq!(collected, 0);
            assert_eq!(threshold, 3);
        }
        other => panic!("expected Exhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn too_few_notaries_is_immediate_exhaustion() {
    // N < M can never succeed → Exhausted without asking anyone.
    let checker = ScriptedChecker::empty();
    let err = collect_approvals(3, &notaries(2), &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect_err("N < M cannot succeed");
    assert!(matches!(err, PolicyError::Exhausted { .. }));
    assert_eq!(checker.call_count(), 0, "no notary asked when N < M");
}

#[tokio::test]
async fn warranted_is_a_hard_stop() {
    // A Warranted verdict from any notary aborts the WHOLE migration via the Err
    // channel — not a substitution.
    let ns = notaries(5);
    struct WarrantChecker;
    impl Checker for WarrantChecker {
        async fn check(&self, _notary: AgentPubKey) -> Result<CheckOutcome, PolicyError> {
            Err(PolicyError::Warranted)
        }
    }
    let err = collect_approvals(3, &ns, &opts(), &WarrantChecker, &NoSleep, &mut rng())
        .await
        .expect_err("warranted hard-stops");
    assert!(matches!(err, PolicyError::Warranted));
}

#[tokio::test]
async fn unable_to_verify_substitutes() {
    // UnableToVerify is transient → substitute (like a timeout). 6 notaries /
    // threshold 2 guarantees enough reserve to absorb both failures regardless
    // of the shuffle, so collection succeeds via substitution.
    let ns = notaries(6);
    let mut scripts = HashMap::new();
    scripts.insert(ns[0].clone(), vec![CheckOutcome::UnableToVerify]);
    scripts.insert(ns[1].clone(), vec![CheckOutcome::UnableToVerify]);
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(2, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("succeeds via substitution on UnableToVerify");
    assert_eq!(approvals.len(), 2);
    // No approval is from a notary that only returned UnableToVerify.
    assert!(!approvals
        .iter()
        .any(|s| *s == ns[0] && checker.calls_to(&ns[0]) == 1));
    assert!(!approvals
        .iter()
        .any(|s| *s == ns[1] && checker.calls_to(&ns[1]) == 1));
}

#[tokio::test]
async fn duplicate_notary_in_gd_list_still_collects_m_distinct() {
    // The old DNA's GD can hand back a non-distinct notary list. With dedup the
    // duplicate collapses to one key and the check still reaches M distinct
    // approvals: N = [a, a, b, c], threshold 3 → a, b and c, `a` asked once.
    let a = AgentPubKey::from_raw_36(vec![1; 36]);
    let b = AgentPubKey::from_raw_36(vec![2; 36]);
    let c = AgentPubKey::from_raw_36(vec![3; 36]);
    let n_with_dup = vec![a.clone(), a.clone(), b.clone(), c.clone()];

    let checker = ScriptedChecker::empty();
    let approvals = collect_approvals(3, &n_with_dup, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("a non-distinct GD list still collects M distinct approvals");

    assert_eq!(
        approvals.len(),
        3,
        "collects M distinct approvals despite the dup"
    );
    let mut approvers: Vec<_> = approvals.to_vec();
    approvers.sort();
    approvers.dedup();
    assert_eq!(approvers.len(), 3, "the M approvers are distinct");
    assert!(
        checker.calls_to(&a) <= 1,
        "the duplicated key is treated as one notary, asked at most once"
    );
}

#[tokio::test]
async fn distinct_count_below_threshold_is_immediate_exhaustion() {
    // A list that is long only because of duplicates can't reach M: N =
    // [a, a, a] with threshold 2 has just ONE distinct notary → Exhausted
    // without churning (the dedup is what catches this; a raw-length check would
    // have let it try and then fail late).
    let a = AgentPubKey::from_raw_36(vec![1; 36]);
    let checker = ScriptedChecker::empty();
    let err = collect_approvals(
        2,
        &[a.clone(), a.clone(), a],
        &opts(),
        &checker,
        &NoSleep,
        &mut rng(),
    )
    .await
    .expect_err("one distinct notary cannot reach threshold 2");
    assert!(matches!(err, PolicyError::Exhausted { .. }));
}

#[tokio::test]
async fn zero_threshold_collects_nothing() {
    // A disabled direction (M == 0) collects no approvals and asks no one.
    let checker = ScriptedChecker::empty();
    let approvals = collect_approvals(0, &notaries(3), &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("zero threshold is trivially satisfied");
    assert!(approvals.is_empty());
    assert_eq!(checker.call_count(), 0);
}

#[tokio::test]
async fn target_not_approved_asks_the_same_notary_again() {
    let ns = notaries(3);
    let mut scripts = HashMap::new();
    scripts.insert(ns[0].clone(), vec![CheckOutcome::TargetNotApproved]);
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(3, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("the notary approves once its view catches up");
    assert_eq!(approvals.len(), 3);
    assert_eq!(checker.calls_to(&ns[0]), 2, "asked again, not substituted");
}

#[tokio::test]
async fn not_a_closing_notary_is_substituted() {
    let ns = notaries(4);
    let mut scripts = HashMap::new();
    scripts.insert(ns[0].clone(), vec![CheckOutcome::NotAClosingNotary; 8]);
    let checker = ScriptedChecker::new(scripts);
    let approvals = collect_approvals(3, &ns, &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect("a substitute approves");
    assert_eq!(approvals.len(), 3);
    assert!(checker.calls_to(&ns[0]) <= 1, "never asked again");
    assert!(!approvals.contains(&ns[0]));
}
