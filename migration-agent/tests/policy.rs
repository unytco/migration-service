//! Signature-collection policy tests — the heart of the close service's
//! M-of-N logic, driven over an injected signer + seeded RNG so every branch is
//! deterministic without a conductor (the spec's mocked-seam tier).
//!
//! Covers: picks exactly M; substitutes on timeout / errored / UnableToVerify
//! but NEVER a merely-slow signer; same-notary StateMismatch retry with backoff
//! then substitution; exhaustion fails; Warranted hard-stops.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use holo_hash::AgentPubKey;
use migration_agent::policy::{
    collect_signatures, PolicyError, PolicyOpts, SignOutcome, Signer, Sleeper,
};
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use rave_engine::types::entries::migration::v0_1::NotarySignature;

/// N distinct notary keys, deterministic (`raw_36` with the index as a byte).
fn notaries(n: u8) -> Vec<AgentPubKey> {
    (0..n)
        .map(|i| AgentPubKey::from_raw_36(vec![i + 1; 36]))
        .collect()
}

fn sig_for(notary: &AgentPubKey) -> SignOutcome {
    SignOutcome::Signed(NotarySignature {
        notary: notary.clone(),
        signature: hdi::prelude::Signature([0u8; 64]),
    })
}

/// A scripted signer: a per-notary queue of outcomes consumed in order; once a
/// notary's script is empty it `Signed`s. Records the call order so tests can
/// assert which notaries were asked and how many times.
struct ScriptedSigner {
    scripts: Mutex<HashMap<AgentPubKey, Vec<SignOutcome>>>,
    calls: Mutex<Vec<AgentPubKey>>,
}

