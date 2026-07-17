//! The open service: effectively the new server's install step. A supervised
//! loop that waits out gossip until the migration package is fetchable, then
//! installs the app for the carried key WITH the package as the alliance role's
//! `init_properties` (so the DNA's `init` opens the chain at genesis), drives
//! `init` via the first zome call (`verify_if_migrated`), and verifies — exiting
//! 0 only once the new chain is open and verified. Probe-first and idempotent: a
//! restart never double-opens, and a too-early `init` (the successor GD not yet
//! in effect) is re-driven under a bounded deadline. There is no post-install
//! `migration_init` call and no first-zome-call ordering window to guard.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use holo_hash::{AgentPubKey, AgentPubKeyB64, DnaHashB64};
use rave_engine::types::entries::migration::v0_1::MigrationInitRequest;

use crate::conductor::{
    assert_happ_path, decode_membrane_proof, Conductor, HamConductor, InstallSpec,
};
use crate::config::{Config, OpenConfig};
// The migration-init error classification lives in `dna_errors` (the one home
// for the fragile DNA error-substring contract); re-exported below so callers
// and tests can still reach it via `open::`.
pub use crate::dna_errors::{classify_migration_init_error, InitErrorClass};
use crate::fetch::{self, FetchOutcome};
use crate::joining::{self, LairSigner, NonceSigner};
use crate::probe::{probe_open_state, OpenState};
use crate::state_file::{Phase, State, Step, VerifyReport};
use crate::verify::verify_against_ledger;

/// The router coordinates for the package fetch, plus the carried key and lair
/// details — everything the open service needs beyond [`Config`] / [`OpenConfig`].
pub struct OpenParams {
    pub router_url: String,
    pub from_dna: DnaHashB64,
    pub to_dna: DnaHashB64,
    /// The carried agent key — already imported into the new droplet's lair by
    /// the shell's key-carry step; the open service installs the app FOR it.
    pub agent_key: AgentPubKey,
    /// Lair connection details for signing the joining-service challenge nonce.
    pub lair_url: String,
    pub lair_passphrase: String,
}

/// Supplies the two conductor connections the open loop needs, so the loop is
/// mock-drivable without a live conductor (mirroring `close::run`'s injected
/// `&dyn Conductor`). The open service is two-phase — it connects ADMIN-ONLY
/// first (the app cell doesn't exist yet, so `ham` can't attach) to probe +
/// install, then reconnects with `ham` AFTER the install to drive `init` +
/// verify — so a single conductor can't model it; this factory does. Production
/// supplies [`HamConnector`]; tests supply a mock that hands back a scripted
/// conductor for both.
#[async_trait]
pub trait Connector: Send + Sync {
    /// Admin-only connection for the pre-install probe + install.
    async fn connect_admin_only(&self) -> Result<Arc<dyn Conductor>>;
    /// `ham`-attached connection once the app cell is provisioned; `None` on
    /// shutdown / unreachable (mapped to a transient retry by the caller).
    async fn connect_ham(&self, shutdown: &mut ham::ShutdownRx) -> Option<Arc<dyn Conductor>>;
}

/// The production [`Connector`]: real `HamConductor` connections against the
/// local conductor described by [`Config`].
struct HamConnector<'a> {
    cfg: &'a Config,
}

#[async_trait]
impl Connector for HamConnector<'_> {
    async fn connect_admin_only(&self) -> Result<Arc<dyn Conductor>> {
        let c: Arc<dyn Conductor> = Arc::new(HamConductor::connect_admin_only(self.cfg).await?);
        Ok(c)
    }

    async fn connect_ham(&self, shutdown: &mut ham::ShutdownRx) -> Option<Arc<dyn Conductor>> {
        let ham = HamConductor::connect(self.cfg, shutdown).await?;
        let c: Arc<dyn Conductor> = Arc::new(ham);
        Some(c)
    }
}

/// One open attempt's outcome.
enum OpenOutcome {
    /// New chain open + verified. Exit 0.
    Done,
    /// A hard stop (verify mismatch, warranted close). Exit nonzero.
    HardStop(String),
    /// The successor GD `init` needs is not yet in effect — re-drive `init`, but
    /// under a BOUNDED deadline (the run loop gives up if it never comes).
    TooEarly(anyhow::Error),
    /// Transient — back off and re-probe (gossip lag, a conductor blip).
    Transient(anyhow::Error),
}

