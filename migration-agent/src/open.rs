//! The open service: effectively the new server's install step. A supervised
//! loop that waits out gossip until the migration package is fetchable, then
//! installs the app for the carried key, runs `migration_init` as the FIRST
//! zome call, and verifies — exiting 0 only once the new chain is open and
//! verified. Probe-first and idempotent: a restart never double-opens, and a
//! non-fresh chain (a stray zome call landed before `migration_init`) is
//! recovered by uninstall → reinstall → retry.

use std::time::Duration;

use anyhow::{bail, Context, Result};
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
use crate::probe::{probe_open_state, OpenNext};
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

/// One open attempt's outcome.
enum OpenOutcome {
    /// New chain open + verified. Exit 0.
    Done,
    /// A hard stop (verify mismatch, warranted close). Exit nonzero.
    HardStop(String),
    /// Transient — back off and re-probe (gossip lag, conductor blip).
    Transient(anyhow::Error),
    /// A non-fresh chain was rejected: uninstall, then re-probe (which will
    /// reinstall + retry migration_init).
    NonFreshChain,
}

/// Run the open service to completion (or a hard stop). The `shutdown` receiver
/// is installed ONCE by the caller (`main.rs`) and threaded all the way down
/// into every conductor (re)connect and sleep — the helpers never install their
/// own handler (that would leak a task + watch channel each pass and detach the
/// helpers from the real signal). The `ham`-backed conductor is rebuilt AFTER an
/// install (it cannot attach until the app cell exists), reusing this same
/// receiver.
pub async fn run(
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
            tracing::info!("shutdown before open completed");
            return Ok(());
        }
        match attempt(cfg, open_cfg, params, &http, &signer, shutdown, &mut state).await {
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
            OpenOutcome::NonFreshChain => {
                tracing::warn!("non-fresh new chain — uninstalling to retry a clean open");
                persist(cfg, &mut state, |s| {
                    // The chain is being torn down for a clean retry — clear the
                    // stale opened/verify progress so the report doesn't show a
                    // half-open state for the cell we just uninstalled.
                    s.new_chain_opened = false;
                    s.verify = None;
                    s.message = "non-fresh chain — uninstalling for a clean retry".into();
                });
                // A short, fixed pause before the immediate re-probe.
                if sleep_or_shutdown(cfg.retry_initial, shutdown).await {
                    return Ok(());
                }
                attempts = 0;
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
                    return Ok(());
                }
                attempts = attempts.saturating_add(1);
            }
        }
    }
}