impl ScriptedSigner {
    fn new(scripts: HashMap<AgentPubKey, Vec<SignOutcome>>) -> Self {
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

impl Signer for ScriptedSigner {
    async fn sign(&self, notary: AgentPubKey) -> Result<SignOutcome, PolicyError> {
        self.calls.lock().unwrap().push(notary.clone());
        let mut scripts = self.scripts.lock().unwrap();
        let next = scripts.get_mut(&notary).and_then(|q| {
            if q.is_empty() {
                None
            } else {
                Some(q.remove(0))
            }
        });
        match next {
            Some(SignOutcome::Signed(_)) | None => Ok(sig_for(&notary)),
            Some(other) => Ok(other),
        }
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
    // 5 notaries, threshold 3, all sign → exactly 3 signatures, exactly 3 calls
    // (M working slots, no substitution, the other 2 are never touched).
    let signer = ScriptedSigner::empty();
    let sigs = collect_signatures(3, &notaries(5), &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("collection succeeds");
    assert_eq!(sigs.len(), 3, "collects exactly M signatures");
    assert_eq!(
        signer.call_count(),
        3,
        "asks exactly M notaries when all sign first try"
    );
    // The signers are distinct.
    let mut signers: Vec<_> = sigs.iter().map(|s| s.notary.clone()).collect();
    signers.sort();
    signers.dedup();
    assert_eq!(signers.len(), 3, "M distinct signers");
}

#[tokio::test]
async fn substitutes_on_timeout() {
    // Two notaries time out once → they are substituted; we still reach M, and a
    // timed-out notary is asked at most once (substituted, never retried). 6
    // notaries / threshold 2 guarantees enough reserve for both substitutions
    // regardless of the shuffle.
    let ns = notaries(6);
    let mut scripts = HashMap::new();
    scripts.insert(ns[0].clone(), vec![SignOutcome::TimedOut]);
    scripts.insert(ns[1].clone(), vec![SignOutcome::TimedOut]);
    let signer = ScriptedSigner::new(scripts);
    let sigs = collect_signatures(2, &ns, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("collection succeeds despite timeouts");
    assert_eq!(sigs.len(), 2, "still collects M after substitution");
    // A timed-out notary is asked at most once (substituted, not retried).
    assert!(signer.calls_to(&ns[0]) <= 1);
    assert!(signer.calls_to(&ns[1]) <= 1);
    // None of the collected signatures are from a notary that only timed out.
    assert!(!sigs
        .iter()
        .any(|s| s.notary == ns[0] && signer.calls_to(&ns[0]) == 1));
    assert!(!sigs
        .iter()
        .any(|s| s.notary == ns[1] && signer.calls_to(&ns[1]) == 1));
}

#[tokio::test]
async fn never_substitutes_a_slow_but_signing_notary() {
    // A "slow" signer is modeled as a notary that simply Signs (its slowness is
    // absorbed by the per-request timeout the CALLER applies — within the
    // policy it is a plain Signed). So every working-slot notary signs and is
    // asked exactly once; no substitution happens at all.
    let ns = notaries(4);
    let signer = ScriptedSigner::empty(); // all Sign on first ask
    let sigs = collect_signatures(4, &ns, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("all four sign");
    assert_eq!(sigs.len(), 4);
    assert_eq!(
        signer.call_count(),
        4,
        "a signing (even if slow) notary is asked once and never substituted"
    );
    for n in &ns {
        assert_eq!(signer.calls_to(n), 1, "each notary asked exactly once");
    }
}

#[tokio::test]
async fn retries_same_notary_on_state_mismatch_then_succeeds() {
    // One notary returns StateMismatch twice (DHT lag) then Signs → it is
    // retried on the SAME key (with backoff) and ultimately counts, with no
    // substitution.
    let ns = notaries(3);
    let mut scripts = HashMap::new();
    scripts.insert(
        ns[0].clone(),
        vec![SignOutcome::StateMismatch, SignOutcome::StateMismatch],
    );
    let signer = ScriptedSigner::new(scripts);
    let sigs = collect_signatures(3, &ns, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("succeeds after same-notary retries");
    assert_eq!(sigs.len(), 3);
    // ns[0] was asked 3 times (2 mismatches + 1 success) — retried, not replaced.
    assert_eq!(
        signer.calls_to(&ns[0]),
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
    // 1 initial + state_mismatch_retries(3) more = 4 mismatches, never signs.
    scripts.insert(
        ns[0].clone(),
        vec![SignOutcome::StateMismatch; 8], // more than enough to exhaust
    );
    let signer = ScriptedSigner::new(scripts);
    let sigs = collect_signatures(3, &ns, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("succeeds via substitution");
    assert_eq!(sigs.len(), 3);
    // ns[0] asked exactly retries+1 times then substituted (not signed).
    let calls = signer.calls_to(&ns[0]);
    assert!(
        calls == 0 || calls == 4,
        "a stuck-mismatch notary is asked 0 (never picked) or retries+1 times: was {calls}"
    );
    assert!(!sigs.iter().any(|s| s.notary == ns[0]));
}

#[tokio::test]
async fn exhaustion_below_threshold_fails() {
    // 3 notaries, threshold 3, but ALL time out → no reserve to substitute from,
    // so collection is Exhausted (nothing committed; the agent re-runs later).
    let ns = notaries(3);
    let mut scripts = HashMap::new();
    for n in &ns {
        scripts.insert(n.clone(), vec![SignOutcome::TimedOut; 4]);
    }
    let signer = ScriptedSigner::new(scripts);
    let err = collect_signatures(3, &ns, &opts(), &signer, &NoSleep, &mut rng())
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
    let signer = ScriptedSigner::empty();
    let err = collect_signatures(3, &notaries(2), &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect_err("N < M cannot succeed");
    assert!(matches!(err, PolicyError::Exhausted { .. }));
    assert_eq!(signer.call_count(), 0, "no notary asked when N < M");
}

#[tokio::test]
async fn warranted_is_a_hard_stop() {
    // A Warranted verdict from any notary aborts the WHOLE migration via the Err
    // channel — not a substitution.
    let ns = notaries(5);
    struct WarrantSigner;
    impl Signer for WarrantSigner {
        async fn sign(&self, _notary: AgentPubKey) -> Result<SignOutcome, PolicyError> {
            Err(PolicyError::Warranted)
        }
    }
    let err = collect_signatures(3, &ns, &opts(), &WarrantSigner, &NoSleep, &mut rng())
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
    scripts.insert(ns[0].clone(), vec![SignOutcome::UnableToVerify]);
    scripts.insert(ns[1].clone(), vec![SignOutcome::UnableToVerify]);
    let signer = ScriptedSigner::new(scripts);
    let sigs = collect_signatures(2, &ns, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("succeeds via substitution on UnableToVerify");
    assert_eq!(sigs.len(), 2);
    // No collected signature comes from a notary that only returned UnableToVerify.
    assert!(!sigs
        .iter()
        .any(|s| s.notary == ns[0] && signer.calls_to(&ns[0]) == 1));
    assert!(!sigs
        .iter()
        .any(|s| s.notary == ns[1] && signer.calls_to(&ns[1]) == 1));
}

#[tokio::test]
async fn duplicate_notary_in_gd_list_still_collects_m_distinct() {
    // The old DNA's GD can hand back a non-distinct notary list. A naive policy
    // would let two working slots target the duplicated key, drop the second
    // signature on insert, yet still finish the slot "satisfied" → collect M-1 →
    // Exhausted → the close loops forever. With dedup, the duplicate collapses
    // to one distinct key and collection still reaches M distinct signatures.
    //
    // N = [a, a, b, c] (a duplicated), threshold 3 → must still collect 3
    // distinct signatures (a, b, c) and succeed; `a` is asked at most once.
    let a = AgentPubKey::from_raw_36(vec![1; 36]);
    let b = AgentPubKey::from_raw_36(vec![2; 36]);
    let c = AgentPubKey::from_raw_36(vec![3; 36]);
    let n_with_dup = vec![a.clone(), a.clone(), b.clone(), c.clone()];

    let signer = ScriptedSigner::empty(); // everyone signs on first ask
    let sigs = collect_signatures(3, &n_with_dup, &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("a non-distinct GD list still collects M distinct signatures");

    assert_eq!(
        sigs.len(),
        3,
        "collects M distinct signatures despite the dup"
    );
    let mut signers: Vec<_> = sigs.iter().map(|s| s.notary.clone()).collect();
    signers.sort();
    signers.dedup();
    assert_eq!(signers.len(), 3, "the M collected signers are distinct");
    assert!(
        signer.calls_to(&a) <= 1,
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
    let signer = ScriptedSigner::empty();
    let err = collect_signatures(
        2,
        &[a.clone(), a.clone(), a],
        &opts(),
        &signer,
        &NoSleep,
        &mut rng(),
    )
    .await
    .expect_err("one distinct notary cannot reach threshold 2");
    assert!(matches!(err, PolicyError::Exhausted { .. }));
}

#[tokio::test]
async fn zero_threshold_collects_nothing() {
    // A disabled direction (M == 0) collects no signatures and asks no one.
    let signer = ScriptedSigner::empty();
    let sigs = collect_signatures(0, &notaries(3), &opts(), &signer, &NoSleep, &mut rng())
        .await
        .expect("zero threshold is trivially satisfied");
    assert!(sigs.is_empty());
    assert_eq!(signer.call_count(), 0);
}