/// True once the successor-GD wait has exceeded its budget, measured from the
/// FIRST too-early (`started_us`, wall-clock µs). Pulled out so the restart-safe
/// deadline (persisted, not a per-process `Instant`) is unit-testable without
/// driving the full open loop.
fn gd_wait_expired(started_us: i64, now_us: i64, timeout: Duration) -> bool {
    Duration::from_micros(now_us.saturating_sub(started_us).max(0) as u64) >= timeout
}

/// The exhaustion message stamped into the state file (which the `automation`
/// rail cats out) and returned as the failing error when the successor-GD wait
/// runs out. The LEADING text is an actionable CONFIG-FAULT diagnosis, not a
/// bare genesis error: after the full budget the likeliest cause is a wrong
/// successor DNA hash / registry entry or a target Holochain+network
/// misconfiguration, so point the operator there. The raw `init` cause rides
/// along as a trailing detail. Pure (no loop / no I/O) so it is unit-testable.
fn gd_wait_exhausted_message(
    from: &DnaHashB64,
    to: &DnaHashB64,
    timeout: Duration,
    cause: &anyhow::Error,
) -> String {
    format!(
        "v2 GD never gossiped to the target within {}s — check the successor DNA hash / \
         registry and the target's Holochain+network config (from={from} to={to}). \
         Last init error: {cause:#}",
        timeout.as_secs()
    )
}

/// Run the open service to completion (or a hard stop), against the real local
/// conductor. Thin wrapper over [`run_with`] that supplies the production
/// [`HamConnector`]; `main.rs` calls this.
pub async fn run(
    cfg: &Config,
    open_cfg: &OpenConfig,
    params: &OpenParams,
    shutdown: &mut ham::ShutdownRx,
) -> Result<()> {
    run_with(&HamConnector { cfg }, cfg, open_cfg, params, shutdown).await
}