/// One probe → act pass. Admin-only connect suffices to probe presence and to
/// install; the `migration_init` / `get_ledger` zome calls need a `ham` attach,
/// obtained by a fresh `connect` once the app cell exists (threading the single
/// `shutdown`, never installing a new handler).
///
/// The package fetch is CONDITIONAL on the branch that needs it. Verify reads
/// the OLD-side close via the router, so it can only run *during* the migration,
/// while the old side is up — never on a later restart once the operator has
/// torn down the old side. So the already-migrated-AND-already-verified path
/// short-circuits to `Done` from the PERSISTED state (`safe_to_teardown` written
/// authoritatively at verify success), with NO router fetch — otherwise an
/// idempotent restart would hard-require a fetch and spin forever after teardown.
/// The three remaining branches each fetch the package up front (install +
/// migration_init commit it; the not-yet-verified opened path verifies against
/// it), preserving the spec's fetch-before-install ordering rule.
async fn attempt(
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
    let admin = match HamConductor::connect_admin_only(cfg).await {
        Ok(c) => c,
        Err(e) => return OpenOutcome::Transient(e.context("admin connect for probe")),
    };
    let open_state = match probe_open_state(&admin, &cfg.app_id).await {
        Ok(s) => s,
        Err(e) => return OpenOutcome::Transient(e.context("probing open state")),
    };

    match open_state.next() {
        OpenNext::AlreadyOpened => {
            // Already migrated. If a prior pass already verified, this restart is
            // idempotently Done — exit WITHOUT a router fetch, since verify needs
            // the old side which the operator tears down after a verified
            // migration. The teardown latch is read from the SEEDED in-memory
            // `state` (carried from disk by `run`'s `seed_from_persisted`), NOT a
            // fresh re-read of a file the first `persist` has already rewritten —
            // that re-read is exactly what the round-2 clobber bug defeated. Only
            // the not-yet-verified case fetches + verifies (the early-restart-
            // before-teardown window, where a fetch failure is an acceptable
            // KeepWaiting).
            if state.safe_to_teardown {
                tracing::info!(
                    "already migrated and previously verified (persisted) — no fetch needed"
                );
                return OpenOutcome::Done;
            }
            let package = match fetch_or_outcome(cfg, params, &agent_b64, http, state).await {
                Ok(p) => p,
                Err(outcome) => return outcome,
            };
            let conductor = match connect_ham(cfg, shutdown).await {
                Ok(c) => c,
                Err(o) => return o,
            };
            verify_after_open_with(cfg, &conductor, &package, state).await
        }
        OpenNext::OpenOnly => {
            // Installed but not migrated — fetch the package, then run
            // migration_init (first zome call) and verify.
            let package = match fetch_or_outcome(cfg, params, &agent_b64, http, state).await {
                Ok(p) => p,
                Err(outcome) => return outcome,
            };
            let conductor = match connect_ham(cfg, shutdown).await {
                Ok(c) => c,
                Err(o) => return o,
            };
            run_migration_init(cfg, &conductor, &package, state).await
        }
        OpenNext::FetchInstallOpen => {
            // Fetch BEFORE installing (the spec's hard ordering rule), then
            // install for the carried key, reconnect ham → migration_init.
            let package = match fetch_or_outcome(cfg, params, &agent_b64, http, state).await {
                Ok(p) => p,
                Err(outcome) => return outcome,
            };
            if let Err(outcome) = install(cfg, open_cfg, params, http, signer, &admin, state).await
            {
                return outcome;
            }
            let conductor = match connect_ham(cfg, shutdown).await {
                Ok(c) => c,
                Err(o) => return o,
            };
            run_migration_init(cfg, &conductor, &package, state).await
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
            Ok(p)
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
    cfg: &Config,
    shutdown: &mut ham::ShutdownRx,
) -> std::result::Result<HamConductor, OpenOutcome> {
    match HamConductor::connect(cfg, shutdown).await {
        Some(c) => Ok(c),
        None => Err(OpenOutcome::Transient(anyhow::anyhow!(
            "ham could not attach to the installed app cell (shutdown or unreachable)"
        ))),
    }
}

/// Get a fresh membrane proof for the carried key and install + enable the app
/// (the package has already been fetched by the caller, satisfying the
/// fetch-before-install ordering rule). Returns the install outcome to
/// propagate, or `Ok(())` on success.
async fn install(
    cfg: &Config,
    open_cfg: &OpenConfig,
    params: &OpenParams,
    http: &reqwest::Client,
    signer: &dyn NonceSigner,
    admin: &HamConductor,
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
    };
    if let Err(e) = admin.install_app(&spec).await {
        return Err(OpenOutcome::Transient(
            e.context("install_app for the carried key"),
        ));
    }
    Ok(())
}

/// Run `migration_init` as the first zome call on the (now-attached) cell using
/// the already-fetched `package`, then verify. A non-fresh-chain rejection is
/// mapped to [`OpenOutcome::NonFreshChain`] after uninstalling; a hard-failure
/// verdict (agent mismatch, insufficient/invalid signatures, malformed
/// carry-forward) is a [`OpenOutcome::HardStop`] — never an infinite retry.
async fn run_migration_init(
    cfg: &Config,
    conductor: &HamConductor,
    package: &MigrationInitRequest,
    state: &mut State,
) -> OpenOutcome {
    persist(cfg, state, |s| {
        s.step = Step::OpeningChain;
        s.message = "running migration_init as the first zome call".into();
    });
    let request = MigrationInitRequest {
        payload: package.payload.clone(),
        notary_signatures: package.notary_signatures.clone(),
        close_action: package.close_action.clone(),
    };
    match conductor.migration_init(request).await {
        Ok(()) => verify_after_open_with(cfg, conductor, package, state).await,
        Err(e) => {
            let rendered = format!("{e:#}");
            match classify_migration_init_error(&rendered) {
                InitErrorClass::NonFreshChain => {
                    tracing::warn!(error = %rendered,
                        "migration_init rejected on a non-fresh chain");
                    if let Err(ue) = conductor.uninstall_app(&cfg.app_id).await {
                        return OpenOutcome::Transient(
                            ue.context("uninstalling after non-fresh-chain rejection"),
                        );
                    }
                    OpenOutcome::NonFreshChain
                }
                InitErrorClass::AlreadyMigrated => {
                    // A race: another pass already opened the chain — verify.
                    verify_after_open_with(cfg, conductor, package, state).await
                }
                // The validator gave a terminal Invalid verdict (e.g. the
                // carried key doesn't match the notarized agent, or the
                // signatures don't meet the new GD's opening threshold). No
                // amount of retrying fixes that — fail loudly.
                InitErrorClass::HardFailure => OpenOutcome::HardStop(format!(
                    "migration_init rejected with an unrecoverable verdict: {rendered}"
                )),
                InitErrorClass::Transient => OpenOutcome::Transient(e.context("migration_init")),
            }
        }
    }
}

/// The shared verify: new-chain ledger vs the carried close summary.
async fn verify_after_open_with(
    cfg: &Config,
    conductor: &HamConductor,
    package: &MigrationInitRequest,
    state: &mut State,
) -> OpenOutcome {
    persist(cfg, state, |s| {
        s.step = Step::Verifying;
        s.message = "verifying new-chain ledger against the close summary".into();
    });
    let ledger = match conductor.get_ledger().await {
        Ok(l) => l,
        Err(e) => return OpenOutcome::Transient(e.context("reading new-chain ledger for verify")),
    };
    let report: VerifyReport = verify_against_ledger(&package.payload.closing_state, &ledger);
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

/// Probe whether the new chain is opened, for the `Status` command's report.
pub async fn probe_for_status(conductor: &dyn Conductor, app_id: &str) -> Result<bool> {
    Ok(matches!(
        probe_open_state(conductor, app_id)
            .await
            .context("probing open state for status")?,
        crate::probe::OpenState::Migrated
    ))
}
