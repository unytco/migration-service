//! Gated live round-trip + restart drills: the REAL headless-migrator services
//! against locally running conductors (an old-DNA conductor with a stateful
//! agent + notary cells, and a new-DNA conductor), plus a `wrangler dev` router.
//! Proves the close → key-carry → open → verify arc end-to-end, and that a kill
//! mid-flow resumes without re-collecting for a closed chain or double-opening.
//!
//! Ignored by default — these need live conductors + a router and so run only
//! at release time (this milestone is build + local verification). They are
//! kept compiling so the wiring can't rot. The release-integration live smoke
//! is the instrument that actually runs them.
//!
//! ## Fixture + run
//!
//! Stand up, via the unyt repo's sweettest / `make launch-tauri` tooling:
//!   * an OLD-DNA conductor hosting the `alliance` app for a stateful agent,
//!     with notary cells on the old DNA (so close-time signing works);
//!   * a `wrangler dev` router whose registry maps the old DNA → new DNA and
//!     lists the local notary daemon(s) (see `migration-service/router`);
//!   * a NEW-DNA conductor (admin reachable) with the carried key imported into
//!     its lair (the shell's `migrate-carry-key.sh` step) + `lair-sign` on PATH;
//!   * the target release's joining service reachable for a fresh membrane proof.
//!
//! Then, from `headless-migrator/`:
//!
//! ```bash
//! # ── close service (old conductor) ──
//! MIGRATION_AGENT_STATE_FILE=/tmp/mig-close.json \
//! HOLOCHAIN_ADMIN_PORT=<old-admin> HOLOCHAIN_APP_PORT=<old-app> \
//! HOLOCHAIN_APP_ID=<old-app-id> HOLOCHAIN_ROLE_NAME=alliance \
//! LIVE_OLD_ADMIN_PORT=<old-admin> LIVE_OLD_APP_PORT=<old-app> \
//! LIVE_OLD_APP_ID=<old-app-id> \
//! LIVE_NEW_ADMIN_PORT=<new-admin> LIVE_NEW_APP_PORT=<new-app> \
//! LIVE_NEW_APP_ID=<new-app-id> \
//! LIVE_ROUTER_URL=http://127.0.0.1:8787 \
//! LIVE_FROM_DNA=<uhC0k...old> LIVE_TO_DNA=<uhC0k...new> \
//! LIVE_AGENT_KEY=<uhCAk...carried> \
//! LIVE_HAPP_PATH=<path/to/new.happ> \
//! LIVE_JOINING_URL=<https://target-joining> \
//! LIVE_LAIR_URL=<unix:///.../lair/socket?k=...> LIVE_LAIR_PASSPHRASE=<pass> \
//! cargo test --test live_roundtrip -- --ignored --nocapture
//! ```

use std::str::FromStr;

use anyhow::{Context, Result};
use holo_hash::{AgentPubKey, AgentPubKeyB64, DnaHashB64};

use headless_migrator::conductor::HamConductor;
use headless_migrator::config::{Config, OpenConfig};
use headless_migrator::open::{self, OpenParams};
use headless_migrator::verify::VerifyParams;
use headless_migrator::{close, verify};

/// Read all the live env vars into the agent's config + params, or skip with a
/// clear message if the fixture isn't present.
struct LiveEnv {
    old_cfg: Config,
    new_cfg: Config,
    open_cfg: OpenConfig,
    open_params: OpenParams,
    verify_params: VerifyParams,
}

fn var(key: &str) -> Result<String> {
    std::env::var(key).with_context(|| format!("{key} is required (see file header)"))
}