/// [`run`] with the conductor factory injected. The `shutdown` receiver is
/// installed ONCE by the caller (`main.rs`) and threaded all the way down into
/// every conductor (re)connect and sleep — the helpers never install their own
/// handler (that would leak a task + watch channel each pass and detach the
/// helpers from the real signal). The `ham`-backed conductor is rebuilt AFTER an
/// install (it cannot attach until the app cell exists), reusing this same
/// receiver. Tests supply a mock [`Connector`] to drive the whole loop with no
/// live conductor.
pub async fn run_with(
    connector: &dyn Connector,
    cfg: &Config,
    open_cfg: &OpenConfig,
    params: &OpenParams,
    shutdown: &mut ham::ShutdownRx,
) -> Result<()> {
    assert_happ_path(&open_cfg.happ_path)?;
    let agent_b64 = AgentPubKeyB64::from(params.agent_key.clone()).to_string();
    let http = joining::http_client()?;
    let signer = LairSigner::new(
        &params.agent_key,
        params.lair_url.clone(),
        params.lair_passphrase.clone(),
    );

    // One `State` carried across every pass (like the close service) so probe
    // flags / verify progress persist into the final record rather than being
    // re-stamped to defaults on each write. The agent is the carried key,
    // constant for the whole run.
    //
    // SEED the monotonic `safe_to_teardown` latch (and the prior verify detail)
    // from disk BEFORE the first persist. A cross-process restart starts here with
    // a fresh `State{safe_to_teardown: false}`; without this seed the first
    // `persist` in `attempt` would clobber a prior `true` on disk (defeating the
    // idempotent already-verified short-circuit, leaving the open service to spin
    // forever once the old side is torn down, and flipping the `automation`
    // roll-up's `.safe_to_teardown` false on a verified migration). Seeded, every
    // persist carries the prior `true` forward, and the `AlreadyOpened`
    // short-circuit checks the in-memory value rather than a just-written file.
    let mut state = State::new(Phase::Open, Step::Probing, "").with_agent(Some(agent_b64.clone()));
    state.seed_from_persisted(&cfg.state_file);
    let backoff = cfg.loop_backoff();
    let mut attempts: u32 = 0;
    loop {
        if *shutdown.borrow() {
            return shutdown_before_complete();
        }
        match attempt(
            connector, cfg, open_cfg, params, &http, &signer, shutdown, &mut state,
        )
        .await
        {
            OpenOutcome::Done => {
                persist(cfg, &mut state, |s| {
                    s.step = Step::Done;
                    s.new_chain_opened = true;
                    s.old_chain_closed = true;
                    s.safe_to_teardown = true;
                    s.message = "new chain opened + verified".into();
                });
                tracing::info!("open service complete: new chain opened + verified");
                return Ok(());
            }
            OpenOutcome::HardStop(why) => {
                persist(cfg, &mut state, |s| {
                    s.step = Step::Failed;
                    s.message = format!("hard stop: {why}");
                });
                bail!("open hard-stopped: {why}");
            }
            OpenOutcome::TooEarly(e) => {
                // Bounded retry: the successor GD `init` needs isn't in effect yet.
                // Re-drive after a backoff, but give up once it has stayed
                // unresolved past the deadline (it may never come). The deadline is
                // measured from the FIRST too-early and PERSISTED to the state file,
                // so a supervised `Restart=on-failure` resumes the SAME budget
                // rather than starting a fresh 30 minutes each restart.
                let now = crate::state_file::now_us();
                let started = *state.gd_wait_started_us.get_or_insert(now);
                if gd_wait_expired(started, now, open_cfg.gd_wait_timeout) {
                    // Budget spent: stamp + fail with the actionable config-fault
                    // diagnosis (NOT the raw genesis error), so the rail's
                    // state-file cat points the operator at the likely cause.
                    let msg = gd_wait_exhausted_message(
                        &params.from_dna,
                        &params.to_dna,
                        open_cfg.gd_wait_timeout,
                        &e,
                    );
                    persist(cfg, &mut state, |s| {
                        s.step = Step::Failed;
                        s.message = msg.clone();
                    });
                    bail!("{msg}");
                }
                // Clamp the backoff to the remaining budget so the wait never
                // overshoots the deadline by a full interval.
                let elapsed = Duration::from_micros(now.saturating_sub(started).max(0) as u64);
                let remaining = open_cfg.gd_wait_timeout.saturating_sub(elapsed);
                let delay =
                    Duration::from_millis(ham::compute_delay_ms(attempts, &backoff)).min(remaining);
                tracing::warn!(error = %format!("{e:#}"), delay_ms = delay.as_millis() as u64,
                    "successor GD not yet in effect; backing off (bounded)");
                // Persist carries the first-too-early stamp (set above) so the
                // budget survives the restart.
                persist(cfg, &mut state, |s| {
                    s.message = format!("waiting for the successor GD to come into effect: {e:#}");
                });
                if sleep_or_shutdown(delay, shutdown).await {
                    return shutdown_before_complete();
                }
                attempts = attempts.saturating_add(1);
            }
            OpenOutcome::Transient(e) => {
                // Jittered backoff via ham's shared curve (de-synchronizes many
                // open services riding parallel new-droplet init).
                let delay = Duration::from_millis(ham::compute_delay_ms(attempts, &backoff));
                tracing::warn!(error = %format!("{e:#}"), delay_ms = delay.as_millis() as u64,
                    "transient open failure; backing off");
                persist(cfg, &mut state, |s| {
                    s.message = format!("transient failure, retrying: {e:#}");
                });
                if sleep_or_shutdown(delay, shutdown).await {
                    return shutdown_before_complete();
                }
                attempts = attempts.saturating_add(1);
            }
        }
    }
}

