//! Post-migration verification: the live new-chain ledger against the close
//! summary the agent carried. Balance and carry-forward units must match what
//! was closed on the old DNA; any mismatch is a per-field report and a nonzero
//! exit. The comparison is a pure function over its inputs so it is unit-tested
//! without a conductor.
//!
//! ## The agreement-state cross-check (B49)
//!
//! The carry-forward section's integrity is enforced on-chain (the notary
//! signatures cover the whole `SummaryStatePayload`, and the open validator
//! re-checks signatures + structure at author time). What verify adds is an
//! INDEPENDENT two-source comparison: the router-fetched close package on one
//! side, and what the new chain actually COMMITTED on the other, read back
//! through the DNA's `get_opened_agreement_state` extern — count and
//! per-agreement hashes both. (A count taken from the fetched package alone
//! compared a value against itself and was deliberately absent until the
//! extern existed.)

use std::path::Path;

use anyhow::{bail, Context, Result};
use holo_hash::DnaHashB64;
use rave_engine::types::entries::migration::v0_1::SummaryState;
use rave_engine::types::ledger::Ledger;

use crate::conductor::{Conductor, HamConductor, OpenedAgreementState};
use crate::config::Config;
use crate::fetch::{self, FetchOutcome};
use crate::state_file::{Phase, State, Step, VerifyReport};

/// Compare the live new-chain `ledger` against the `closing_state` the agent
/// carried (the close summary's `SummaryState`).
///
/// The new chain opened with the old chain's *closing* balance/CFU as its
/// *opening* state, so after `migration_init` (and before any new transaction)
/// the ledger's `balance` / `carry_forward_units` equal the close's
/// `closing_balance` / `closing_carry_forward_units` — an independent
/// conductor-side recomputation, which is what makes this a real cross-check
/// rather than a self-comparison. The carry-forward *section* itself is covered
/// on-chain (see the module docs), not here.
pub fn verify_against_ledger(closing_state: &SummaryState, ledger: &Ledger) -> VerifyReport {
    let mut mismatches = Vec::new();

    let balance_match = ledger.balance == closing_state.closing_balance;
    if !balance_match {
        mismatches.push(format!(
            "balance mismatch: new-chain ledger {:?} != close summary closing_balance {:?}",
            ledger.balance, closing_state.closing_balance
        ));
    }

    let carry_forward_units_match =
        ledger.carry_forward_units == closing_state.closing_carry_forward_units;
    if !carry_forward_units_match {
        mismatches.push(format!(
            "carry-forward units mismatch: new-chain ledger {:?} != close summary \
             closing_carry_forward_units {:?}",
            ledger.carry_forward_units, closing_state.closing_carry_forward_units
        ));
    }

    VerifyReport {
        balance_match,
        carry_forward_units_match,
        // The ledger halves can't see the agreement section; the caller fills
        // this from `verify_agreement_state` (fetch_and_compare does both).
        agreement_state_match: false,
        mismatches,
    }
}

/// The B49 cross-check: the fetched package's carry-forward section against
/// the new chain's COMMITTED opened agreement state. Pure over its inputs.
/// `opened == None` (chain reports not-migrated) is a mismatch by definition —
/// a verify only runs against a chain the open service believes migrated.
pub fn verify_agreement_state(
    closing_state: &SummaryState,
    opened: Option<&OpenedAgreementState>,
) -> (bool, Vec<String>) {
    let mut carried: Vec<holo_hash::ActionHash> = closing_state
        .agreement_carry_forward
        .iter()
        .map(|c| c.smart_agreement_hash.clone())
        .collect();
    carried.sort();
    match opened {
        None => (
            false,
            vec!["agreement-state mismatch: the new chain reports no opened agreement state (chain not migrated?)".to_string()],
        ),
        Some(o) => {
            let mut mismatches = Vec::new();
            if o.agreement_hashes.len() != carried.len() {
                mismatches.push(format!(
                    "agreement-count mismatch: new chain committed {} != close package carried {}",
                    o.agreement_hashes.len(),
                    carried.len()
                ));
            }
            if o.agreement_hashes != carried {
                mismatches.push(format!(
                    "agreement-hash mismatch: new chain committed {:?} != close package carried {:?}",
                    o.agreement_hashes, carried
                ));
            }
            (mismatches.is_empty(), mismatches)
        }
    }
}

/// Router coordinates for the standalone `Verify` command — it re-fetches the
/// carried close package to compare the new-chain ledger against.
pub struct VerifyParams {
    pub router_url: String,
    pub from_dna: DnaHashB64,
    pub to_dna: DnaHashB64,
    pub agent_b64: String,
}

