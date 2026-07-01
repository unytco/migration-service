//! Open-service recovery decisions: an `init` rejection is classified into the
//! right action — a non-fresh chain → a HARD stop (with the chain opened at
//! genesis, pre-existing value is anomalous, not a recoverable reinstall); the
//! double-migration guard → re-verify; a terminal `Invalid` verdict (wrong
//! carried key, insufficient/invalid signatures, malformed carry-forward) → a
//! HARD failure (NOT an infinite retry); anything else → transient back-off.
//! (This proves the classification that drives those decisions.)
//!
//! The substrings asserted here are copied from the alliance integrity
//! validators and the coordinator guard — see `src/dna_errors.rs`.

use headless_migrator::open::{classify_migration_init_error, InitErrorClass};

mod support;

use std::time::Duration;

use headless_migrator::config::{Config, OpenConfig};
use headless_migrator::open::{self, OpenParams};
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{Phase, State, Step};

/// A `Config` whose conductor ports point nowhere, with a unique temp state file
/// and snappy retries.
fn down_cfg(state_file: std::path::PathBuf) -> Config {
    Config {
        admin_port: 1, // unroutable — no conductor listening
        app_port: 1,
        app_id: "unyt".into(),
        role_name: "alliance".into(),
        request_timeout_secs: 1,
        state_file,
        retry_initial: Duration::from_millis(1),
        retry_max: Duration::from_millis(2),
        policy: PolicyOpts {
            request_timeout: Duration::from_secs(1),
            state_mismatch_retries: 1,
            retry_initial: Duration::from_millis(1),
            retry_max: Duration::from_millis(2),
        },
        to_dna: None,
    }
}

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-open-test-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// Fix 1 (the monotonic latch survives a cross-process open restart): with a
/// verified record (`safe_to_teardown = true`) already on disk, a FRESH
/// `open::run` — exactly a restarted process — must NOT clobber that latch on its
/// very first persist, even though it starts from a default `State{..false}` and
/// the conductor is down. This is the round-2 clobber bug: the old code persisted
/// the fresh `false` (line ~168) before reading the prior value (line ~189), so a
/// real restart flipped a verified migration's authoritative `.safe_to_teardown`
/// back to false. Driven without a live conductor — the admin connect fails, so
/// the loop persists Probing then backs off; a shutdown fired after the first pass
/// lets `run` return. The persisted latch must still read `true`.
#[tokio::test]
async fn open_restart_does_not_clobber_persisted_safe_to_teardown() {
    let state_file = tmp("open-restart-no-clobber");

    // A prior verified open persisted the monotonic latch.
    let mut verified = State::new(Phase::Open, Step::Done, "new chain opened + verified");
    verified.new_chain_opened = true;
    verified.old_chain_closed = true;
    verified.safe_to_teardown = true;
    verified.write(&state_file).unwrap();
    assert!(State::persisted_safe_to_teardown(&state_file));

    // A happ bundle must exist for `assert_happ_path` (the very first check in
    // `open::run`); its contents don't matter (the conductor is down).
    let happ = tmp("dummy-happ");
    std::fs::write(&happ, b"not a real happ").unwrap();

    let cfg = down_cfg(state_file.clone());
    let open_cfg = OpenConfig {
        happ_path: happ.clone(),
        joining_url: "http://127.0.0.1:1".into(),
        network_seed: None,
        gd_wait_timeout: Duration::from_secs(1800),
    };
    let params = OpenParams {
        router_url: "http://127.0.0.1:1".into(),
        from_dna: support::dna_b64(1),
        to_dna: support::dna_b64(2),
        agent_key: support::agent(3),
        lair_url: "unix:///nonexistent".into(),
        lair_passphrase: "x".into(),
    };

    // Fire shutdown shortly after the first pass so `run` returns from the
    // backoff sleep (the loop is probe→admin-connect-fails→Transient→backoff). A
    // shutdown before the open completes exits nonzero (the chain isn't verified
    // yet — a supervised one-shot exits 0 only on success); the point of THIS
    // test is the orthogonal latch invariant below, which must hold regardless.
    let (tx, rx) = tokio::sync::watch::channel(false);
    let mut shutdown = rx;
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(60)).await;
        let _ = tx.send(true);
    });

    let result = open::run(&cfg, &open_cfg, &params, &mut shutdown).await;
    assert!(
        result.is_err(),
        "an incomplete open interrupted by shutdown exits nonzero (not Ok)"
    );

    // The crux: the fresh run's persists did NOT lower the latch.
    assert!(
        State::persisted_safe_to_teardown(&state_file),
        "a fresh open restart must not clobber the persisted safe_to_teardown=true"
    );
    let reread = State::read(&state_file).unwrap();
    assert!(
        reread.safe_to_teardown,
        "the re-read record still carries the latched true after the restart's persists"
    );

    let _ = std::fs::remove_file(&state_file);
    let _ = std::fs::remove_file(&happ);
}

