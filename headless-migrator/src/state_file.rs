//! The machine-readable progress file every supervised service writes (and the
//! `Status` command reads back) — the contract the `automation/` report
//! collector (`make migrate-status`) aggregates per agent. Written atomically
//! (temp + rename) so a concurrent reader never sees a half-written file.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Which supervised phase wrote this file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Close,
    Open,
    Verify,
    Status,
}

/// Coarse progress within a phase — enough for the collector to render a
/// per-agent status without parsing free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// Probing chain / app state on (re)start.
    Probing,
    /// Old chain: clearing owed fees before preparing the summary.
    DroppingFees,
    /// Old chain: collecting M-of-N notary signatures.
    CollectingSignatures,
    /// Old chain: committing the close + close_chain.
    Closing,
    /// New server: waiting for the migration package to gossip / be fetchable.
    WaitingForPackage,
    /// New server: installing the app for the carried key.
    Installing,
    /// New server: driving `init` via the first zome call (the install carried
    /// the package as `init_properties`, so `init` opens the chain).
    OpeningChain,
    /// Verifying the new-chain ledger against the close summary.
    Verifying,
    /// The phase reached its terminal success state.
    Done,
    /// The phase hit a hard stop (e.g. warrants, verify mismatch) — not
    /// retryable; the operator must intervene.
    Failed,
}

/// The prior on-disk `safe_to_teardown` latch as the write guard sees it — the
/// three-way distinction the monotonic latch needs but the reporting read
/// ([`State::persisted_safe_to_teardown`]) collapses. Only the write path cares
/// that a corrupt file is NOT an absent one.
enum PriorRecord {
    /// No state file yet — a genuinely first write; persist the caller's value.
    Absent,
    /// The file read + parsed; carries the two write-guarded fields: the
    /// monotonic `safe_to_teardown` latch and the write-once
    /// `gd_wait_started_us` stamp.
    Known {
        safe_to_teardown: bool,
        gd_wait_started_us: Option<i64>,
    },
    /// The file is present but could not be read / parsed — the prior guarded
    /// fields are UNKNOWN. Carries the read error so the caller can surface it.
    Unreadable(anyhow::Error),
}

/// The full progress record. `safe_to_teardown` is set once close + open +
/// verify are all green for this agent — the only teardown signal (the operator
/// destroys droplets manually).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub phase: Phase,
    pub step: Step,
    /// The agent this record is about (`AgentPubKeyB64`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Probe flags — the three questions the spec's `Status` answers.
    pub old_chain_closed: bool,
    /// Close-side only: `true` when the old-chain probe could NOT determine
    /// closed-ness (the conductor was unreachable / errored), so `old_chain_closed`
    /// is `false` because it is UNKNOWN, not because the chain is definitively
    /// still open. Lets the report (and a human) tell "couldn't check" from
    /// "checked, not closed" without overloading the `bool`. Omitted (defaults
    /// `false`) on every record where the answer is known.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub old_chain_closed_unknown: bool,
    pub package_fetchable: bool,
    pub new_chain_opened: bool,
    /// Signatures collected so far / needed (close phase).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signatures_collected: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signatures_threshold: Option<u32>,
    /// Verify per-field outcome, when a verify has run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyReport>,
    /// True only when close + open + verify are all green for this agent.
    pub safe_to_teardown: bool,
    /// Human-readable latest detail (also logged to journald).
    pub message: String,
    /// Wall-clock of this write (µs since epoch) so a stale file is detectable.
    pub updated_at_us: i64,
    /// Wall-clock (µs since epoch) of the FIRST too-early `init` (successor GD not
    /// yet in effect) in this migration. The open service is a supervised
    /// `Restart=on-failure` unit, so the GD-wait deadline must survive a restart —
    /// a monotonic `Instant` would reset each start, letting a never-arriving
    /// successor GD retry forever. Seeded from the prior record so the budget is
    /// measured from the first too-early across all restarts, not per-process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gd_wait_started_us: Option<i64>,
}

/// Per-field result of the close-summary ⇄ new-chain-ledger comparison. The two
/// fields are the only values the new chain recomputes *independently* of the
/// carried package (it opened with them as its opening state); the carried
/// agreement-state section is verified on-chain by the DNA's `init`, not here
/// (see [`crate::verify`] module docs), so it has no field of its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VerifyReport {
    pub balance_match: bool,
    pub carry_forward_units_match: bool,
    /// The B49 cross-check: the new chain's COMMITTED agreement state (read
    /// via `get_opened_agreement_state`) matches the fetched close package's
    /// carry-forward section — count and per-agreement hashes. `serde(default)`
    /// keeps pre-existing persisted reports decoding (they never had the
    /// field; their latch was already sealed at write time).
    #[serde(default)]
    pub agreement_state_match: bool,
    /// Per-field human detail for any mismatch (empty ⇒ all matched).
    pub mismatches: Vec<String>,
}

