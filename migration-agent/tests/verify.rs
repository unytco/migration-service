//! `Verify` per-field comparison: a match passes; each field's mismatch is
//! reported independently with a nonzero (failing) report. The comparison is
//! over the two values the new chain recomputes independently of the carried
//! package — balance and CFU; the carried agreement-state section is verified
//! on-chain by `migration_init`, not here (so there is no count field to test).

mod support;

use std::time::Duration;

use migration_agent::config::Config;
use migration_agent::policy::PolicyOpts;
use migration_agent::state_file::{Phase, State, Step, VerifyReport};
use migration_agent::verify::{build_verify_state, verify_against_ledger};
use rave_engine::types::ledger::CarryForwardUnits;
use support::*;
use zfuel::fuel::ZFuel;

fn tmp(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "migration-agent-verify-test-{}-{}.json",
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
    }
}

fn passing_report() -> VerifyReport {
    VerifyReport {
        balance_match: true,
        carry_forward_units_match: true,
        mismatches: vec![],
    }
}

fn failing_report() -> VerifyReport {
    VerifyReport {
        balance_match: false,
        carry_forward_units_match: true,
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
    let params = migration_agent::status::StatusParams {
        router_url: "http://127.0.0.1:1".into(),
        from_dna: dna_b64(1),
        to_dna: dna_b64(2),
        agent_b64: agent_b64(),
    };
    let reported = migration_agent::status::run(&cfg, Some(&params))
        .await
        .expect("status run Ok with everything down");
    assert!(
        reported.safe_to_teardown,
        "status after a passing verify still reports safe_to_teardown=true (monotonic)"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn all_fields_match_passes() {
    let closing = summary_state(unit_map(0, 100), CarryForwardUnits::new(), 2);
    // New-chain ledger mirrors the close: balance + CFU equal closing_*.
    let ledger = ledger(unit_map(0, 100), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(report.passed(), "all fields match: {:?}", report.mismatches);
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
fn agreement_count_is_not_a_verify_field() {
    // A close summary carrying any number of agreements still passes verify so
    // long as balance + CFU match: the carry-forward section's integrity is the
    // job of `migration_init`'s on-chain validator (notary signatures cover the
    // whole payload, and the validator re-checks the section's structure), NOT a
    // self-comparison here. This is the regression guard for the removed
    // tautological `committed.len() == fetched.len()` check (which could never
    // fail, since both lengths came from the one fetched package).
    let closing = summary_state(unit_map(0, 10), CarryForwardUnits::new(), 3);
    let ledger = ledger(unit_map(0, 10), CarryForwardUnits::new(), ZFuel::zero());
    let report = verify_against_ledger(&closing, &ledger);
    assert!(
        report.passed(),
        "agreement count is not a verify field: {:?}",
        report.mismatches
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
