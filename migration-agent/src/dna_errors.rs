//! The migration error-substring contract, in ONE place.
//!
//! `rave_engine 0.4.0` and the alliance transactor DNA expose **no typed
//! migration error enum** — a rejected `migration_init` / `get_migration_close_state`
//! surfaces only as a stringly-rendered conductor error, and the router returns
//! a string `code`. Classifying those into actions therefore means matching
//! substrings, which is fragile: a DNA reword silently reclassifies an error.
//!
//! To keep that fragility auditable, every substring the close service, open
//! service, and package fetch key off lives here as one table, next to the
//! exact validator/router source it mirrors — rather than three lists drifting
//! apart across `open.rs`, `probe.rs`, and `fetch.rs`. The substrings below are
//! copied from:
//!   * the alliance integrity `validate_opening_state_summary` +
//!     `verify_notary_threshold` + `validate_carry_forward_structure`
//!     (`dnas/alliance/zomes/integrity/transactor/src/entries/migration/`),
//!   * the coordinator `migration_init` double-migration guard
//!     (`.../coordinator/transactor/src/migration/open.rs`),
//!   * the alliance `get_migration_close_state` close-state messages, and
//!   * the router's wire error codes (`migration-service/router`).
//!
//! DNA-OWNER BACKLOG: expose typed migration-init / close errors (a
//! `#[derive]`d error enum on the zome surface) so this substring contract can
//! be replaced by a match on variants. Until then, any change to a validator
//! message MUST be mirrored here.

/// How a failed `migration_init` should be handled by the open service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitErrorClass {
    /// The open validator rejected a non-fresh chain (a zome call landed before
    /// `migration_init`, leaving a non-zero balance / owed fees) — uninstall,
    /// reinstall, retry. Recoverable: nothing of value is on that cell.
    NonFreshChain,
    /// The double-migration guard fired — another pass already opened the chain.
    /// Treat as success-adjacent: re-verify.
    AlreadyMigrated,
    /// A terminal `Invalid` verdict that retrying can never fix — the carried
    /// key doesn't match the notarized agent, the carried signatures don't meet
    /// the new GD's opening threshold (or don't verify, or aren't from listed
    /// notaries), or the carry-forward section is malformed. Fail loudly.
    HardFailure,
    /// Anything else (a websocket blip, a transient host error) — back off and
    /// re-probe.
    Transient,
}

/// Classify a `migration_init` error from its rendered chain.
///
/// Order matters: a terminal hard-failure verdict is checked **before** the
/// broad `"already migrated"` / fresh-chain tokens, so a genuinely unfixable
/// `Invalid(...)` that happens to mention "already migrated" (or any other
/// recoverable token) is never shadowed into a re-verify / reinstall loop. The
/// recoverable / success-adjacent cases come next, and `Transient` is the
/// fallthrough — so an unfixable verdict is never mistaken for a transient blip
/// (which would retry forever on, e.g., a wrong carried key).
pub fn classify_migration_init_error(rendered: &str) -> InitErrorClass {
    let r = rendered.to_lowercase();
    if is_migration_init_hard_failure(&r) {
        InitErrorClass::HardFailure
    } else if r.contains("already been migrated") || r.contains("already migrated") {
        InitErrorClass::AlreadyMigrated
    } else if is_non_fresh_chain(&r) {
        InitErrorClass::NonFreshChain
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

/// Terminal `Invalid` verdicts from the opening-summary validator + the notary
/// threshold check — an unfixable payload/key/signature problem. Mirrors the
/// exact `ValidateCallbackResult::Invalid(...)` strings in the integrity zome.
/// Each token is anchored to the fuller, distinctive validator phrase so a
/// short, ambiguous fragment can't shadow a transient error.
fn is_migration_init_hard_failure(r_lower: &str) -> bool {
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
    rendered.contains("No closing state summary found")
        || rendered.contains("no CloseChain action found")
}

/// Whether a router error `code` is a genuine hard stop for the migration. The
/// rest — crucially including `no_close_found` (which, after a known close, can
/// only be propagation lag for a headless restoring agent) — are "keep
/// waiting". Mirrors the router's wire codes (a different namespace from the
/// DNA validator strings above, but the same fragile string contract, so it
/// shares this home).
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
        | "to_is_chain_root"
        | "not_registered_predecessor"
    )
}