/// Fetch the carried close package and compare it against the live new-chain
/// `ledger`, producing a [`VerifyReport`]. The single home for the
/// fetch-then-compare sequence the `Verify` command, the open service, and the
/// `Status` command's teardown gate all share — so `status` enforces exactly
/// the same close+open+**verify** invariant the open service does (an opened
/// chain that has not verified is never `safe_to_teardown`).
///
/// `Ok(None)` means the package was not fetchable (gossip lag / a transient
/// router error) — for `Status` that is simply "verify not yet provable", not a
/// failure. A hard router fault is `Err`.
pub async fn fetch_and_compare(
    conductor: &dyn Conductor,
    client: &reqwest::Client,
    router_url: &str,
    from_dna: &DnaHashB64,
    to_dna: &DnaHashB64,
    agent_b64: &str,
) -> Result<Option<VerifyReport>> {
    let package = match fetch::fetch_package(client, router_url, from_dna, to_dna, agent_b64).await
    {
        FetchOutcome::Package(p) => *p,
        FetchOutcome::KeepWaiting(_) => return Ok(None),
        FetchOutcome::HardStop(why) => bail!("close package fetch hard stop: {why}"),
    };
    let ledger = conductor
        .get_ledger()
        .await
        .context("reading new-chain ledger for verify")?;
    let opened = conductor
        .get_opened_agreement_state()
        .await
        .context("reading new-chain opened agreement state for verify")?;
    let mut report = verify_against_ledger(&package.payload.closing_state, &ledger);
    let (agreement_state_match, mut agreement_mismatches) =
        verify_agreement_state(&package.payload.closing_state, opened.as_ref());
    report.agreement_state_match = agreement_state_match;
    report.mismatches.append(&mut agreement_mismatches);
    Ok(Some(report))
}

/// Build the [`State`] record the standalone `Verify` command persists from its
/// `report`, seeding from the prior persisted state at `state_file` so the
/// monotonic teardown latch is honored. Pure (no conductor / no IO beyond reading
/// the prior file), so the latch behavior is unit-testable without a live verify.
///
/// - **Seed first:** a fresh `State` carries the prior `safe_to_teardown = true`
///   forward, so a `verify` run never lowers a latch an earlier open-service
///   success already wrote (the round-2 verify-clobber bug). `verify` always
///   supplies its own fresh `verify` report, so only the latch is carried.
/// - **A PASSING verify may RAISE the latch:** it proves the full
///   close+open+verify condition on its own — the close package was fetchable (the
///   router only serves a *committed* close ⇒ the old chain is closed), the new
///   cell is open (`new_chain_opened`), and the ledger matched.
/// - **A FAILING verify never lowers it:** it leaves the seeded (possibly prior-
///   true) value as-is; the write-side guard in [`State::write`] is the backstop.
pub fn build_verify_state(agent_b64: &str, report: &VerifyReport, state_file: &Path) -> State {
    let passed = report.passed();
    let mut state =
        State::new(Phase::Verify, Step::Verifying, "").with_agent(Some(agent_b64.to_string()));
    state.seed_from_persisted(state_file);
    state.new_chain_opened = true;
    state.verify = Some(report.clone());
    if passed {
        state.old_chain_closed = true;
        state.safe_to_teardown = true;
    }
    state.step = if passed { Step::Done } else { Step::Failed };
    state.message = if passed {
        "verify passed".into()
    } else {
        format!("verify FAILED: {}", report.mismatches.join("; "))
    };
    state
}

/// The standalone `Verify` command: fetch the carried close package, read the
/// live new-chain ledger, and compare per-field. Writes the per-field report to
/// the state file and returns `Err` on any mismatch (the binary exits nonzero).
pub async fn run(cfg: &Config, params: &VerifyParams) -> Result<()> {
    let client = fetch::http_client_for_status()?;
    let mut shutdown = ham::install_shutdown_handler();
    let conductor = HamConductor::connect(cfg, &mut shutdown)
        .await
        .context("connecting to the new conductor for verify")?;

    let report = match fetch_and_compare(
        &conductor,
        &client,
        &params.router_url,
        &params.from_dna,
        &params.to_dna,
        &params.agent_b64,
    )
    .await?
    {
        Some(r) => r,
        None => bail!("close package not yet fetchable for verify (gossip lag)"),
    };
    let passed = report.passed();

    let state = build_verify_state(&params.agent_b64, &report, &cfg.state_file);
    state.write(&cfg.state_file)?;

    if passed {
        tracing::info!("verify passed: new chain matches the close summary");
        Ok(())
    } else {
        for m in &report.mismatches {
            tracing::error!("verify mismatch: {m}");
        }
        bail!("verify failed: {}", report.mismatches.join("; "))
    }
}