/// One probe → act pass. Admin-only connect suffices to probe presence and to
/// install; the `verify_if_migrated` (which drives `init`) / `get_ledger` zome
/// calls need a `ham` attach, obtained by a fresh `connect` once the app cell
/// exists (threading the single `shutdown`, never installing a new handler).
///
/// The package fetch is CONDITIONAL on the branch that needs it. Verify reads
/// the OLD-side close via the router, so it can only run *during* the migration,
/// while the old side is up — never on a later restart once the operator has
/// torn down the old side. So the already-migrated-AND-already-verified path
/// short-circuits to `Done` from the PERSISTED state (`safe_to_teardown` written
/// authoritatively at verify success), with NO router fetch — otherwise an
/// idempotent restart would hard-require a fetch and spin forever after teardown.
/// The remaining branches each fetch the package up front (the install applies
/// it as `init_properties`, opening the chain at `init`; the not-yet-verified
/// opened path verifies against it), preserving the fetch-before-install rule.
async fn attempt(
    connector: &dyn Connector,
    cfg: &Config,
    open_cfg: &OpenConfig,
    params: &OpenParams,
    http: &reqwest::Client,
    signer: &dyn NonceSigner,
    shutdown: &mut ham::ShutdownRx,
    state: &mut State,
) -> OpenOutcome {
    let agent_b64 = state.agent.clone().unwrap_or_default();
    persist(cfg, state, |s| {
        s.step = Step::Probing;
        s.message = "probing new-server open state".into();
    });
    let admin = match connector.connect_admin_only().await {
        Ok(c) => c,
        Err(e) => return OpenOutcome::Transient(e.context("admin connect for probe")),
    };
    let open_state = match probe_open_state(admin.as_ref(), &cfg.app_id).await {
        Ok(s) => s,
        Err(e) => return OpenOutcome::Transient(e.context("probing open state")),
    };

    match open_state {
        OpenState::Installed => {
            // Idempotent restart after a verified migration: short-circuit to
            // Done WITHOUT a router fetch, since verify needs the old side which
            // the operator tears down after a verified migration. The teardown
            // latch is read from the SEEDED in-memory `state` (carried from disk
            // by `run`'s `seed_from_persisted`), NOT a fresh re-read of a file the
            // first `persist` has already rewritten — that re-read is exactly what
            // the round-2 clobber bug defeated.
            if state.safe_to_teardown {
                tracing::info!(
                    "already migrated and previously verified (persisted) — no fetch needed"
                );
                return OpenOutcome::Done;
            }
            // Installed but not yet verified — either the chain is already open (a
            // restart mid-verify) or `init` hasn't been driven yet (a restart
            // right after install). Fetch the package, connect ham, drive `init`
            // (`verify_if_migrated` opens the chain on the first call, idempotent
            // if already open), then verify. The fetch can be an acceptable
            // KeepWaiting in the early-restart-before-teardown window.
            let package = match fetch_or_outcome(cfg, params, &agent_b64, http, state).await {
                Ok(p) => p,
                Err(outcome) => return outcome,
            };
            let conductor = match connect_ham(connector, shutdown).await {
                Ok(c) => c,
                Err(o) => return o,
            };
            drive_open_and_verify(cfg, conductor.as_ref(), &package, state).await
        }
        OpenState::NotInstalled => {
            // Fetch BEFORE installing (the spec's hard ordering rule), then
            // install for the carried key WITH the package as the role's
            // `init_properties`, so the DNA's `init` opens the chain on the first
            // zome call. Reconnect ham → drive `init` + verify.
            let package = match fetch_or_outcome(cfg, params, &agent_b64, http, state).await {
                Ok(p) => p,
                Err(outcome) => return outcome,
            };
            if let Err(outcome) = install(
                cfg,
                open_cfg,
                params,
                http,
                signer,
                admin.as_ref(),
                &package,
                state,
            )
            .await
            {
                return outcome;
            }
            let conductor = match connect_ham(connector, shutdown).await {
                Ok(c) => c,
                Err(o) => return o,
            };
            drive_open_and_verify(cfg, conductor.as_ref(), &package, state).await
        }
    }
}

/// Fetch the migration package, mapping the fetch outcome to the open service's
/// `KeepWaiting → Transient` / `HardStop` semantics in ONE place. A
/// `no_close_found` (or any transient code) AFTER a known close is propagation
/// lag → keep waiting (`Transient`), NEVER a fresh-agent path; only a true
/// client/contract fault is a `HardStop`. On success it records
/// `package_fetchable`.
async fn fetch_or_outcome(
    cfg: &Config,
    params: &OpenParams,
    agent_b64: &str,
    http: &reqwest::Client,
    state: &mut State,
) -> std::result::Result<MigrationInitRequest, OpenOutcome> {
    persist(cfg, state, |s| {
        s.step = Step::WaitingForPackage;
        s.message = "fetching migration package (waiting out gossip)".into();
    });
    match fetch::fetch_package(
        http,
        &params.router_url,
        &params.from_dna,
        &params.to_dna,
        agent_b64,
    )
    .await
    {
        FetchOutcome::Package(p) => {
            persist(cfg, state, |s| {
                s.package_fetchable = true;
                s.message = "migration package fetched".into();
            });
            Ok(*p)
        }
        FetchOutcome::KeepWaiting(why) => Err(OpenOutcome::Transient(anyhow::anyhow!(
            "package not yet fetchable ({why}); waiting out gossip"
        ))),
        FetchOutcome::HardStop(why) => Err(OpenOutcome::HardStop(why)),
    }
}

