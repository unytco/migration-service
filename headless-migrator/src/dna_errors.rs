//! The migration error contract, in ONE place — typed code first, substring
//! fallback second.
//!
//! `rave_engine`'s `MigrationError` renders a machine-extractable
//! `[MIGERR:<CODE>]` prefix on every DNA-side migration verdict, and
//! `MigrationError::from_rendered` recovers the variant from a
//! conductor-wrapped string. Every classifier below matches **the code, not
//! the English text**, so a validator message reword can never silently
//! reclassify. The substring tables remain only as the fallback for the
//! surfaces that carry no tag:
//!
//!   * the coordinator's untagged too-early wrapper ("Could not resolve a
//!     successor GlobalDefinition at init") + the GD lookup's "No Global
//!     Definition found" (`.../progenitor_calls/global_definition.rs`),
//!   * transport / conductor errors that never came from a validator, and
//!   * the router's wire error codes (`migration-service/migration-router`) — a separate
//!     string namespace that shares this home, unchanged.
//!
//! Any change to an UNTAGGED message must still be mirrored here; tagged
//! messages may reword freely.

use rave_engine::types::entries::migration::MigrationError;

/// Map a typed DNA migration error to the open service's class. Exhaustive on
/// purpose: a future variant fails compilation here instead of drifting into a
/// default. Close-side codes cannot legitimately surface from an `init` — an
/// anomaly is a HARD stop (fail loud), never an unbounded retry.
fn init_class_of(code: MigrationError) -> InitErrorClass {
    use MigrationError::*;
    match code {
        OpeningSummaryUpdateForbidden
        | KeyDoesNotMatchNotarizedAgent
        | TargetDnaMismatch
        | CarryForwardMalformed
        | SourceNotAcceptedPredecessor
        | MigrationDisabled
        | DuplicateNotarySigner
        | SelfNotarizedClose
        | NotaryNotConfigured
        | SignatureDoesNotVerify
        | NotaryThresholdNotMet => InitErrorClass::HardFailure,
        AlreadyMigrated => InitErrorClass::AlreadyMigrated,
        NonFreshChain => InitErrorClass::NonFreshChain,
        ClosingSummaryUpdateForbidden
        | CloseAuthorMismatch
        | CloseSourceDnaMismatch
        | StaleClose
        | CloseTargetNotUpgradeTarget
        | CloseSummaryMismatch
        | NoClosingSummary
        | NoCloseChainAction => InitErrorClass::HardFailure,
    }
}

/// How a failed open (`init`) should be handled by the open service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitErrorClass {
    /// The open validator rejected a non-fresh chain at `init` — the chain being
    /// opened already carries a non-zero balance / owed fees. With the chain
    /// opened at genesis this is anomalous (not the old pre-`init` zome-call
    /// race), so it is a **hard stop**, not a recoverable reinstall.
    NonFreshChain,
    /// The double-migration guard fired — another pass already opened the chain.
    /// Treat as success-adjacent: re-verify.
    AlreadyMigrated,
    /// A terminal `Invalid` verdict that retrying can never fix — the carried
    /// key doesn't match the notarized agent, the carried signatures don't meet
    /// the new GD's opening threshold (or don't verify, or aren't from listed
    /// notaries), or the carry-forward section is malformed. Fail loudly.
    HardFailure,
    /// The successor `GlobalDefinition` `init` needs to open the chain is not yet
    /// in effect (not gossiped in, or before its effective date). Recoverable —
    /// the open service re-drives `init` once the GD syncs — but under a BOUNDED
    /// deadline, since the classifier can't tell "not yet" from "never".
    TooEarly,
    /// Anything else (a websocket blip, a transient host error) — back off and
    /// re-probe.
    Transient,
}

