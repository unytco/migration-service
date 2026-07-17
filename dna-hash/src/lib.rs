//! Core of the `dna-hash` tool: load a role's DNA from a `.happ` / `.webhapp` /
//! `.dna` bundle and compute its install-time DNA hash from the network seed +
//! properties, WITHOUT a conductor or a deploy. Extracted from `main.rs` so the
//! load + modifier + hash logic — fragile across Holochain pins (see backlog
//! B85) — is directly testable against a real bundle fixture.
//!
//! Two things are load-bearing and easy to get wrong:
//!   1. `DnaBundle::into_dna_file` returns the ORIGINAL (pre-override) hash — the
//!      modified hash lives on the `DnaFile`, so overrides are applied via
//!      `DnaFile::update_modifiers(..).dna_hash()`.
//!   2. `properties` is msgpack-serialized as the DNA modifier verbatim (a
//!      `YamlProperties` map), using Holochain's own encoder so the bytes match
//!      what the conductor stores in the DNA modifiers at install.

use anyhow::{anyhow, bail, Context, Result};
use holo_hash::DnaHash;
use holochain_types::prelude::{
    AppBundle, DnaBundle, DnaModifiersOpt, SerializedBytes, UnsafeBytes, YamlProperties,
};
use holochain_types::web_app::WebAppBundle;

/// The operator's `--properties` (JSON or YAML) as the exact `SerializedBytes`
/// the DNA hashes over: JSON/YAML → `YamlProperties` → msgpack, using
/// Holochain's own encoder so the bytes match what the conductor stores in the
/// DNA modifiers at install. (JSON is a subset of YAML, so `serde_yaml` parses
/// either into the same `Value`.)
pub fn properties_serialized(raw: &str) -> Result<SerializedBytes> {
    let value: serde_yaml::Value =
        serde_yaml::from_str(raw).context("parsing properties as JSON/YAML")?;
    let bytes = holochain_serialized_bytes::encode(&YamlProperties::new(value))
        .context("msgpack-encoding properties")?;
    Ok(SerializedBytes::from(UnsafeBytes::from(bytes)))
}

/// Load the target `role`'s DNA bundle from raw bundle bytes, branching on the
/// (lowercased) extension:
///   * `.dna`     — the DNA bundle itself.
///   * `.happ`    — an `AppBundle`; select the role and pull its bundled DNA.
///   * `.webhapp` — a `WebAppBundle` WRAPPING the `.happ`; unwrap to the inner
///     `AppBundle` first (B85: the old code fed webhapp bytes straight to
///     `AppBundle::unpack`, which fails on the outer web-app envelope).
pub async fn load_dna_bundle(bytes: &[u8], ext: &str, role: &str) -> Result<DnaBundle> {
    match ext {
        "dna" => DnaBundle::unpack(bytes).context("unpacking the .dna bundle"),
        "happ" | "webhapp" => {
            let app = if ext == "webhapp" {
                WebAppBundle::unpack(bytes)
                    .context("unpacking the .webhapp bundle")?
                    .happ_bundle()
                    .await
                    .context("extracting the inner .happ from the .webhapp")?
            } else {
                AppBundle::unpack(bytes).context("unpacking the .happ bundle")?
            };
            dna_bundle_for_role(&app, role)
        }
        other => {
            bail!("unrecognized bundle extension '.{other}' (expected .happ, .webhapp, or .dna)")
        }
    }
}

/// Select `role` inside a hApp bundle and unpack its bundled DNA resource.
fn dna_bundle_for_role(app: &AppBundle, role: &str) -> Result<DnaBundle> {
    let manifest = app.manifest();
    let role_manifest = manifest
        .app_roles()
        .into_iter()
        .find(|r| r.name == role)
        .ok_or_else(|| {
            anyhow!(
                "role '{}' not found; roles: [{}]",
                role,
                manifest
                    .app_roles()
                    .iter()
                    .map(|r| r.name.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let resource_id = role_manifest
        .dna
        .path
        .clone()
        .ok_or_else(|| anyhow!("role '{}' has no bundled DNA path", role))?;
    let resource = app
        .get_resource(&resource_id)
        .ok_or_else(|| anyhow!("bundle has no resource at '{}'", resource_id))?;
    DnaBundle::unpack(resource.as_ref())
        .with_context(|| format!("unpacking the DNA resource '{resource_id}'"))
}

/// Compute the install-time DNA hash: load the DNA at the bundle's baked
/// modifiers, then apply the operator's `network_seed` / `properties` overrides
/// on the `DnaFile` and read ITS hash.
///
/// `into_dna_file` returns the ORIGINAL (pre-override) hash, so the modified
/// hash must be read from the overridden `DnaFile`. Only supplied fields
/// override; the DNA's baked origin/quantum time stay put — exactly as the
/// conductor overrides at install.
pub async fn hash_dna(
    dna_bundle: DnaBundle,
    network_seed: Option<String>,
    properties: Option<SerializedBytes>,
) -> Result<DnaHash> {
    let (dna_file, _original_hash) = dna_bundle
        .into_dna_file(DnaModifiersOpt::none())
        .await
        .context("loading the DNA file from the bundle")?;
    let mut modifiers: DnaModifiersOpt<SerializedBytes> = DnaModifiersOpt::none();
    modifiers.network_seed = network_seed;
    modifiers.properties = properties;
    Ok(dna_file.update_modifiers(modifiers).dna_hash().clone())
}