/// Reconnect `ham` once the app cell is provisioned, threading the single
/// `shutdown` receiver. The `None → Transient` mapping (shutdown / unreachable)
/// lives here so every reconnect site is identical.
async fn connect_ham(
    connector: &dyn Connector,
    shutdown: &mut ham::ShutdownRx,
) -> std::result::Result<Arc<dyn Conductor>, OpenOutcome> {
    match connector.connect_ham(shutdown).await {
        Some(c) => Ok(c),
        None => Err(OpenOutcome::Transient(anyhow::anyhow!(
            "ham could not attach to the installed app cell (shutdown or unreachable)"
        ))),
    }
}

/// Get a fresh membrane proof for the carried key and install + enable the app,
/// carrying the already-fetched `package` as the alliance role's
/// `init_properties` so the DNA's `init` opens the chain on the first zome call
/// (no post-install `migration_init`). Returns the install outcome to propagate,
/// or `Ok(())` on success.
// One install attempt genuinely threads config (+ open config), the router/join
// params, the joining-service I/O (http + signer), the conductor, the fetched
// package, and the run's state — grouping them into a struct would be artificial
// indirection for a single call site.
#[allow(clippy::too_many_arguments)]
async fn install(
    cfg: &Config,
    open_cfg: &OpenConfig,
    params: &OpenParams,
    http: &reqwest::Client,
    signer: &dyn NonceSigner,
    admin: &dyn Conductor,
    package: &MigrationInitRequest,
    state: &mut State,
) -> std::result::Result<(), OpenOutcome> {
    // Fresh membrane proof for the carried key from the TARGET joining service.
    persist(cfg, state, |s| {
        s.step = Step::Installing;
        s.message = "requesting fresh membrane proof for the carried key".into();
    });
    let provision =
        match joining::join_and_provision(http, &open_cfg.joining_url, &params.agent_key, signer)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return Err(OpenOutcome::Transient(
                    e.context("fresh membrane proof from target joining service"),
                ))
            }
        };
    let membrane_proof = match provision.membrane_proofs.get(&cfg.role_name) {
        Some(b64) => match decode_membrane_proof(b64) {
            Ok(bytes) => Some(bytes),
            Err(e) => return Err(OpenOutcome::Transient(e)),
        },
        // No proof for the role is valid only if the role needs none; pass
        // `None` and let the validator decide.
        None => None,
    };

    persist(cfg, state, |s| {
        s.message = "installing app for the carried key".into();
    });
    let spec = InstallSpec {
        app_id: cfg.app_id.clone(),
        role_name: cfg.role_name.clone(),
        agent_key: params.agent_key.clone(),
        happ_path: open_cfg.happ_path.clone(),
        // The joining service's network seed wins when present.
        network_seed: provision
            .network_seed
            .or_else(|| open_cfg.network_seed.clone()),
        membrane_proof,
        migration_package: Some(package.clone()),
    };
    if let Err(e) = admin.install_app(&spec).await {
        return Err(install_error_outcome(e));
    }
    Ok(())
}