/// Classify an open (`init`) error from its rendered chain.
///
/// Order matters: a terminal hard-failure verdict is checked **before** the
/// broad `"already migrated"` / fresh-chain tokens, so a genuinely unfixable
/// `Invalid(...)` that happens to mention "already migrated" (or any other
/// recoverable token) is never shadowed into a re-verify / reinstall loop. The
/// recoverable / success-adjacent cases come next, and `Transient` is the
/// fallthrough — so an unfixable verdict is never mistaken for a transient blip
/// (which would retry forever on, e.g., a wrong carried key).
pub fn classify_migration_init_error(rendered: &str) -> InitErrorClass {
    if let Some(code) = MigrationError::from_rendered(rendered) {
        return init_class_of(code);
    }
    let r = rendered.to_lowercase();
    if is_migration_init_hard_failure(&r) {
        InitErrorClass::HardFailure
    } else if r.contains("already been migrated") || r.contains("already migrated") {
        InitErrorClass::AlreadyMigrated
    } else if is_non_fresh_chain(&r) {
        InitErrorClass::NonFreshChain
    } else if is_successor_gd_not_in_effect(&r) {
        InitErrorClass::TooEarly
    } else {
        InitErrorClass::Transient
    }
}

/// The open integrity validator rejects a chain that isn't fresh (non-zero
/// balance or owed fees): the full validator phrase is "The Summary can only be
/// added to a fresh chain". Anchored to that distinctive wording — the bare
/// token `"fresh"` matches unrelated/transient errors ("connection refreshed",
/// "stale snapshot, fetching fresh") and would misclassify a recoverable blip
/// as a non-fresh-chain reinstall — plus the older `"source chain not empty"`
/// phrasing as belt-and-braces.
fn is_non_fresh_chain(r_lower: &str) -> bool {
    r_lower.contains("added to a fresh chain") || r_lower.contains("source chain not empty")
}

/// The DNA's `init` could not resolve a successor `GlobalDefinition` to open the
/// chain against — it is not yet in effect (not gossiped in, or before its
/// effective date): a too-early install. Distinct from a generic transient blip
/// because the open service bounds the retry by a deadline (the GD might never
/// come). Mirrors the wrapper `apply_migration_init_properties` puts on the GD
/// lookup ("Could not resolve a successor GlobalDefinition at init") plus the
/// underlying "No Global Definition found".
fn is_successor_gd_not_in_effect(r_lower: &str) -> bool {
    // Anchored to the two distinctive phrases (verified in the alliance DNA:
    // `migration/open.rs` wrapper + `progenitor_calls/global_definition.rs` lookup)
    // — NOT the bare `"successor globaldefinition"` token, which would also swallow
    // a *malformed* / mis-configured successor GD (a hard failure) into the bounded
    // TooEarly retry until the deadline expires.
    r_lower.contains("could not resolve a successor globaldefinition")
        || r_lower.contains("no global definition found")
}

