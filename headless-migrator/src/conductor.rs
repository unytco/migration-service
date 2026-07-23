//! The [`Conductor`] abstraction over the local Holochain conductor — every
//! zome call and admin app-lifecycle operation the four services need. The real
//! impl ([`HamConductor`]) wraps `ham` for typed zome calls and a direct
//! `AdminWebsocket` for install / uninstall / enable / list (which `ham` does
//! not expose); tests inject a mock so the probe / close / open / verify state
//! machines run without a conductor — the notary-daemon's mocked-seam pattern.
//!
//! `ham` attaches to a *provisioned* app cell, so it cannot connect before the
//! open service installs the app. The open service therefore connects
//! admin-only first (install), then reconnects with `ham` to drive `init` (the
//! first `verify_if_migrated` call opens the chain) and verify — hence `ham` is
//! `Option` here, and a zome call attempted before that reconnect fails with a
//! clear error rather than panicking.

use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use holo_hash::{AgentPubKey, DnaHash};
use holochain_client::{AdminWebsocket, AppInfo, CellInfo, WebsocketConfig};
use holochain_types::app::{AppBundleSource, InstallAppPayload, RoleSettings, RoleSettingsMap};
use holochain_types::prelude::{
    CellId, DnaModifiersOpt, InitProperties, MembraneProof, SerializedBytes, UnsafeBytes,
    YamlProperties,
};
use rave_engine::types::entries::migration::v0_1::{
    CloseRequest, CommittedClose, MigrationInitRequest, NotarySignature, PrepareCloseResponse,
    SignClosingResponse, SignRequest, SummaryStatePayload,
};
use rave_engine::types::ledger::Ledger;

use crate::config::Config;

/// Whether an app with our id is installed on the conductor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppPresence {
    Absent,
    Installed,
}

/// Settings for installing the carried-key app on the new DNA. Held by the open
/// service; passed through to [`Conductor::install_app`].
#[derive(Debug, Clone)]
pub struct InstallSpec {
    pub app_id: String,
    pub role_name: String,
    pub agent_key: AgentPubKey,
    pub happ_path: std::path::PathBuf,
    pub network_seed: Option<String>,
    /// The network's DNA properties for the role, as the target release's joining
    /// service serves them ([`crate::joining::Provision::properties`]). Hashed
    /// into the DNA hash alongside the network seed, so an install that omits
    /// them lands the cell on a DIFFERENT DNA than the network — its own empty
    /// DHT, no peers, no gossip, and an `init` that can never resolve the GD.
    pub properties: Option<YamlProperties>,
    /// Base64-decoded membrane proof for the role, fetched fresh from the target
    /// joining service for the carried key (`None` ⇒ no proof required).
    pub membrane_proof: Option<Vec<u8>>,
    /// The fetched migration package, installed as the alliance role's
    /// `init_properties` so the DNA's `init` opens the chain at genesis — no
    /// post-install `migration_init` zome call. `None` for a fresh,
    /// non-migrating install.
    pub migration_package: Option<MigrationInitRequest>,
}

/// Build the `InstallAppPayload` for the carried key from an [`InstallSpec`].
/// Pure (no conductor I/O), so the init_properties encode — the fetched
/// migration package placed as the migrating role's `init_properties`, which the
/// DNA's `init` reads to open the chain at genesis — is unit-testable without a
/// live conductor. `HamConductor::install_app` is a thin wrapper: build + send.
pub fn build_install_payload(spec: &InstallSpec) -> Result<InstallAppPayload> {
    let membrane_proof: Option<MembraneProof> = spec
        .membrane_proof
        .as_ref()
        .map(|bytes| Arc::new(SerializedBytes::from(UnsafeBytes::from(bytes.clone()))));
    // Both DNA modifiers are hashed into the DNA hash, so the role override
    // carries whatever the joining service gave us for BOTH — the properties in
    // particular, since the happ manifest declares none (`properties: ~`) and
    // every fleet installer supplies the network's from the release config. An
    // override that set only the seed would leave the cell on a property-less
    // DNA — a different hash from the network's, hence its own empty DHT. The
    // conductor applies only the `Some` fields (`AppManifestV0::override_modifiers`),
    // so a field we don't have leaves the manifest's value untouched; when we have
    // neither, no override is sent at all.
    let modifiers = match (&spec.network_seed, &spec.properties) {
        (None, None) => None,
        (network_seed, properties) => Some(DnaModifiersOpt::<YamlProperties> {
            network_seed: network_seed.clone(),
            properties: properties.clone(),
        }),
    };
    // A migrating install carries the fetched package as the role's
    // `init_properties`; the DNA's `init` reads it and opens the chain at genesis
    // (driven by the open service's first zome call). A fresh, non-migrating
    // install carries none.
    let init_properties = spec
        .migration_package
        .as_ref()
        .map(|pkg| {
            SerializedBytes::try_from(pkg)
                .map(InitProperties)
                .map_err(|e| anyhow::anyhow!("encoding migration init_properties: {e:?}"))
        })
        .transpose()?;
    let mut roles: RoleSettingsMap = RoleSettingsMap::new();
    roles.insert(
        spec.role_name.clone(),
        RoleSettings::Provisioned {
            membrane_proof,
            modifiers,
            init_properties,
        },
    );
    Ok(InstallAppPayload {
        source: AppBundleSource::Path(spec.happ_path.clone()),
        agent_key: Some(spec.agent_key.clone()),
        installed_app_id: Some(spec.app_id.clone()),
        network_seed: spec.network_seed.clone(),
        roles_settings: Some(roles),
        ignore_genesis_failure: false,
    })
}