/// Map an `install_app` (install + enable) failure onto the open outcome. The
/// install carries the package as the role's `init_properties` with
/// `ignore_genesis_failure: false`, so a DNA-level verdict — the too-early
/// successor GD included — may surface from the admin call itself rather than
/// the later `verify_if_migrated`. Routing through the same
/// [`classify_migration_init_error`] contract as [`drive_open_and_verify`] keeps
/// that case on the BOUNDED [`OpenOutcome::TooEarly`] deadline (never an
/// unbounded `Transient` retry) and keeps terminal verdicts a hard stop.
/// `AlreadyMigrated` here means another pass raced this install — transient: the
/// next probe finds the app installed and proceeds straight to verify.
fn install_error_outcome(e: anyhow::Error) -> OpenOutcome {
    let rendered = format!("{e:#}");
    match classify_migration_init_error(&rendered) {
        InitErrorClass::HardFailure => OpenOutcome::HardStop(format!(
            "install rejected the opening summary with an unrecoverable verdict: {rendered}"
        )),
        InitErrorClass::NonFreshChain => OpenOutcome::HardStop(format!(
            "unexpected non-fresh chain surfaced at install: {rendered}"
        )),
        InitErrorClass::TooEarly => {
            OpenOutcome::TooEarly(e.context("successor GD not yet in effect (surfaced at install)"))
        }
        InitErrorClass::AlreadyMigrated | InitErrorClass::Transient => {
            OpenOutcome::Transient(e.context("install_app for the carried key"))
        }
    }
}

/// Drive the new cell's `init` and verify. With the package installed as the
/// role's `init_properties`, the FIRST zome call (`verify_if_migrated`) makes the
/// DNA's `init` read it and commit the `OpeningStateSummary` + `open_chain` — so
/// this both opens the chain and reports whether it opened. On `true` → verify.
/// A too-early `init` (the successor GD not yet in effect) surfaces as the
/// `Transient` fallthrough — the supervised loop re-drives it once the GD syncs.
/// A terminal validator verdict (key mismatch, signatures below threshold,
/// malformed carry-forward) is a [`OpenOutcome::HardStop`] — never an infinite
/// retry.
async fn drive_open_and_verify(
    cfg: &Config,
    conductor: &dyn Conductor,
    package: &MigrationInitRequest,
    state: &mut State,
) -> OpenOutcome {
    persist(cfg, state, |s| {
        s.step = Step::OpeningChain;
        s.message = "driving init via the first zome call (opening the chain)".into();
    });
    match conductor.verify_if_migrated().await {
        Ok(true) => verify_after_open_with(cfg, conductor, package, state).await,
        // We installed WITH the package as init_properties, so a completed-but-
        // un-opened init means the properties were not applied — a structural
        // install fault retrying can't fix, not a fresh-agent path.
        Ok(false) => OpenOutcome::HardStop(
            "installed with a migration package but the chain did not open at init \
             (init_properties were not applied)"
                .to_string(),
        ),
        Err(e) => {
            let rendered = format!("{e:#}");
            match classify_migration_init_error(&rendered) {
                InitErrorClass::AlreadyMigrated => {
                    // A race: another pass already opened the chain — verify.
                    verify_after_open_with(cfg, conductor, package, state).await
                }
                // The validator gave a terminal Invalid verdict (e.g. the
                // carried key doesn't match the notarized agent, or the
                // signatures don't meet the new GD's opening threshold). No
                // amount of retrying fixes that — fail loudly.
                InitErrorClass::HardFailure => OpenOutcome::HardStop(format!(
                    "init rejected the opening summary with an unrecoverable verdict: {rendered}"
                )),
                // A non-fresh chain can no longer arise: `init` runs on the first
                // zome call, so nothing precedes it. Treat the (now-unexpected)
                // verdict defensively as a hard stop rather than the removed
                // uninstall-retry.
                InitErrorClass::NonFreshChain => OpenOutcome::HardStop(format!(
                    "unexpected non-fresh chain at init (no call should precede it): {rendered}"
                )),
                // The successor GD is not yet in effect — re-drive `init` once it
                // syncs, but bounded by the run loop's deadline.
                InitErrorClass::TooEarly => {
                    OpenOutcome::TooEarly(e.context("successor GD not yet in effect"))
                }
                // Any other blip: back off and re-probe.
                InitErrorClass::Transient => OpenOutcome::Transient(e.context("driving init")),
            }
        }
    }
}