/// Terminal `Invalid` verdicts from the opening-summary validator, the notary
/// threshold check, and `genesis_self_check`'s membrane-proof gate — an unfixable
/// payload/key/signature problem. Mirrors the exact
/// `ValidateCallbackResult::Invalid(...)` strings in the integrity zome. Each
/// token is anchored to the fuller, distinctive validator phrase so a short,
/// ambiguous fragment can't shadow a transient error.
fn is_migration_init_hard_failure(r_lower: &str) -> bool {
    // `genesis_self_check` → `check_membrane_proof` (alliance integrity zome,
    // `mem_proof.rs`). These became REACHABLE on this path only once the install
    // started applying the network's DNA properties: the gate is skipped entirely
    // while `joining_server_signer` is None, which is exactly what a
    // property-less (isolated) install produced. A rejected proof is terminal —
    // the joining service must issue a new one — so it must never land on the
    // unbounded transient retry, which would spin forever emitting nothing but
    // "transient failure, retrying".
    if r_lower.contains("membrane proof required")
        || r_lower.contains("signer is not the authorized progenitor")
        || r_lower.contains("membrane proof is not for this agent")
        || r_lower.contains("membrane proof signature is invalid")
    {
        return true;
    }
    // `validate_opening_state_summary`: the carried key isn't the notarized agent.
    r_lower.contains("does not match the notarized agent")
        // `verify_notary_threshold`: not enough valid signatures. The full
        // verdict is "...valid notary signature(s); the threshold is N" —
        // anchored to the distinctive "valid notary signature" so the bare
        // "the threshold is" can't match an unrelated threshold message.
        || r_lower.contains("valid notary signature")
        // ...a signature that doesn't verify over the payload.
        || r_lower.contains("does not verify over the payload")
        // ...a signer not in the GD's opening-notary list.
        || r_lower.contains("not a configured notary")
        // ...a duplicate notary signer.
        || r_lower.contains("duplicate notary signer")
        // ...the agent tried to notarise its own close.
        || r_lower.contains("cannot notarise its own close")
        // ...migration disabled in this direction (empty list / zero threshold).
        || r_lower.contains("migration is disabled in this direction")
        // `validate_carry_forward_structure`: section too large / duplicate keys.
        || r_lower.contains("carry-forward section")
        // An update to the opening summary is never allowed.
        || r_lower.contains("opening state summary update is not allowed")
        // Single-landing reject (M13): the close's target_dna_hash != the DNA
        // being opened. Lowercase `dna` — the verdict is "...names a different
        // target DNA" and the input is lowercased before matching.
        || r_lower.contains("names a different target dna")
        // The close's source_dna_hash has no entry in the target GD's
        // opening_predecessors (M13).
        || r_lower.contains("is not an accepted predecessor")
}

/// Whether a close-side error — from `prepare_closing_summary`'s pre-check or the
/// close validator — is a terminal target-binding fault: the configured `to_dna`
/// is not in the source GD's `upgrade_targets`, so no amount of retrying fixes it
/// (unlike propagation lag / a transient blip, which the close loop retries
/// forever). Mirrors M13's two strings; lowercased internally so the caller may
/// pass the raw rendered error.
pub fn is_close_target_hard_failure(rendered: &str) -> bool {
    if let Some(code) = MigrationError::from_rendered(rendered) {
        return code == MigrationError::CloseTargetNotUpgradeTarget;
    }
    let r = rendered.to_lowercase();
    // `prepare_closing_summary` pre-check: "target DNA {:?} is not in this network's upgrade_targets".
    r.contains("is not in this network's upgrade_targets")
        // close validator: "Close target is not in this DNA's upgrade_targets".
        || r.contains("close target is not in this dna's upgrade_targets")
}

/// The non-closed close states the close-side probe must distinguish from a
/// rendered `get_migration_close_state` error. A missing-`CloseChain` error
/// means the summary IS committed but the chain isn't closed (partial);
/// anything else (no summary at all, a transport error) is treated as a plain
/// open chain that the next supervised pass re-probes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseErrorClass {
    /// Summary committed, `CloseChain` not yet observed — finish the close.
    PartialClose,
    /// No summary (or a transport error) — (re)prepare from an open chain.
    Open,
}

/// Classify a `get_migration_close_state` error string. The
/// `"no CloseChain action found on chain"` contract is the alliance close
/// surface's; an open chain says `"No closing state summary found"`.
pub fn classify_close_error(rendered: &str) -> CloseErrorClass {
    match MigrationError::from_rendered(rendered) {
        Some(MigrationError::NoCloseChainAction) => return CloseErrorClass::PartialClose,
        Some(_) => return CloseErrorClass::Open,
        None => {}
    }
    if rendered.contains("no CloseChain action found") {
        CloseErrorClass::PartialClose
    } else {
        CloseErrorClass::Open
    }
}