/// The opened chain's committed migrated agreement state, as the DNA's
/// `get_opened_agreement_state` extern returns it — re-exported from
/// `rave_engine` under this module's path, so call sites read it as
/// `conductor::OpenedAgreementState`.
pub use rave_engine::types::entries::migration::v0_1::OpenedAgreementState;

#[async_trait]
pub trait Conductor: Send + Sync {
    /// Lightweight liveness probe against the conductor's app cell.
    async fn ping(&self) -> Result<()>;

    // ── Old-DNA (close-side) zome calls ──────────────────────────────────

    /// `transactor::get_ledger` — the agent's ledger, for the fees-owed probe
    /// and the close-side half of `Verify`.
    async fn get_ledger(&self) -> Result<Ledger>;

    /// `transactor::drop_off_fees` — clear any owed fees BEFORE preparing the
    /// summary (post-signing chain activity voids the signatures).
    async fn drop_off_fees(&self) -> Result<String>;

    /// `transactor::prepare_closing_summary` — the payload to collect signatures
    /// over plus the GD's closing pair (N, M). Takes the successor `target` the
    /// close binds to; the extern pre-checks it against this DNA's
    /// `upgrade_targets`.
    async fn prepare_closing_summary(&self, target: DnaHash) -> Result<PrepareCloseResponse>;

    /// `transactor::request_closing_signature` — one `call_remote` to a notary's
    /// `notary_sign_closing_summary`, response verbatim.
    async fn request_closing_signature(&self, req: SignRequest) -> Result<SignClosingResponse>;

    /// `transactor::close_agent_chain` — commit the `ClosingStateSummary` and
    /// `close_chain`. Returns the summary action hash.
    async fn close_agent_chain(
        &self,
        payload: SummaryStatePayload,
        notary_signatures: Vec<NotarySignature>,
    ) -> Result<holo_hash::ActionHash>;

    /// `transactor::get_migration_close_state` — the agent's own committed close
    /// `{ payload, notary_signatures, close_action }`. Errors if no close is
    /// committed; used to tell open-chain from closed-chain in the probe and to
    /// feed the open service + `Verify`.
    async fn get_migration_close_state(&self) -> Result<CommittedClose>;

    // ── New-DNA (open-side) zome calls ───────────────────────────────────

    /// `transactor::verify_if_migrated` — stable "has this chain migrated onto
    /// this DNA?" query (an `OpeningStateSummary` exists). On a cell installed
    /// with migration `init_properties`, the FIRST call to this drives `init`,
    /// which reads them and opens the chain — so it doubles as the open service's
    /// "drive `init`" step (no separate `migration_init` extern).
    async fn verify_if_migrated(&self) -> Result<bool>;

    /// `transactor::get_opened_agreement_state` — the migrated agreement state
    /// the new chain actually COMMITTED at open (`None` ⇒ this chain did not
    /// migrate). The independent new-chain half of Verify's agreement
    /// cross-check against the router-fetched close package.
    async fn get_opened_agreement_state(&self) -> Result<Option<OpenedAgreementState>>;

    // ── Admin app-lifecycle (open-side) ──────────────────────────────────

    /// Whether an app with `app_id` is installed on the conductor.
    async fn app_presence(&self, app_id: &str) -> Result<AppPresence>;

    /// The `CellId` the installed app's provisioned cell for `role_name` sits on
    /// (DNA + agent), or `None` when the app isn't installed — or is installed but
    /// has no readable provisioned cell for the role. Read-only — admin
    /// `list_apps`, never a zome call, so it is safe on an `init_properties` cell
    /// whose first zome call would drive `init`. Lets the open service hold an
    /// ALREADY-installed app to the migration target, not just one it installed
    /// itself.
    ///
    /// `None` deliberately conflates "absent" with "present but unreadable": both
    /// are UNKNOWN, and the caller treats unknown as "skip the check" rather than
    /// "mismatch". The alternative — `Err` for present-but-unreadable — turns a
    /// PERMANENT condition into an unbounded transient retry, which is the worse
    /// failure. The caller logs the skip instead.
    async fn installed_cell_id(&self, app_id: &str, role_name: &str) -> Result<Option<CellId>>;

