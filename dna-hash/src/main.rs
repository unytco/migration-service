//! Predict a role's DNA hash from a hApp / DNA bundle plus the install-time
//! network seed + properties — WITHOUT a conductor or a deploy.
//!
//! A Holochain DNA hash is deterministic over the integrity code + the DNA
//! **modifiers** (network_seed, properties, origin/quantum time). Installing a
//! `.happ` adds a network seed + properties (progenitor pubkey, joining-server
//! signer, …) that change the hash, so today the migration registry's successor
//! `to_dna_hash` is only knowable after deploying the fleet. This computes it up
//! front by reusing Holochain's OWN bundle machinery, so the result is
//! byte-identical to what the conductor produces at install (validated against
//! two live fleets — see backlog B85).
//!
//! The load + modifier + hash core lives in [`dna_hash`] (the crate library) so
//! it is testable against a real bundle fixture without a conductor.
//!
//! Examples:
//!   dna-hash --bundle unyt.happ --role alliance \
//!            --network-seed unyt-local-testnet-a \
//!            --properties '{"progenitor_pubkey":"uhCAk…","joining_server_signer":"uhCAk…"}'
//!   dna-hash --bundle alliance.dna --network-seed my-seed

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use holo_hash::DnaHashB64;

#[derive(Parser, Debug)]
#[command(
    name = "dna-hash",
    about = "Predict a role's DNA hash from a hApp/DNA bundle + install-time modifiers (backlog B85)."
)]
struct Args {
    /// Path to the release bundle: a `.happ`/`.webhapp` (a role is selected) or a bare `.dna`.
    #[arg(long)]
    bundle: PathBuf,

    /// Role name to hash inside a `.happ` (ignored for a bare `.dna`).
    #[arg(long, default_value = "alliance")]
    role: String,

    /// Network seed applied at install (the DNA modifier). Omit to keep the bundle's default.
    #[arg(long)]
    network_seed: Option<String>,

    /// DNA properties applied at install, as a JSON object. Mutually exclusive with --properties-file.
    #[arg(long, conflicts_with = "properties_file")]
    properties: Option<String>,

    /// DNA properties as a JSON/YAML file.
    #[arg(long)]
    properties_file: Option<PathBuf>,
}

fn raw_properties(args: &Args) -> Result<Option<String>> {
    match (&args.properties, &args.properties_file) {
        (Some(s), _) => Ok(Some(s.clone())),
        (None, Some(p)) => Ok(Some(
            std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
        )),
        (None, None) => Ok(None),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = Args::parse();
    let ext = args
        .bundle
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let properties = match raw_properties(&args)? {
        Some(raw) => Some(dna_hash::properties_serialized(&raw)?),
        None => None,
    };
    let bytes = std::fs::read(&args.bundle)
        .with_context(|| format!("reading bundle {}", args.bundle.display()))?;

    let dna_bundle = dna_hash::load_dna_bundle(&bytes, &ext, &args.role).await?;
    let dna_hash = dna_hash::hash_dna(dna_bundle, args.network_seed, properties).await?;
    println!("{}", DnaHashB64::from(dna_hash));
    Ok(())
}
