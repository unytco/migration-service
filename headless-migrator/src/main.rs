//! `headless-migrator` — the headless server-agent migration driver. The command
//! surface is the spec's `MigrateCommand`: `status` · `close-service` ·
//! `open-service` · `verify`. Each supervised service exits 0 only on success
//! and is probe-first + idempotent, so systemd `Restart=on-failure` drives the
//! loop (there is no overall deadline). Logs go to stderr (journald-friendly);
//! every service also writes a machine-readable state file the report collector
//! reads.

use std::process::ExitCode;
use std::str::FromStr;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use holo_hash::{AgentPubKey, AgentPubKeyB64, DnaHashB64};

use headless_migrator::config::{Config, OpenConfig};
use headless_migrator::open::OpenParams;
use headless_migrator::status::StatusParams;
use headless_migrator::verify::VerifyParams;
use headless_migrator::{close, open, status, verify};

#[derive(Parser, Debug)]
#[command(
    name = "headless-migrator",
    about = "Headless server-agent migration: close the old chain (M-of-N) and \
             re-open it on the successor DNA with the carried key",
    long_about = None,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Probe + report: old chain open/closed · package fetchable · new chain
    /// opened. Read-only. Router args are optional (close-side has no router).
    Status {
        /// Router base URL (enables the package-fetchability probe).
        #[arg(long)]
        router_url: Option<String>,
        /// Predecessor (from) DNA hash, base64. Required with `--router-url`.
        #[arg(long)]
        from_dna: Option<String>,
        /// Successor (to) DNA hash, base64. Required with `--router-url`.
        #[arg(long)]
        to_dna: Option<String>,
        /// The migrating agent's pub key (base64). Required with `--router-url`.
        #[arg(long)]
        agent_key: Option<String>,
    },

    /// Supervised loop: probe → (drop fees if owed) → prepare → collect M-of-N
    /// → close_agent_chain. Exits 0 only once the old chain is closed.
    CloseService,

    /// Supervised loop: wait/retry the package fetch → fresh membrane proof for
    /// the carried key → install_app WITH the package as init_properties → drive
    /// init via the first zome call → verify. Exits 0 only once the new chain is
    /// open + verified.
    OpenService {
        /// Router base URL (the package source).
        #[arg(long)]
        router_url: String,
        /// Predecessor (from) DNA hash, base64.
        #[arg(long)]
        from_dna: String,
        /// Successor (to) DNA hash, base64.
        #[arg(long)]
        to_dna: String,
        /// The carried agent's pub key (base64) — already in the new lair.
        #[arg(long)]
        agent_key: String,
        /// Lair connection URL on this droplet (for signing the join nonce).
        #[arg(long, env = "MIGRATION_AGENT_LAIR_URL")]
        lair_url: String,
        /// Lair passphrase on this droplet.
        #[arg(long, env = "MIGRATION_AGENT_LAIR_PASSPHRASE")]
        lair_passphrase: String,
    },

    /// One-shot: close-summary vs new-chain ledger (balances, CFU, agreement
    /// count). Nonzero exit + per-field report on mismatch.
    Verify {
        /// Router base URL (the close-summary source).
        #[arg(long)]
        router_url: String,
        /// Predecessor (from) DNA hash, base64.
        #[arg(long)]
        from_dna: String,
        /// Successor (to) DNA hash, base64.
        #[arg(long)]
        to_dna: String,
        /// The migrating agent's pub key (base64).
        #[arg(long)]
        agent_key: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    // Logs to stderr (journald captures it); not JSON by default so journald
    // lines stay readable, but env-filter still applies.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "headless-migrator failed");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::from_env().context("loading config from environment")?;

    match cli.command {
        Command::Status {
            router_url,
            from_dna,
            to_dna,
            agent_key,
        } => {
            let params = build_status_params(router_url, from_dna, to_dna, agent_key)?;
            status::run(&cfg, params.as_ref()).await?;
            Ok(())
        }

        Command::CloseService => {
            let mut shutdown = ham::install_shutdown_handler();
            let conductor = connect(&cfg, &mut shutdown)
                .await
                .context("connecting to the old conductor")?;
            close::run(&conductor, &cfg, &mut shutdown).await
        }

        Command::OpenService {
            router_url,
            from_dna,
            to_dna,
            agent_key,
            lair_url,
            lair_passphrase,
        } => {
            let open_cfg = OpenConfig::from_env().context("loading open-service config")?;
            let params = OpenParams {
                router_url,
                from_dna: parse_dna(&from_dna, "from_dna")?,
                to_dna: parse_dna(&to_dna, "to_dna")?,
                agent_key: parse_agent(&agent_key)?,
                lair_url,
                lair_passphrase,
            };
            let mut shutdown = ham::install_shutdown_handler();
            open::run(&cfg, &open_cfg, &params, &mut shutdown).await
        }

        Command::Verify {
            router_url,
            from_dna,
            to_dna,
            agent_key,
        } => {
            let params = VerifyParams {
                router_url,
                from_dna: parse_dna(&from_dna, "from_dna")?,
                to_dna: parse_dna(&to_dna, "to_dna")?,
                agent_b64: parse_agent(&agent_key).map(|k| AgentPubKeyB64::from(k).to_string())?,
            };
            verify::run(&cfg, &params).await
        }
    }
}

/// Connect a `ham`-backed conductor (the close / status path). Separated so the
/// `Option` → `Result` lift has one home.
async fn connect(
    cfg: &Config,
    shutdown: &mut ham::ShutdownRx,
) -> Result<headless_migrator::conductor::HamConductor> {
    headless_migrator::conductor::HamConductor::connect(cfg, shutdown)
        .await
        .context("conductor unreachable before shutdown")
}

/// Build the optional `Status` router params: all four must be present together
/// (or all absent → close-side status with no fetchability probe).
fn build_status_params(
    router_url: Option<String>,
    from_dna: Option<String>,
    to_dna: Option<String>,
    agent_key: Option<String>,
) -> Result<Option<StatusParams>> {
    match (router_url, from_dna, to_dna, agent_key) {
        (None, None, None, None) => Ok(None),
        (Some(router_url), Some(from), Some(to), Some(agent)) => Ok(Some(StatusParams {
            router_url,
            from_dna: parse_dna(&from, "from_dna")?,
            to_dna: parse_dna(&to, "to_dna")?,
            agent_b64: parse_agent(&agent).map(|k| AgentPubKeyB64::from(k).to_string())?,
        })),
        _ => anyhow::bail!(
            "status router probe needs all of --router-url --from-dna --to-dna --agent-key, \
             or none of them"
        ),
    }
}

fn parse_dna(s: &str, what: &str) -> Result<DnaHashB64> {
    DnaHashB64::from_str(s).with_context(|| format!("parsing {what} as a DnaHashB64: {s}"))
}

fn parse_agent(s: &str) -> Result<AgentPubKey> {
    Ok(AgentPubKeyB64::from_str(s)
        .with_context(|| format!("parsing agent_key as an AgentPubKeyB64: {s}"))?
        .into())
}