    /// Install the app for the carried key and enable it, returning the `CellId`
    /// the provisioned cell actually landed on — the caller holds it to the
    /// migration target (DNA *and* carried agent), so a modifiers or key mismatch
    /// fails loudly here instead of surfacing much later as an `init` that can
    /// never resolve the GD.
    async fn install_app(&self, spec: &InstallSpec) -> Result<CellId>;
}

/// Real conductor connection: `ham` for app-cell zome calls + a separate
/// `AdminWebsocket` for the app-lifecycle admin calls `ham` doesn't surface.
pub struct HamConductor {
    /// `None` until an app is provisioned (the open service's pre-install
    /// admin-only phase). Zome calls require it.
    ham: Option<ham::Ham>,
    admin: AdminWebsocket,
    role_name: String,
}

impl HamConductor {
    /// Connect both `ham` (with backoff until the conductor + app cell are
    /// reachable, or shutdown fires) and the admin socket. The close / status /
    /// verify services use this — the app is already installed.
    pub async fn connect(cfg: &Config, shutdown: &mut ham::ShutdownRx) -> Option<Self> {
        let ham_cfg = ham::HamConfig::new(cfg.admin_port, cfg.app_port, cfg.app_id.clone())
            .with_request_timeout_secs(cfg.request_timeout_secs);
        let backoff = ham::BackoffConfig::default();
        let ham =
            ham::connect_with_backoff(|| ham::Ham::connect(ham_cfg.clone()), &backoff, shutdown)
                .await?;
        let admin = match Self::connect_admin(cfg).await {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %format!("{e:#}"), "admin websocket connect failed");
                return None;
            }
        };
        Some(Self {
            ham: Some(ham),
            admin,
            role_name: cfg.role_name.clone(),
        })
    }

    /// Connect ONLY the admin socket — the open service's pre-install phase,
    /// when there is no provisioned app cell for `ham` to attach to.
    pub async fn connect_admin_only(cfg: &Config) -> Result<Self> {
        let admin = Self::connect_admin(cfg).await?;
        Ok(Self {
            ham: None,
            admin,
            role_name: cfg.role_name.clone(),
        })
    }

    async fn connect_admin(cfg: &Config) -> Result<AdminWebsocket> {
        let mut ws_config = WebsocketConfig::CLIENT_DEFAULT;
        // Install on first run compiles + validates the DNA wasm — minutes on a
        // small droplet — so give the admin socket a generous floor.
        ws_config.default_request_timeout = Duration::from_secs(cfg.request_timeout_secs.max(600));
        AdminWebsocket::connect_with_config(
            (Ipv4Addr::LOCALHOST, cfg.admin_port),
            Arc::new(ws_config),
            Some("headless-migrator".into()),
        )
        .await
        .map_err(|e| anyhow::anyhow!("admin connect: {e}"))
        .context("connecting admin websocket")
    }

    fn ham(&self) -> Result<&ham::Ham> {
        self.ham.as_ref().context(
            "no app cell connection — the app must be installed before this zome call \
             (open service connects admin-only until install)",
        )
    }
}

#[async_trait]
impl Conductor for HamConductor {
    async fn ping(&self) -> Result<()> {
        self.ham()?.ping().await
    }

    async fn get_ledger(&self) -> Result<Ledger> {
        self.ham()?
            .call_zome(&self.role_name, "transactor", "get_ledger", ())
            .await
            .context("get_ledger zome call failed")
    }

    async fn drop_off_fees(&self) -> Result<String> {
        self.ham()?
            .call_zome(&self.role_name, "transactor", "drop_off_fees", ())
            .await
            .context("drop_off_fees zome call failed")
    }

    async fn prepare_closing_summary(&self, target: DnaHash) -> Result<PrepareCloseResponse> {
        self.ham()?
            .call_zome(
                &self.role_name,
                "transactor",
                "prepare_closing_summary",
                target,
            )
            .await
            .context("prepare_closing_summary zome call failed")
    }

    async fn request_closing_signature(&self, req: SignRequest) -> Result<SignClosingResponse> {
        self.ham()?
            .call_zome(
                &self.role_name,
                "transactor",
                "request_closing_signature",
                req,
            )
            .await
            .context("request_closing_signature zome call failed")
    }

