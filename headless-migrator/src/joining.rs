//! Fresh-membrane-proof acquisition from the TARGET release's joining service
//! for the carried key. The old proof is never reused (proof requirements can
//! change per version); only the agent key is continuous.
//!
//! Mirrors the fleet's existing `agent_allow_list` join flow
//! (`automation/packages/unyt-deploy`): `POST /join` → if pending, sign the
//! challenge nonce with the carried key via lair → `POST /join/:session/verify`
//! → `GET /join/:session/provision`, which returns the per-role membrane proofs
//! and dna modifiers. Nonce signing is the same `lair-sign` invocation the
//! fleet uses, factored behind [`NonceSigner`] so the HTTP flow is unit-tested
//! without lair.

use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use holo_hash::{AgentPubKey, AgentPubKeyB64};
use serde::Deserialize;

/// What the joining service returns from `provision`: per-role membrane proofs
/// (base64) and optional DNA modifiers (the network seed takes precedence over
/// any configured one at install).
#[derive(Debug, Clone, Default)]
pub struct Provision {
    /// Role name → base64 membrane proof.
    pub membrane_proofs: std::collections::HashMap<String, String>,
    pub network_seed: Option<String>,
}

/// Signs a join challenge nonce (base64) with the carried key, returning the
/// base64 ed25519 signature — the seam between the HTTP flow and lair.
pub trait NonceSigner {
    fn sign_nonce(&self, nonce_b64: &str) -> Result<String>;
}

/// The production signer: shell out to `lair-sign` against the local lair (the
/// same command the fleet's deploy runs, but local rather than over SSH — the
/// open service is on the new droplet). Output is the trimmed base64 signature.
pub struct LairSigner {
    pub connection_url: String,
    pub passphrase: String,
    /// The carried key's ed25519 component, base64 (the 32 bytes after the
    /// 3-byte holo_hash prefix), as `lair-sign --pub-key` expects.
    pub pub_key_ed25519_b64: String,
}

impl LairSigner {
    /// Build from the carried agent key + lair connection details.
    pub fn new(agent_key: &AgentPubKey, connection_url: String, passphrase: String) -> Self {
        Self {
            connection_url,
            passphrase,
            pub_key_ed25519_b64: agent_key_to_ed25519_b64(agent_key),
        }
    }
}

impl NonceSigner for LairSigner {
    fn sign_nonce(&self, nonce_b64: &str) -> Result<String> {
        let out = Command::new("lair-sign")
            .arg("--connection-url")
            .arg(&self.connection_url)
            .arg("--passphrase")
            .arg(&self.passphrase)
            .arg("--pub-key")
            .arg(&self.pub_key_ed25519_b64)
            .arg("--data")
            .arg(nonce_b64)
            .output()
            .context("invoking lair-sign (is it on PATH on the droplet?)")?;
        if !out.status.success() {
            bail!(
                "lair-sign failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8(out.stdout)
            .context("lair-sign output was not UTF-8")?
            .trim()
            .to_string())
    }
}

/// The ed25519 portion of a holo_hash agent key, base64 — `lair-sign`'s
/// `--pub-key`. A holo_hash `AgentPubKey` is `0x84 0x20 0x24` ++ 32 core bytes
/// ++ 4-byte location; the raw signing key is those 32 core bytes.
pub fn agent_key_to_ed25519_b64(agent_key: &AgentPubKey) -> String {
    use base64::Engine;
    let raw = agent_key.get_raw_32();
    base64::engine::general_purpose::STANDARD.encode(raw)
}

// ── HTTP wire shapes (the joining service's `agent_allow_list` flow) ──────

#[derive(Deserialize)]
struct JoinResponse {
    session: String,
    status: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    challenges: Vec<Challenge>,
}

#[derive(Deserialize)]
struct Challenge {
    id: String,
    #[serde(rename = "type")]
    challenge_type: String,
    #[serde(default)]
    metadata: Option<ChallengeMeta>,
}

#[derive(Deserialize)]
struct ChallengeMeta {
    #[serde(default)]
    nonce: Option<String>,
}

#[derive(Deserialize)]
struct VerifyResponse {
    status: String,
}

#[derive(Deserialize)]
struct ProvisionResponse {
    #[serde(default)]
    membrane_proofs: std::collections::HashMap<String, String>,
    #[serde(default)]
    dna_modifiers: Option<DnaModifiers>,
}

#[derive(Deserialize)]
struct DnaModifiers {
    #[serde(default)]
    network_seed: Option<String>,
}

/// Run the full join + provision flow against `joining_url` for `agent_key`,
/// signing the challenge nonce with `signer`. Returns the per-role membrane
/// proofs + modifiers for the install.
pub async fn join_and_provision(
    client: &reqwest::Client,
    joining_url: &str,
    agent_key: &AgentPubKey,
    signer: &dyn NonceSigner,
) -> Result<Provision> {
    let base = joining_url.trim_end_matches('/');
    let agent_b64 = AgentPubKeyB64::from(agent_key.clone()).to_string();

    // Step 1: POST /join.
    let join: JoinResponse = client
        .post(format!("{base}/join"))
        .json(&serde_json::json!({ "agent_key": agent_b64 }))
        .send()
        .await
        .context("POST /join")?
        .error_for_status()
        .context("POST /join returned an error status")?
        .json()
        .await
        .context("decoding /join response")?;

    let session = match join.status.as_str() {
        // Already cleared (e.g. an allow-list with no challenge) → provision.
        "ready" => join.session.clone(),
        "pending" => {
            let challenge = join
                .challenges
                .iter()
                .find(|c| c.challenge_type == "agent_allow_list")
                .context("no agent_allow_list challenge in /join response")?;
            let nonce = challenge
                .metadata
                .as_ref()
                .and_then(|m| m.nonce.as_deref())
                .context("agent_allow_list challenge missing nonce")?;
            let signature = signer.sign_nonce(nonce)?;

            // Step 3: POST /join/:session/verify.
            let verify: VerifyResponse = client
                .post(format!("{base}/join/{}/verify", join.session))
                .json(&serde_json::json!({
                    "challenge_id": challenge.id,
                    "response": signature,
                }))
                .send()
                .await
                .context("POST /join/:session/verify")?
                .error_for_status()
                .context("verify returned an error status")?
                .json()
                .await
                .context("decoding verify response")?;
            if verify.status != "ready" {
                bail!("join verify status {} (expected ready)", verify.status);
            }
            join.session.clone()
        }
        other => {
            let detail = join
                .reason
                .map(|r| format!(" (reason: {r})"))
                .unwrap_or_default();
            bail!("unexpected join status {other}{detail}");
        }
    };

    // Step 4: GET /join/:session/provision.
    let provision: ProvisionResponse = client
        .get(format!("{base}/join/{session}/provision"))
        .send()
        .await
        .context("GET /join/:session/provision")?
        .error_for_status()
        .context("provision returned an error status")?
        .json()
        .await
        .context("decoding provision response")?;

    Ok(Provision {
        membrane_proofs: provision.membrane_proofs,
        network_seed: provision.dna_modifiers.and_then(|m| m.network_seed),
    })
}

/// A `reqwest` client with a sane timeout for the joining-service calls.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building joining-service HTTP client")
}