impl VerifyReport {
    pub fn passed(&self) -> bool {
        self.balance_match && self.carry_forward_units_match && self.agreement_state_match
    }
}

impl State {
    /// A fresh record for `phase` at `step`, all probe flags false.
    pub fn new(phase: Phase, step: Step, message: impl Into<String>) -> Self {
        Self {
            phase,
            step,
            agent: None,
            old_chain_closed: false,
            old_chain_closed_unknown: false,
            package_fetchable: false,
            new_chain_opened: false,
            signatures_collected: None,
            signatures_threshold: None,
            verify: None,
            safe_to_teardown: false,
            message: message.into(),
            updated_at_us: now_us(),
            gd_wait_started_us: None,
        }
    }

    pub fn with_agent(mut self, agent: Option<String>) -> Self {
        self.agent = agent;
        self
    }

    /// Stamp the write time and persist atomically (temp file + rename) so the
    /// report collector never reads a partial write.
    ///
    /// **Monotonic-latch guard.** `safe_to_teardown` is a one-way latch: once any
    /// run has proven close + open + verify all green and written `true`, no later
    /// write may lower it back to `false`. So every write read-modify-writes that
    /// one field — if the on-disk record already says `true`, the new record is
    /// forced to `true` regardless of what the caller passed. This makes the latch
    /// robust to a fresh `State` (a cross-process restart, the standalone `verify`
    /// command, or a `status` probe) that would otherwise clobber the prior `true`
    /// with its default `false`. Callers should also seed from the prior value
    /// ([`seed_from_persisted`]) so the in-memory record matches; this guard is the
    /// defensive backstop that holds even if a caller forgets to.
    ///
    /// **Write-once stamp guard.** `gd_wait_started_us` records the FIRST
    /// too-early `init` (the successor GD not yet in effect) so the open
    /// service's bounded GD-wait budget is measured across supervised restarts,
    /// not per process. A fresh `State` defaults it to `None`, so a writer that
    /// never saw the stamp (a `status` report, the standalone `verify`) would
    /// erase it — and the next open-service restart would renew the full
    /// budget, reopening the unbounded-retry hole the stamp closes. So a `None`
    /// in the record is filled from the prior on-disk stamp; a caller with its
    /// own `Some` (the open service, seeded first) wins. Nothing clears the
    /// stamp within a migration — the file itself is retired with the droplet.
    ///
    /// **Read-error fail-safe.** The guard reads the prior on-disk record to learn
    /// the latch, so it must tell a genuinely-ABSENT file (a first write — fine to
    /// persist whatever the caller passed) from a file that is PRESENT but
    /// unreadable / corrupt (an UNKNOWN prior latch). A corrupt file might hold a
    /// latched `true` we cannot read, so a fresh `false` written over it would
    /// silently drop that latch. So when the prior file is present-but-unreadable
    /// **and** this write would lower the latch (`safe_to_teardown == false`), the
    /// write **fails closed**: it refuses to overwrite the corrupt record, leaves
    /// it intact for inspection, and returns the read error rather than treating
    /// the unknown prior as a fresh `false`. Raising the latch (`true`) over a
    /// corrupt file is always allowed — it cannot lower anything and replaces the
    /// corruption with a valid record.
    pub fn write(&self, path: &Path) -> Result<()> {
        let mut record = self.clone();
        match Self::read_prior_record(path) {
            PriorRecord::Known {
                safe_to_teardown,
                gd_wait_started_us,
            } => {
                // A prior latch that is up is held up (monotonic); a prior
                // `false` persists the caller's value as-is (a legitimate
                // false→true raise included).
                if safe_to_teardown {
                    record.safe_to_teardown = true;
                }
                // Write-once stamp: fill a fresh record's `None` from the
                // prior stamp so an interleaved status/verify write can't
                // renew the open service's bounded GD-wait budget.
                if record.gd_wait_started_us.is_none() {
                    record.gd_wait_started_us = gd_wait_started_us;
                }
            }
            // A genuinely new file → persist the caller's value as-is.
            PriorRecord::Absent => {}
            // Present but unreadable → the prior latch is UNKNOWN. Refuse to
            // lower it: fail closed on a `false` write so a corrupt `true`
            // can't be dropped. (The stamp rides the same fail-closed path; a
            // latch-raising `true` write over corruption proceeds and repairs
            // the file, accepting that an unreadable stamp is lost with it —
            // by then the open has verified, so no GD wait remains.)
            PriorRecord::Unreadable(err) => {
                if !record.safe_to_teardown {
                    return Err(err.context(format!(
                        "refusing to lower safe_to_teardown: prior state file {} is present \
                         but unreadable, so its latch is unknown (fail closed)",
                        path.display()
                    )));
                }
                // A `true` write is a raise — safe to proceed and repair the file.
            }
        }
        record.updated_at_us = now_us();
        let json = serde_json::to_string_pretty(&record).context("serializing state file")?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating state dir {}", parent.display()))?;
            }
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }

    /// Read a previously written record back (the `Status` command, and the
    /// report collector).
    pub fn read(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    /// Read the persisted `safe_to_teardown` signal from the state file at
    /// `path`, defaulting to `false` if the file is absent or unreadable.
    ///
    /// This is the **authoritative, monotonic** verify outcome: the open service
    /// writes `safe_to_teardown = true` exactly once, when verify passes (while
    /// the old side is still up — verify needs the old-side router fetch). Both
    /// the open service's idempotent already-migrated path and the `Status`
    /// command read it back here rather than re-running a live verify, which can
    /// only run *during* the migration and would (incorrectly) flip the signal
    /// false on a later restart once the operator has torn down the old side.
    /// Once true, it stays true. The one home for "has this agent verified?".
    ///
    /// This is the **reporting** read — a known `true` is the only "yes"; an
    /// absent *or* unreadable file reads `false`, the conservative default
    /// (report not-safe rather than guess). The *write*-side latch guard does NOT
    /// reuse this collapse — it must distinguish absent from corrupt to avoid
    /// dropping a latch it cannot read (see [`read_prior_record`](Self::read_prior_record)).
    pub fn persisted_safe_to_teardown(path: &Path) -> bool {
        Self::read(path)
            .map(|s| s.safe_to_teardown)
            .unwrap_or(false)
    }

    /// Classify the prior on-disk record's write-guarded fields (the teardown
    /// latch + the GD-wait stamp) for the write-side fail-safe, telling a
    /// genuinely-ABSENT file apart from a PRESENT-but-corrupt one. Unlike
    /// [`persisted_safe_to_teardown`](Self::persisted_safe_to_teardown) — which
    /// collapses both to `false` for *reporting* — the write guard must not, since
    /// a corrupt file may hold a latched `true` it cannot read and a fresh `false`
    /// written over it would silently drop that latch.
    fn read_prior_record(path: &Path) -> PriorRecord {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PriorRecord::Absent,
            Err(e) => PriorRecord::Unreadable(
                anyhow::Error::new(e).context(format!("reading {}", path.display())),
            ),
            Ok(raw) => match serde_json::from_str::<State>(&raw) {
                Ok(s) => PriorRecord::Known {
                    safe_to_teardown: s.safe_to_teardown,
                    gd_wait_started_us: s.gd_wait_started_us,
                },
                Err(e) => PriorRecord::Unreadable(
                    anyhow::Error::new(e).context(format!("parsing {}", path.display())),
                ),
            },
        }
    }

    /// Seed this (fresh) in-memory record from the prior persisted state at
    /// `path`, carrying forward the **monotonic** `safe_to_teardown` latch and the
    /// last `verify` detail. The single home for "a new run must not start below
    /// what a prior run already proved": a cross-process restart of the open
    /// service, the standalone `verify` command, and the `status` probe all begin
    /// from a fresh `State` (default `safe_to_teardown = false`), which — written
    /// as-is — would clobber a prior `true` on disk. Seeding first keeps the
    /// in-memory loop's idempotent short-circuit (`AlreadyOpened` → `Done`)
    /// checking the SAME value the file holds, rather than a default it would have
    /// to re-read (and re-clobber) to recover. The write-side guard in
    /// [`write`](Self::write) is the defensive backstop; this is the primary path.
    ///
    /// `verify` is carried only when the prior record had one (`None` never
    /// overwrites a value the caller may have set). A missing / unreadable file is
    /// a no-op (a first run has nothing to seed from).
    pub fn seed_from_persisted(&mut self, path: &Path) {
        if let Ok(prior) = Self::read(path) {
            if prior.safe_to_teardown {
                self.safe_to_teardown = true;
            }
            if self.verify.is_none() {
                self.verify = prior.verify;
            }
            // The GD-wait deadline is measured from the FIRST too-early across all
            // supervised restarts, so carry the persisted stamp forward — else a
            // never-arriving successor GD would reset the budget every restart.
            if self.gd_wait_started_us.is_none() {
                self.gd_wait_started_us = prior.gd_wait_started_us;
            }
        }
    }
}

pub(crate) fn now_us() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