    async fn close_agent_chain(
        &self,
        payload: SummaryStatePayload,
        notary_signatures: Vec<NotarySignature>,
    ) -> Result<holo_hash::ActionHash> {
        let req = CloseRequest {
            payload,
            notary_signatures,
        };
        self.ham()?
            .call_zome(&self.role_name, "transactor", "close_agent_chain", req)
            .await
            .context("close_agent_chain zome call failed")
    }

    async fn get_migration_close_state(&self) -> Result<CommittedClose> {
        self.ham()?
            .call_zome(
                &self.role_name,
                "transactor",
                "get_migration_close_state",
                (),
            )
            .await
            .context("get_migration_close_state zome call failed")
    }

    async fn verify_if_migrated(&self) -> Result<bool> {
        self.ham()?
            .call_zome(&self.role_name, "transactor", "verify_if_migrated", ())
            .await
            .context("verify_if_migrated zome call failed")
    }

    async fn get_opened_agreement_state(&self) -> Result<Option<OpenedAgreementState>> {
        self.ham()?
            .call_zome(
                &self.role_name,
                "transactor",
                "get_opened_agreement_state",
                (),
            )
            .await
            .context("get_opened_agreement_state zome call failed")
    }

    async fn app_presence(&self, app_id: &str) -> Result<AppPresence> {
        let apps = self
            .admin
            .list_apps(None)
            .await
            .map_err(|e| anyhow::anyhow!("list_apps: {e}"))?;
        Ok(if apps.iter().any(|a| a.installed_app_id == app_id) {
            AppPresence::Installed
        } else {
            AppPresence::Absent
        })
    }

    async fn installed_cell_id(&self, app_id: &str, role_name: &str) -> Result<Option<CellId>> {
        let apps = self
            .admin
            .list_apps(None)
            .await
            .map_err(|e| anyhow::anyhow!("list_apps: {e}"))?;
        Ok(apps
            .iter()
            .find(|a| a.installed_app_id == app_id)
            .and_then(|info| provisioned_cell_id(info, role_name).ok()))
    }

    async fn install_app(&self, spec: &InstallSpec) -> Result<CellId> {
        let payload = build_install_payload(spec)?;
        let info = self
            .admin
            .install_app(payload)
            .await
            .map_err(|e| anyhow::anyhow!("install_app: {e}"))?;
        self.admin
            .enable_app(spec.app_id.clone())
            .await
            .map_err(|e| anyhow::anyhow!("enable_app: {e}"))?;
        provisioned_cell_id(&info, &spec.role_name)
    }
}

/// The `CellId` the app's provisioned cell for `role_name` landed on, read from
/// the install response. The conductor hands this back at install, so the open
/// service can check the carried key joined the DNA it is migrating TO rather
/// than discovering an isolated cell later, via an `init` that never resolves the
/// successor GD. It is the CellId rather than the DNA hash alone because the pair
/// is what identifies the cell: a node running two migrating apps off one shared
/// carried lair can land the right DNA under the WRONG agent, and the hash alone
/// would wave that through.
fn provisioned_cell_id(info: &AppInfo, role_name: &str) -> Result<CellId> {
    info.cell_info
        .get(role_name)
        .and_then(|cells| {
            cells.iter().find_map(|cell| match cell {
                CellInfo::Provisioned(p) => Some(p.cell_id.clone()),
                _ => None,
            })
        })
        .with_context(|| {
            format!("the install response carried no provisioned cell for role '{role_name}'")
        })
}

/// Decode a base64 membrane-proof string (the joining service's wire form) into
/// the raw bytes the install payload carries.
pub fn decode_membrane_proof(b64: &str) -> Result<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(b64)
        .context("decoding base64 membrane proof")
}

/// Error clearly if a happ bundle is missing, so the open service fails loudly
/// up front rather than deep inside `install_app`.
pub fn assert_happ_path(path: &Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("happ bundle not found at {}", path.display());
    }
    // A directory or special path can't be a `.happ` bundle — reject it up front
    // rather than letting `install_app` fail obscurely deep in the install.
    if !path.is_file() {
        anyhow::bail!("happ bundle path is not a regular file: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch path under the temp dir (no `tempfile` dev-dep, matching
    /// the integration tests' convention).
    fn scratch(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "headless-migrator-conductor-test-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn assert_happ_path_accepts_a_regular_file() {
        let f = scratch("regular-file");
        std::fs::write(&f, b"not a real happ, but a file").unwrap();
        assert!(assert_happ_path(&f).is_ok());
        let _ = std::fs::remove_file(&f);
    }

    #[test]
    fn assert_happ_path_rejects_a_missing_path() {
        let missing = scratch("missing");
        let err = assert_happ_path(&missing).unwrap_err().to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn assert_happ_path_rejects_a_directory() {
        // A directory exists() but is not a `.happ` bundle — fail fast.
        let dir = scratch("dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = assert_happ_path(&dir).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