/// Whether a `get_migration_close_state` error string is a *recognized DNA
/// close-state response* (the conductor was reached and the chain definitively
/// has no committed close yet) rather than a transport / unexpected failure
/// (which leaves the close state UNKNOWN). The close **service** treats every
/// non-closed error as "open, re-probe" (safe — its actions are idempotent), but
/// the **status report** must not present an unreachable conductor as a definitive
/// `old_chain_closed = false`; this predicate is what lets it distinguish the two.
/// Mirrors the same two alliance close-surface strings `classify_close_error`
/// keys off: `"No closing state summary found"` (plain open) and
/// `"no CloseChain action found on chain"` (partial close).
pub fn is_recognized_close_state_response(rendered: &str) -> bool {
    if let Some(code) = MigrationError::from_rendered(rendered) {
        return matches!(
            code,
            MigrationError::NoClosingSummary | MigrationError::NoCloseChainAction
        );
    }
    rendered.contains("No closing state summary found")
        || rendered.contains("no CloseChain action found")
}

/// Whether a router error `code` is a genuine hard stop for the migration —
/// a fault no amount of retrying will fix. Mirrors the router's wire codes (the
/// `ErrorCode` union in `migration-service/migration-router/src/errors.ts`; a different
/// namespace from the DNA validator strings above, but the same fragile string
/// contract, so it shares this home). Keep this set + [`router_code_is_retryable`]
/// in sync with that union: every code is exactly one of the two.
pub fn router_code_is_hard_stop(code: &str) -> bool {
    matches!(
        code,
        // The agent's chain carries warrants — migration cannot proceed.
        "warranted"
        // Malformed request or a DNA pair the registry rejects — a config fault
        // that retrying will never fix.
        | "bad_request"
        | "unknown_to_dna"
        | "unknown_from_dna"
        | "unknown_current_dna"
        | "to_is_chain_root"
        | "unreachable_target"
    )
}

