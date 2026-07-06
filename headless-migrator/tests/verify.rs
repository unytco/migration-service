//! `Verify` per-field comparison: a match passes; each field's mismatch is
//! reported independently with a nonzero (failing) report. Balance and CFU
//! compare the carried package against the ledger the new chain recomputes;
//! the agreement section (B49) compares it against the new chain's COMMITTED
//! opened state, read back through `get_opened_agreement_state`.

mod support;

use std::time::Duration;

use headless_migrator::conductor::OpenedAgreementState;
use headless_migrator::config::Config;
use headless_migrator::policy::PolicyOpts;
use headless_migrator::state_file::{Phase, State, Step, VerifyReport};
use headless_migrator::verify::{
    build_verify_state, verify_against_ledger, verify_agreement_state,
};
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use zfuel::fuel::ZFuel;

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "headless-migrator-verify-test-{}-{}.json",
        name,
        std::process::id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

/// A `Config` whose conductor ports point nowhere, a tiny status budget, and the
/// given state file — so `status::run` assembles its report from the persisted
/// file + failing probes (the "old side gone" path) without a live conductor.
fn down_cfg(state_file: std::path::PathBuf) -> Config {
    std::env::set_var("MIGRATION_AGENT_STATUS_CONNECT_BUDGET_MS", "150");
    Config {
        admin_port: 1,
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

fn passing_report() -> VerifyReport {
    VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        agreement_state_match: true,
        mismatches: vec![],
    }
}

fn failing_report() -> VerifyReport {
    VerifyReport {
        balance_match: false,
        carry_forward_units_match: true,
        agreement_state_match: true,
        mismatches: vec!["balance mismatch".into()],
    }
}

fn agent_b64() -> String {
    holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_36(vec![3; 36])).to_string()
}

/// Fix 2 (the standalone `verify` command never lowers the latch): a FAILING
/// verify after a prior open-service success persisted `safe_to_teardown = true`
/// must NOT write `false` over it. `verify::run` used to build `State::new(..)`
/// (default false) and write it, so a passing open followed by a (later, old-side-
/// gone) failing verify flipped the authoritative signal false. The record
/// `verify` builds now seeds the latch; the write-side guard is the backstop.
#[tokio::test]
async fn failing_verify_does_not_lower_persisted_safe_to_teardown() {
    let path = tmp("verify-failing-no-lower");
    // A prior verified open persisted the monotonic latch.
    let mut prior = State::new(Phase::Open, Step::Done, "verified");
    prior.new_chain_opened = true;
    prior.old_chain_closed = true;
    prior.safe_to_teardown = true;
    prior.write(&path).unwrap();

    // The record a FAILING `verify::run` would build + write. It SEEDS the latch
    // from the prior persisted state, so even a failing verify carries the prior
    // true forward (rather than the round-2 bug's default false).
    let state = build_verify_state(&agent_b64(), &failing_report(), &path);
    assert_eq!(state.step, Step::Failed, "the verify itself failed");
    assert!(
        state.safe_to_teardown,
        "a failing verify carries the prior monotonic true forward via the seed (never lowers it)"
    );
    state.write(&path).unwrap();

    assert!(
        State::persisted_safe_to_teardown(&path),
        "a failing standalone verify must NOT lower the persisted safe_to_teardown=true"
    );
    let _ = std::fs::remove_file(&path);
}