/// The shared verify: new-chain ledger vs the carried close summary.
async fn verify_after_open_with(
    cfg: &Config,
    conductor: &dyn Conductor,
    package: &MigrationInitRequest,
    state: &mut State,
) -> OpenOutcome {
    persist(cfg, state, |s| {
        s.step = Step::Verifying;
        // The chain IS open by the time verify starts (`init` committed the
        // opening summary) — stamp it now, not only at Done, so a read-only
        // `status` (which reports this persisted signal rather than making a
        // zome call that could itself drive `init`) sees the open as soon as
        // it happens, not only after verify passes.
        s.new_chain_opened = true;
        s.message = "verifying new-chain ledger against the close summary".into();
    });
    let ledger = match conductor.get_ledger().await {
        Ok(l) => l,
        Err(e) => return OpenOutcome::Transient(e.context("reading new-chain ledger for verify")),
    };
    let opened = match conductor.get_opened_agreement_state().await {
        Ok(o) => o,
        Err(e) => {
            return OpenOutcome::Transient(
                e.context("reading new-chain opened agreement state for verify"),
            )
        }
    };
    let mut report: VerifyReport = verify_against_ledger(&package.payload.closing_state, &ledger);
    let (agreement_state_match, mut agreement_mismatches) =
        crate::verify::verify_agreement_state(&package.payload, opened.as_ref());
    report.agreement_state_match = agreement_state_match;
    report.mismatches.append(&mut agreement_mismatches);
    let passed = report.passed();
    persist(cfg, state, |s| {
        s.verify = Some(report.clone());
        if passed {
            s.message = "verify passed".into();
        } else {
            s.message = format!("verify FAILED: {}", report.mismatches.join("; "));
        }
    });
    if passed {
        OpenOutcome::Done
    } else {
        OpenOutcome::HardStop(format!("verify mismatch: {}", report.mismatches.join("; ")))
    }
}

/// Sleep up to `dur`, returning `true` if shutdown fired first.
async fn sleep_or_shutdown(dur: Duration, shutdown: &mut ham::ShutdownRx) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(dur) => false,
        _ = shutdown.changed() => true,
    }
}

/// Shutdown fired before the new chain was open + verified: the migration is
/// INCOMPLETE, so the service must exit nonzero (not `Ok`). A supervised
/// one-shot exits 0 only on success — exiting 0 here would let systemd's
/// `Restart=on-failure` treat an interrupted (e.g. reboot mid-open) run as done
/// and never resume it. The already-verified idempotent restart short-circuits
/// to `Done` (Ok) BEFORE this is reached (it checks the seeded latch up top), so
/// this only fires on a genuinely incomplete run. We deliberately do NOT write
/// the state file here — leaving the last meaningful record (the seeded
/// `safe_to_teardown` latch + verify detail, plus this run's probe progress)
/// intact rather than re-stamping a message over it; the next restart's first
/// probe rewrites it, and the monotonic latch is untouched either way.
fn shutdown_before_complete() -> Result<()> {
    bail!("shutdown before open completed (new chain not yet verified)")
}

/// Apply `f` to the carried `state` and persist it (logging a write error —
/// never fatal). Mutating the carried `state` keeps probe flags / verify
/// progress across passes rather than re-stamping a fresh `State::new` each
/// write.
fn persist(cfg: &Config, state: &mut State, f: impl FnOnce(&mut State)) {
    f(state);
    if let Err(e) = state.write(&cfg.state_file) {
        tracing::error!(error = %format!("{e:#}"), "failed writing state file");
    }
}

/// Report whether the new chain is opened, for the `Status` command — READ-ONLY.
/// `status` must never mutate the chain, and on a cell installed with migration
/// `init_properties` the FIRST zome call (`verify_if_migrated` included) drives
/// `init` and opens the chain — the supervised open service's job, under its
/// bounded/verified flow. So this probe makes NO zome call: it reports the open
/// service's persisted `new_chain_opened` signal (stamped the moment `init` has
/// driven the open — see [`verify_after_open_with`]), gated on the app actually
/// being present. `false` if the app isn't installed, whatever the record says
/// (e.g. a stale file surviving an uninstall); otherwise the persisted signal.
pub async fn probe_for_status(
    conductor: &dyn Conductor,
    app_id: &str,
    persisted_new_chain_opened: bool,
) -> Result<bool> {
    match conductor
        .app_presence(app_id)
        .await
        .context("probing app presence for status")?
    {
        crate::conductor::AppPresence::Absent => Ok(false),
        crate::conductor::AppPresence::Installed => Ok(persisted_new_chain_opened),
    }
}