#[test]
fn non_fresh_chain_messages_classify_as_non_fresh_chain() {
    // The open integrity validator asserts a fresh chain (zero balance / fees):
    // "The Summary can only be added to a fresh chain" — the exact two
    // `validate_opening_state_summary` verdicts, plus the older
    // `"source chain not empty"` phrasing the classifier still honors.
    for msg in [
        "Invalid balance on current chain, The Summary can only be added to a fresh chain",
        "Invalid fees on current chain, The Summary can only be added to a fresh chain",
        "wasm error: source chain not empty",
    ] {
        assert_eq!(
            classify_migration_init_error(msg),
            InitErrorClass::NonFreshChain,
            "{msg}"
        );
    }
}

#[test]
fn already_migrated_routes_to_verify() {
    // The coordinator's double-migration guard text.
    for msg in [
        "Failed to call zome: Chain has already been migrated",
        "chain already migrated",
    ] {
        assert_eq!(
            classify_migration_init_error(msg),
            InitErrorClass::AlreadyMigrated,
            "{msg}"
        );
    }
}

#[test]
fn agent_mismatch_is_a_hard_failure() {
    // The carried key is not the notarized agent — the opening-summary validator
    // rejects it. Retrying with the same (wrong) key can never succeed, so this
    // must be a hard failure, never a transient retry.
    let msg = "Invalid(\"Opening state summary author does not match the notarized agent\")";
    assert_eq!(
        classify_migration_init_error(msg),
        InitErrorClass::HardFailure,
        "agent mismatch must hard-fail"
    );
}

#[test]
fn insufficient_or_invalid_signatures_is_a_hard_failure() {
    // Every terminal verdict from `verify_notary_threshold` /
    // `validate_carry_forward_structure` is unfixable by retry → HardFailure.
    for msg in [
        // Not enough valid signatures for the new GD's opening threshold.
        "Invalid(\"Close carries 1 valid notary signature(s); the threshold is 3\")",
        // A signature that doesn't verify over the payload.
        "Invalid(\"Notary signature does not verify over the payload: AgentPubKey(...)\")",
        // A signer not in the configured opening-notary list.
        "Invalid(\"Signer is not a configured notary: AgentPubKey(...)\")",
        // A duplicate notary signer.
        "Invalid(\"Duplicate notary signer: AgentPubKey(...)\")",
        // The agent tried to notarise its own close.
        "Invalid(\"A migrating agent cannot notarise its own close\")",
        // Migration disabled in this direction (empty list / zero threshold).
        "Invalid(\"migration is disabled in this direction: ...\")",
        // Malformed carry-forward section (too large / duplicate keys).
        "Invalid(\"Agreement carry-forward section has 99 entries; limit is 64\")",
        "Invalid(\"Duplicate agreement in carry-forward section: ActionHash(...)\")",
    ] {
        assert_eq!(
            classify_migration_init_error(msg),
            InitErrorClass::HardFailure,
            "{msg}"
        );
    }
}

#[test]
fn other_errors_are_transient() {
    for msg in [
        "Websocket closed: ConnectionClosed",
        "deserialize error",
        "some unrelated failure",
    ] {
        assert_eq!(
            classify_migration_init_error(msg),
            InitErrorClass::Transient,
            "{msg}"
        );
    }
}

#[test]
fn ambiguous_tokens_do_not_misclassify_transient_errors() {
    // Regression for the substring-fragility fix: the bare tokens `"fresh"` and
    // `"threshold"` used to leak into NonFreshChain / HardFailure. A transient
    // error that merely happens to contain those words must stay Transient, so a
    // recoverable blip is never promoted to a permanent stop / a reinstall loop.
    for msg in [
        // "fresh" in an unrelated, recoverable context.
        "connection refreshed; retrying the websocket",
        "stale cache, fetching a fresh snapshot",
        // "threshold" unrelated to the notary-signature verdict.
        "rate limiter threshold exceeded, backing off",
        "the threshold is configured but the host is unreachable",
    ] {
        assert_eq!(
            classify_migration_init_error(msg),
            InitErrorClass::Transient,
            "ambiguous token must stay transient: {msg}"
        );
    }
}

#[test]
fn hard_failure_is_not_shadowed_by_already_migrated_phrase() {
    // Ordering regression: a genuine terminal `Invalid` verdict that happens to
    // mention "already migrated" must classify as HardFailure, never be shadowed
    // into a re-verify by the broad "already migrated" token (which is now
    // checked AFTER the hard-failure phrases).
    let msg = "Invalid(\"Opening state summary author does not match the notarized agent; \
               the chain has already migrated under a different key\")";
    assert_eq!(
        classify_migration_init_error(msg),
        InitErrorClass::HardFailure,
        "a hard-failure verdict mentioning 'already migrated' must not be shadowed"
    );
}