/// Fix 2 (a passing standalone `verify` keeps `status` reporting true): after a
/// prior persisted `true`, a passing `verify` writes the record, and a LATER
/// `status` (old side + conductor down) still reports `safe_to_teardown = true` —
/// the monotonic latch end to end across verify → status.
#[tokio::test]
async fn passing_verify_then_status_reports_safe_to_teardown() {
    let path = tmp("verify-passing-then-status");
    // Prior verified open.
    let mut prior = State::new(Phase::Open, Step::Done, "verified");
    prior.new_chain_opened = true;
    prior.old_chain_closed = true;
    prior.safe_to_teardown = true;
    prior.write(&path).unwrap();

    // A passing standalone verify persists its record (raising/keeping the latch).
    let state = build_verify_state(&agent_b64(), &passing_report(), &path);
    assert!(state.safe_to_teardown, "a passing verify sets the latch");
    state.write(&path).unwrap();

    // A later status with everything down still reports the monotonic true.
    let cfg = down_cfg(path.clone());
    let params = headless_migrator::status::StatusParams {
        router_url: "http://127.0.0.1:1".into(),
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_b64: agent_b64(),
    };
    let reported = headless_migrator::status::run(&cfg, Some(&params))
        .await
        .expect("status run Ok with everything down");
    assert!(
        reported.safe_to_teardown,
        "status after a passing verify still reports safe_to_teardown=true (monotonic)"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn ledger_halves_match_when_the_new_chain_mirrors_the_close() {
    // `verify_against_ledger` is the ledger HALF only (B49): it matches
    // balance + CFU and cannot see the agreement section, so it leaves
    // `agreement_state_match` false for the caller (`fetch_and_compare`) to
    // fill. A full `passed()` therefore needs the agreement cross-check too
    // (see `agreement_state_cross_check`), so here we check the two ledger
    // fields directly.
    let closing = summary_state(unit_map(0, 100), CarryForwardUnits::new(), 2);
    let ledger = ledger(unit_map(0, 100), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(report.balance_match, "balance: {:?}", report.mismatches);
    assert!(
        report.carry_forward_units_match,
        "cfu: {:?}",
        report.mismatches
    );
    assert!(report.mismatches.is_empty());
}

#[test]
fn balance_mismatch_is_reported() {
    let closing = summary_state(unit_map(0, 100), CarryForwardUnits::new(), 0);
    let ledger = ledger(unit_map(0, 999), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(!report.passed());
    assert!(!report.balance_match);
    assert!(report.carry_forward_units_match);
    assert_eq!(report.mismatches.len(), 1);
    assert!(report.mismatches[0].contains("balance mismatch"));
}

#[test]
fn carry_forward_units_mismatch_is_reported() {
    let closing_cfu = CarryForwardUnits::from(vec![(0u32, vec!["5"])]);
    let closing = summary_state(unit_map(0, 10), closing_cfu, 0);
    // Ledger has a DIFFERENT (empty) CFU.
    let ledger = ledger(unit_map(0, 10), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(!report.passed());
    assert!(report.balance_match);
    assert!(!report.carry_forward_units_match);
    assert!(report
        .mismatches
        .iter()
        .any(|m| m.contains("carry-forward units mismatch")));
}

#[test]
fn ledger_half_alone_does_not_pass_without_the_agreement_cross_check() {
    // B49 reversed the old "agreement count is not a verify field" stance:
    // once `get_opened_agreement_state` exists the cross-check is a genuine
    // two-source comparison, so the LEDGER half alone must NOT pass —
    // `verify_against_ledger` leaves `agreement_state_match` false, and only
    // `fetch_and_compare` (which reads the extern) can raise it. Regression
    // guard against reverting to the balance+CFU-only pass.
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 3);
    let ledger = ledger(unit_map(0, 10), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(report.balance_match && report.carry_forward_units_match);
    assert!(
        !report.passed(),
        "the agreement cross-check is required for a full pass"
    );
}

#[test]
fn multiple_mismatches_all_reported() {
    // Both independent on-chain-recomputed fields differ → two report lines.
    let closing_cfu = CarryForwardUnits::from(vec![(0u32, vec!["5"])]);
    let closing = summary_state(unit_map(0, 100), closing_cfu, 3);
    let ledger = ledger(unit_map(0, 1), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(!report.passed());
    assert!(!report.balance_match);
    assert!(!report.carry_forward_units_match);
    assert_eq!(report.mismatches.len(), 2);
}

/// B49 — the agreement cross-check: committed state matching the carried
/// section AND the migration identity passes; a count/hash divergence, an
/// identity mismatch, or a not-migrated read each fail with a named mismatch.
#[test]
fn agreement_state_cross_check() {
    let closing = summary_state(unit_map(0, 5), CarryForwardUnits::new(), 2);
    // `payload(3, ..)` carries agent(3), source dna(1), target dna(2) — the
    // identity the matching opened state below mirrors.
    let package = payload(3, closing);
    let mut carried: Vec<holo_hash::ActionHash> = package
        .closing_state
        .agreement_carry_forward
        .iter()
        .map(|c| c.smart_agreement_hash.clone())
        .collect();
    carried.sort();

    let matching = OpenedAgreementState {
        agent_pubkey: agent(3),
        source_dna_hash: dna(1),
        target_dna_hash: dna(2),
        agreement_hashes: carried.clone(),
    };
    let (ok, mismatches) = verify_agreement_state(&package, Some(&matching));
    assert!(ok, "matching committed state must pass: {mismatches:?}");

    // A truncated committed section (the open somehow applied 1 of 2).
    let truncated = OpenedAgreementState {
        agreement_hashes: carried[..1].to_vec(),
        ..matching.clone()
    };
    let (ok, mismatches) = verify_agreement_state(&package, Some(&truncated));
    assert!(!ok);
    assert!(
        mismatches.iter().any(|m| m.contains("agreement-count")),
        "must name the count mismatch: {mismatches:?}"
    );

    // A count-preserving substitution still mismatches on the hash set.
    let mut swapped_hashes = carried.clone();
    swapped_hashes[0] = action_hash(200);
    swapped_hashes.sort();
    let swapped = OpenedAgreementState {
        agreement_hashes: swapped_hashes,
        ..matching.clone()
    };
    let (ok, mismatches) = verify_agreement_state(&package, Some(&swapped));
    assert!(!ok);
    assert!(
        mismatches.iter().any(|m| m.contains("agreement-hash")),
        "must name the hash mismatch: {mismatches:?}"
    );

    // IDENTITY mismatch: the SAME carried hash set but a different agent /
    // source / target must NOT pass — otherwise a response for another
    // migration could raise `safe_to_teardown`.
    for wrong in [
        OpenedAgreementState {
            agent_pubkey: agent(9),
            ..matching.clone()
        },
        OpenedAgreementState {
            source_dna_hash: dna(7),
            ..matching.clone()
        },
        OpenedAgreementState {
            target_dna_hash: dna(7),
            ..matching.clone()
        },
    ] {
        let (ok, mismatches) = verify_agreement_state(&package, Some(&wrong));
        assert!(!ok, "an identity mismatch must fail: {mismatches:?}");
        assert!(
            mismatches.iter().any(|m| m.contains("identity mismatch")),
            "must name the identity mismatch: {mismatches:?}"
        );
    }

    // The new chain reporting not-migrated is a mismatch by definition.
    let (ok, mismatches) = verify_agreement_state(&package, None);
    assert!(!ok);
    assert!(mismatches
        .iter()
        .any(|m| m.contains("no opened agreement state")));
}