#[cfg(test)]
mod tests {
    use super::{gd_wait_exhausted_message, gd_wait_expired, install_error_outcome, OpenOutcome};
    use crate::state_file::now_us;
    use holo_hash::{DnaHash, DnaHashB64};
    use std::time::Duration;

    const BUDGET: Duration = Duration::from_secs(1800);

    #[test]
    fn gd_wait_exhausted_message_is_a_config_fault_diagnosis() {
        // The exhaustion message must LEAD with the actionable config-fault
        // diagnosis (successor DNA hash / registry / target config), carry both
        // DNAs, and keep the raw init error as a trailing detail — NOT a bare
        // genesis error. It is stamped into the state file the rail cats out.
        let from = DnaHashB64::from(DnaHash::from_raw_36(vec![1; 36]));
        let to = DnaHashB64::from(DnaHash::from_raw_36(vec![2; 36]));
        let cause = anyhow::anyhow!("wasm error: No Global Definition found");
        let msg = gd_wait_exhausted_message(&from, &to, BUDGET, &cause);
        assert!(
            msg.contains("check the successor DNA hash / registry"),
            "leads with the config-fault diagnosis: {msg}"
        );
        assert!(
            msg.contains(&from.to_string()) && msg.contains(&to.to_string()),
            "carries both DNAs: {msg}"
        );
        assert!(msg.contains("1800s"), "carries the elapsed budget: {msg}");
        assert!(
            msg.contains("No Global Definition found"),
            "keeps the raw init cause as a trailing detail: {msg}"
        );
    }

    #[test]
    fn gd_wait_not_expired_within_budget() {
        let now = now_us();
        // First too-early "just now" — well within the budget.
        assert!(!gd_wait_expired(now, now, BUDGET));
        // Just under the budget is not yet expired.
        assert!(!gd_wait_expired(0, 1799 * 1_000_000, BUDGET));
    }

    #[test]
    fn gd_wait_expired_past_budget_across_a_restart() {
        // A first too-early recorded 40 minutes ago — e.g. carried across a
        // supervised restart via the persisted stamp — is past the 30-minute
        // budget, so the restarted service hard-stops immediately instead of
        // waiting another full budget.
        let started = now_us() - 40 * 60 * 1_000_000;
        assert!(gd_wait_expired(started, now_us(), BUDGET));
        // Exactly at the budget is expired (`>=`), so it fails ON the boundary.
        assert!(gd_wait_expired(0, 1800 * 1_000_000, BUDGET));
    }

    #[test]
    fn too_early_from_install_is_bounded_not_transient() {
        // With the package as `init_properties` (and `ignore_genesis_failure:
        // false`) the too-early successor GD can surface from the admin
        // install/enable call itself. It must land on the bounded TooEarly path
        // (the run loop's persisted deadline), never the unbounded Transient
        // retry — the same self-recovery bound `drive_open_and_verify` gets.
        let e = anyhow::anyhow!(
            "Could not resolve a successor GlobalDefinition at init (NoGlobalDefinition)"
        );
        assert!(matches!(install_error_outcome(e), OpenOutcome::TooEarly(_)));
        let e = anyhow::anyhow!("wasm error: No Global Definition found");
        assert!(matches!(install_error_outcome(e), OpenOutcome::TooEarly(_)));
    }

    #[test]
    fn terminal_verdict_from_install_is_a_hard_stop() {
        // A terminal validator verdict surfaced at install retries can never
        // fix — hard stop, exactly as when it surfaces from driving `init`.
        let e = anyhow::anyhow!("opening summary agent does not match the notarized agent");
        assert!(matches!(install_error_outcome(e), OpenOutcome::HardStop(_)));
    }

    #[test]
    fn plain_blip_from_install_stays_transient() {
        // An ordinary transport blip keeps the existing back-off-and-re-probe
        // behavior; so does a raced already-migrated (the next probe finds the
        // app installed and proceeds straight to verify).
        let e = anyhow::anyhow!("websocket closed; reconnecting");
        assert!(matches!(
            install_error_outcome(e),
            OpenOutcome::Transient(_)
        ));
        let e = anyhow::anyhow!("this chain has already been migrated");
        assert!(matches!(
            install_error_outcome(e),
            OpenOutcome::Transient(_)
        ));
    }
}