/// Whether a router error `code` is a *recognized, genuinely transient* fault —
/// propagation lag, a momentary outage, or a rate limit — for which the headless
/// restoring agent should keep waiting and re-fetch. The crux is `no_close_found`:
/// AFTER a known close it can only be propagation lag, never a fresh-agent fallback.
///
/// This is an explicit ALLOWLIST, not the complement of [`router_code_is_hard_stop`]:
/// a code the router adds that this agent has never seen is, by default, NOT
/// retryable — it is surfaced as a hard stop by the caller rather than silently
/// retried forever, so a drifted/extended wire contract fails loud instead of
/// hanging. Mirrors the router's `ErrorCode` union; keep the two sets in sync.
pub fn router_code_is_retryable(code: &str) -> bool {
    matches!(
        code,
        // Propagation lag for a headless restoring agent after a known close.
        "no_close_found"
        // Notaries momentarily unreachable / unable to attest — re-fetch later.
        | "all_orgs_unhealthy"
        | "unable_to_verify"
        // The router's own internal/transport error — our fault, retry.
        | "internal"
        // Auth / rate limiting — momentary; back off and retry.
        | "auth_failed"
        | "rate_limited"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_codes_classify_regardless_of_wording() {
        // The point of the typed contract: the English may reword freely, the
        // code still classifies (the old substring matching silently fell to
        // Transient on any reword).
        assert_eq!(
            classify_migration_init_error("Guest(\"[MIGERR:MIG_KEY_MISMATCH] totally reworded\")"),
            InitErrorClass::HardFailure
        );
        assert_eq!(
            classify_migration_init_error("[MIGERR:MIG_NON_FRESH_CHAIN] anything at all"),
            InitErrorClass::NonFreshChain
        );
        assert_eq!(
            classify_migration_init_error("[MIGERR:MIG_ALREADY_MIGRATED] whatever"),
            InitErrorClass::AlreadyMigrated
        );
    }

    #[test]
    fn typed_close_codes_drive_the_close_classifiers() {
        assert!(is_close_target_hard_failure(
            "[MIGERR:MIG_CLOSE_TARGET_NOT_UPGRADE_TARGET] reworded entirely"
        ));
        // A recognized non-target tag answers definitively: not a target fault.
        assert!(!is_close_target_hard_failure(
            "[MIGERR:MIG_STALE_CLOSE] the chain moved"
        ));
        assert_eq!(
            classify_close_error("[MIGERR:MIG_NO_CLOSE_CHAIN_ACTION] partial close"),
            CloseErrorClass::PartialClose
        );
        assert_eq!(
            classify_close_error("[MIGERR:MIG_NO_CLOSING_SUMMARY] open chain"),
            CloseErrorClass::Open
        );
        assert!(is_recognized_close_state_response(
            "[MIGERR:MIG_NO_CLOSING_SUMMARY] x"
        ));
        assert!(!is_recognized_close_state_response(
            "[MIGERR:MIG_STALE_CLOSE] x"
        ));
    }

    #[test]
    fn close_target_faults_are_hard_failures() {
        // The prepare pre-check + close-validator verdicts, in their as-rendered
        // (mixed-case) form — a misconfigured target must hard-stop, not loop.
        assert!(is_close_target_hard_failure(
            "target DNA DnaHash(uhC0k…) is not in this network's upgrade_targets"
        ));
        assert!(is_close_target_hard_failure(
            "Close target is not in this DNA's upgrade_targets"
        ));
        // A transient blip is NOT a target fault.
        assert!(!is_close_target_hard_failure(
            "websocket closed; reconnecting"
        ));
    }

    #[test]
    fn skip_open_rejects_are_hard_failures() {
        // M13 single-landing + unlisted-source verdicts must hard-stop the open
        // service (the classifier lowercases its input before matching).
        assert_eq!(
            classify_migration_init_error("Opening state summary names a different target DNA"),
            InitErrorClass::HardFailure
        );
        assert_eq!(
            classify_migration_init_error(
                "Source DNA DnaHash(uhC0k…) is not an accepted predecessor"
            ),
            InitErrorClass::HardFailure
        );
    }

    /// `genesis_self_check`'s membrane-proof verdicts must be TERMINAL. This path
    /// only became reachable once the install started applying the network's DNA
    /// properties (the gate is skipped while `joining_server_signer` is None), and
    /// a rejected proof is unfixable by retrying — classed transient it would spin
    /// the supervised loop forever with no diagnosis. Strings mirrored verbatim
    /// from the alliance integrity zome's `mem_proof.rs`.
    #[test]
    fn rejected_membrane_proofs_are_hard_failures_not_infinite_retries() {
        for verdict in [
            "Membrane proof required",
            "Signer is not the authorized progenitor",
            "Membrane proof is not for this agent",
            "Membrane proof signature is invalid",
        ] {
            assert_eq!(
                classify_migration_init_error(&format!(
                    "wasm error: Guest(\"InvalidCommit: {verdict}\")"
                )),
                InitErrorClass::HardFailure,
                "a rejected membrane proof must hard-stop, never retry forever: {verdict}"
            );
        }
    }

    #[test]
    fn unreachable_target_is_a_router_hard_stop() {
        assert!(router_code_is_hard_stop("unreachable_target"));
        assert!(!router_code_is_retryable("unreachable_target"));
    }

    #[test]
    fn too_early_successor_gd_is_bounded_not_unbounded() {
        // The DNA wraps the GD lookup as "Could not resolve a successor
        // GlobalDefinition at init (...)"; that classifies as TooEarly (a bounded
        // retry), NOT the unbounded Transient fallthrough.
        assert_eq!(
            classify_migration_init_error(
                "Could not resolve a successor GlobalDefinition at init (NoGlobalDefinition)"
            ),
            InitErrorClass::TooEarly
        );
        assert_eq!(
            classify_migration_init_error("wasm error: No Global Definition found"),
            InitErrorClass::TooEarly
        );
        // A generic blip is still Transient (unbounded).
        assert_eq!(
            classify_migration_init_error("websocket closed; reconnecting"),
            InitErrorClass::Transient
        );
    }
}
