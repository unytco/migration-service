//! Pre-close check policy tests: the close service's M-of-N logic, driven over
//! an injected checker + seeded RNG so every branch is deterministic without a
//! conductor.
//!
//! Covers: asks exactly M; substitutes on timeout / errored / UnableToVerify /
//! NotAClosingNotary but NEVER a merely-slow notary; same-notary retry with
//! backoff on StateMismatch and TargetNotApproved, then substitution; exhaustion
//! fails; an impossible threshold is misconfigured; Warranted hard-stops.

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
async fn too_few_notaries_is_misconfigured_without_asking_anyone() {
    let checker = ScriptedChecker::empty();
    let err = collect_approvals(3, &notaries(2), &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect_err("N < M cannot succeed");
    assert!(
        matches!(&err, PolicyError::Misconfigured(why) if why.contains("2 distinct")),
        "{err:?}"
    );
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
async fn distinct_count_below_threshold_is_misconfigured() {
    // Long only because of duplicates: one distinct notary cannot reach 2.
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
    assert!(matches!(err, PolicyError::Misconfigured(_)), "{err:?}");
}

#[tokio::test]
async fn a_zero_threshold_is_misconfigured() {
    // The network closes no chain: the close must not run unchecked.
    let checker = ScriptedChecker::empty();
    let err = collect_approvals(0, &notaries(3), &opts(), &checker, &NoSleep, &mut rng())
        .await
        .expect_err("a zero threshold closes nothing");
    assert!(
        matches!(&err, PolicyError::Misconfigured(why) if why.contains("closing_threshold 0")),
        "{err:?}"
    );
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
async fn an_unavailable_notary_is_substituted_and_never_asked_again() {
    // Four notaries fail their first ask, two approve: whatever the draw, both
    // slots end on the two that approve, and no failing notary is asked twice.
    for failure in [
        CheckOutcome::TimedOut,
        CheckOutcome::Errored,
        CheckOutcome::UnableToVerify,
        CheckOutcome::NotAClosingNotary,
    ] {
        let ns = notaries(6);
        let mut scripts = HashMap::new();
        for n in &ns[..4] {
            scripts.insert(n.clone(), vec![failure.clone(); 4]);
        }
        let checker = ScriptedChecker::new(scripts);
        let mut approvals = collect_approvals(2, &ns, &opts(), &checker, &NoSleep, &mut rng())
            .await
            .unwrap_or_else(|e| panic!("{failure:?}: {e}"));
        approvals.sort();
        let mut good = ns[4..].to_vec();
        good.sort();
        assert_eq!(approvals, good, "{failure:?}");
        for n in &ns[..4] {
            assert!(
                checker.calls_to(n) <= 1,
                "{failure:?}: a failing notary asked again"
            );
        }
    }
}

#[test]
fn the_policy_reads_the_names_the_release_tooling_renders() {
    let defaults = PolicyOpts::from_lookup(|_| None).unwrap();
    assert_eq!(defaults.request_timeout, Duration::from_secs(120));
    assert_eq!(defaults.state_mismatch_retries, 5);
    assert_eq!(defaults.retry_initial, Duration::from_secs(2));
    assert_eq!(defaults.retry_max, Duration::from_secs(30));

    let env: HashMap<&str, &str> = HashMap::from([
        ("MIGRATION_AGENT_SIGN_TIMEOUT_SECS", "90"),
        ("MIGRATION_AGENT_STATE_MISMATCH_RETRIES", "7"),
        ("MIGRATION_AGENT_SIGN_RETRY_INITIAL_SECS", "3"),
        ("MIGRATION_AGENT_SIGN_RETRY_MAX_SECS", "40"),
    ]);
    let set = PolicyOpts::from_lookup(|k| env.get(k).map(|v| v.to_string())).unwrap();
    assert_eq!(set.request_timeout, Duration::from_secs(90));
    assert_eq!(set.state_mismatch_retries, 7);
    assert_eq!(set.retry_initial, Duration::from_secs(3));
    assert_eq!(set.retry_max, Duration::from_secs(40));

    let err = PolicyOpts::from_lookup(|k| {
        (k == "MIGRATION_AGENT_STATE_MISMATCH_RETRIES").then(|| "many".to_string())
    })
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("MIGRATION_AGENT_STATE_MISMATCH_RETRIES"),
        "{err}"
    );
}