fn load_live_env() -> Result<LiveEnv> {
    let agent_key: AgentPubKey = AgentPubKeyB64::from_str(&var("LIVE_AGENT_KEY")?)
        .context("LIVE_AGENT_KEY as AgentPubKeyB64")?
        .into();
    let from_dna = DnaHashB64::from_str(&var("LIVE_FROM_DNA")?).context("LIVE_FROM_DNA")?;
    let to_dna = DnaHashB64::from_str(&var("LIVE_TO_DNA")?).context("LIVE_TO_DNA")?;
    let router_url = var("LIVE_ROUTER_URL")?;
    let agent_b64 = AgentPubKeyB64::from(agent_key.clone()).to_string();

    let base = |state: &str| -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("headless-migrator-live-{state}.json"));
        p
    };

    let mut old_cfg = Config::from_env().context("old-side config (see file header)")?;
    old_cfg.admin_port = var("LIVE_OLD_ADMIN_PORT")?
        .parse()
        .context("LIVE_OLD_ADMIN_PORT")?;
    old_cfg.app_port = var("LIVE_OLD_APP_PORT")?
        .parse()
        .context("LIVE_OLD_APP_PORT")?;
    old_cfg.app_id = var("LIVE_OLD_APP_ID")?;
    old_cfg.state_file = base("close");

    let mut new_cfg = old_cfg.clone();
    new_cfg.admin_port = var("LIVE_NEW_ADMIN_PORT")?
        .parse()
        .context("LIVE_NEW_ADMIN_PORT")?;
    new_cfg.app_port = var("LIVE_NEW_APP_PORT")?
        .parse()
        .context("LIVE_NEW_APP_PORT")?;
    new_cfg.app_id = var("LIVE_NEW_APP_ID")?;
    new_cfg.state_file = base("open");

    let open_cfg = OpenConfig {
        happ_path: var("LIVE_HAPP_PATH")?.into(),
        joining_url: var("LIVE_JOINING_URL")?,
        network_seed: std::env::var("LIVE_NETWORK_SEED").ok(),
    };

    let open_params = OpenParams {
        router_url: router_url.clone(),
        from_dna: from_dna.clone(),
        to_dna: to_dna.clone(),
        agent_key: agent_key.clone(),
        lair_url: var("LIVE_LAIR_URL")?,
        lair_passphrase: var("LIVE_LAIR_PASSPHRASE")?,
    };

    let verify_params = VerifyParams {
        router_url,
        from_dna,
        to_dna,
        agent_b64,
    };

    Ok(LiveEnv {
        old_cfg,
        new_cfg,
        open_cfg,
        open_params,
        verify_params,
    })
}

/// The full arc: close on the old conductor → (key already carried) → open on
/// the new conductor → verify. The fresh-chain rule makes ordering self-proving:
/// any pre-`migration_init` zome call leaves a non-fresh chain the open
/// validator rejects.
#[tokio::test]
#[ignore = "needs live old+new conductors + a wrangler-dev router; see the file header"]
async fn live_close_carry_open_verify() -> Result<()> {
    let env = load_live_env()?;

    // ── Close (old conductor) ──
    let mut shutdown = ham::install_shutdown_handler();
    let old = HamConductor::connect(&env.old_cfg, &mut shutdown)
        .await
        .context("old conductor unreachable")?;
    close::run(&old, &env.old_cfg, &mut shutdown)
        .await
        .context("close service")?;

    // ── Open (new conductor) — install for the carried key + migration_init ──
    open::run(&env.new_cfg, &env.open_cfg, &env.open_params, &mut shutdown)
        .await
        .context("open service")?;

    // ── Verify (new conductor) ──
    verify::run(&env.new_cfg, &env.verify_params)
        .await
        .context("verify")?;
    Ok(())
}

/// Restart drill: running the open service a SECOND time after it has completed
/// is a no-op (the chain is already opened) and still reaches Done — the
/// double-open guard holds across a fresh process.
///
/// The second run points the router at a DOWN address: an already-migrated +
/// already-verified restart must reach Done from the PERSISTED verify signal
/// (`safe_to_teardown = true`, written by the first run) WITHOUT any router
/// fetch — the regression guard for the idempotent path hard-requiring a fetch
/// and spinning forever once the operator has torn the old side down.
#[tokio::test]
#[ignore = "needs the live fixture from the file header; run after live_close_carry_open_verify"]
async fn live_open_is_idempotent_across_restart() -> Result<()> {
    let mut env = load_live_env()?;
    let mut shutdown = ham::install_shutdown_handler();
    // First run (assumes close already done by the arc test or fixture) — this
    // verifies and persists `safe_to_teardown = true`.
    open::run(&env.new_cfg, &env.open_cfg, &env.open_params, &mut shutdown)
        .await
        .context("first open")?;
    // Second run with the router DOWN: must short-circuit on the persisted
    // verify signal (no fetch) and still reach Done — the old-side-gone case.
    env.open_params.router_url = "http://127.0.0.1:1".into();
    open::run(&env.new_cfg, &env.open_cfg, &env.open_params, &mut shutdown)
        .await
        .context("second open (must be a no-op, no router fetch)")?;
    Ok(())
}

/// Restart drill: re-running the close service after a successful close is a
/// no-op (never re-collects for a closed chain).
#[tokio::test]
#[ignore = "needs the live fixture from the file header; run after a completed close"]
async fn live_close_is_idempotent_across_restart() -> Result<()> {
    let env = load_live_env()?;
    let mut shutdown = ham::install_shutdown_handler();
    let old = HamConductor::connect(&env.old_cfg, &mut shutdown)
        .await
        .context("old conductor unreachable")?;
    // Both runs must succeed; the second short-circuits on the closed probe.
    close::run(&old, &env.old_cfg, &mut shutdown)
        .await
        .context("first close")?;
    close::run(&old, &env.old_cfg, &mut shutdown)
        .await
        .context("second close (must be a no-op)")?;
    Ok(())
}
