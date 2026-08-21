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
use holochain_types::prelude::YamlProperties;
use serde::Deserialize;

/// What the joining service returns from `provision` for THIS migration's
/// configured role: its membrane proof (base64) and the network's DNA
/// modifiers. The seed takes precedence over any configured one at install;
/// the properties have no configured counterpart — this is their ONLY source,
/// and the install applies them verbatim.
#[derive(Debug, Clone, Default)]
pub struct Provision {
    /// Base64 membrane proof for the role, or `None` if the role needs none —
    /// the joining service omits it for a role with no configured DNA hash.
    pub membrane_proof: Option<String>,
    pub network_seed: Option<String>,
    /// The network's DNA properties (`progenitor_pubkey` / `joining_server_signer`
    /// on our fleet). Both modifiers are hashed into the DNA hash, so the install
    /// must apply these — the happ manifest declares none, and every other fleet
    /// installer supplies them from the same release config the joining service
    /// serves here. Held as `YamlProperties` (a `serde_yaml::Value`, IndexMap-
    /// backed) rather than a `serde_json::Value`, so the map keeps the wire order:
    /// properties are msgpack-encoded into the hash and a reordered map is a
    /// different DNA.
    pub properties: Option<YamlProperties>,
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
    roles: std::collections::HashMap<String, RoleProvision>,
}

#[derive(Deserialize)]
struct RoleProvision {
    #[serde(default)]
    membrane_proof: Option<String>,
    #[serde(default)]
    dna_modifiers: Option<DnaModifiers>,
}

#[derive(Deserialize)]
struct DnaModifiers {
    #[serde(default)]
    network_seed: Option<String>,
    #[serde(default)]
    properties: Option<YamlProperties>,
}

/// Pull `role_name`'s provisioning data out of the decoded response, erroring
/// by name rather than defaulting when the role is missing. A response shaped
/// for a different wire contract — e.g. the retired top-level
/// `membrane_proofs`/`dna_modifiers` keys — decodes to an EMPTY `roles` map
/// here, so this must fail rather than let an absent role's data flow to the
/// install as `None`.
fn provision_for_role(mut response: ProvisionResponse, role_name: &str) -> Result<Provision> {
    let role = response.roles.remove(role_name).with_context(|| {
        format!(
            "joining-service provision response has no entry for role '{role_name}' in its \
             roles map (roles present: {:?}) — its membrane proof and DNA modifiers are unknown",
            response.roles.keys().collect::<Vec<_>>()
        )
    })?;
    let (network_seed, properties) = match role.dna_modifiers {
        Some(m) => (m.network_seed, m.properties),
        None => (None, None),
    };
    Ok(Provision {
        membrane_proof: role.membrane_proof,
        network_seed,
        properties,
    })
}

/// Sends `req` and decodes a successful response as `T`. A non-2xx status is
/// reported WITH the response body — the joining service's structured error
/// (`{ "error": { "code": ..., "message": ... } }`) — rather than just the
/// status code: `reqwest::Response::error_for_status()` alone discards the
/// body, which is the only place a rejection reason (`unknown_network`,
/// `join_rejected`, ...) is carried.
async fn send_json<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
    what: &str,
) -> Result<T> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("{what} request"))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("{what} returned {status}: {body}");
    }
    resp.json()
        .await
        .with_context(|| format!("decoding {what} response"))
}

/// Run the full join + provision flow against `joining_url` for `agent_key`,
/// signing the challenge nonce with `signer`. `network` is the release's
/// registered `happ_id` (`publish-joining-modifiers.sh`), sent on `POST /join`
/// so the join lands on the release's own network rather than the joining
/// service's static default. Returns `role_name`'s membrane proof + modifiers
/// for the install, erroring if the response's `roles` map carries no entry
/// for it.
pub async fn join_and_provision(
    client: &reqwest::Client,
    joining_url: &str,
    agent_key: &AgentPubKey,
    signer: &dyn NonceSigner,
    role_name: &str,
    network: &str,
) -> Result<Provision> {
    let base = joining_url.trim_end_matches('/');
    let agent_b64 = AgentPubKeyB64::from(agent_key.clone()).to_string();

    // Step 1: POST /join.
    let join: JoinResponse = send_json(
        client
            .post(format!("{base}/join"))
            .json(&serde_json::json!({ "agent_key": agent_b64, "network": network })),
        "POST /join",
    )
    .await?;

    let session =
        match join.status.as_str() {
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
                let verify: VerifyResponse = send_json(
                client.post(format!("{base}/join/{}/verify", join.session)).json(
                    &serde_json::json!({ "challenge_id": challenge.id, "response": signature }),
                ),
                "POST /join/:session/verify",
            )
            .await?;
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
    let provision: ProvisionResponse = send_json(
        client.get(format!("{base}/join/{session}/provision")),
        "GET /join/:session/provision",
    )
    .await?;

    provision_for_role(provision, role_name)
}

/// A `reqwest` client with a sane timeout for the joining-service calls.
pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building joining-service HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;
    use holochain_types::prelude::SerializedBytes;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    fn decode(body: &str) -> ProvisionResponse {
        serde_json::from_str(body).expect("decode provision")
    }

    struct NeverCalledSigner;
    impl NonceSigner for NeverCalledSigner {
        fn sign_nonce(&self, _nonce_b64: &str) -> Result<String> {
            unreachable!("an already-ready join issues no challenge to sign")
        }
    }

    /// Reads one HTTP/1.1 request off `socket` and returns its body, using
    /// Content-Length to know how far past the header terminator to read.
    async fn read_request_body(socket: &mut TcpStream) -> String {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert_ne!(
                n, 0,
                "peer closed mid-request (before headers/body completed)"
            );
            buf.extend_from_slice(&chunk[..n]);
            if let Some(header_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                let content_length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                let body_start = header_end + 4;
                while buf.len() < body_start + content_length {
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(
                        n, 0,
                        "peer closed mid-body (shorter than its own Content-Length)"
                    );
                    buf.extend_from_slice(&chunk[..n]);
                }
                return String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                    .to_string();
            }
        }
    }

    async fn write_response(socket: &mut TcpStream, status_line: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
    }

    async fn write_json_response(socket: &mut TcpStream, body: &str) {
        write_response(socket, "200 OK", body).await;
    }

    /// The wire-level proof for this change: `join_and_provision` must send the
    /// release's registered network on `POST /join` alongside the agent key —
    /// omitting it is exactly what makes the joining service silently resolve
    /// its static default network instead of the release's own.
    #[tokio::test]
    async fn join_and_provision_sends_the_configured_network_on_join() {
        let captured = Arc::new(Mutex::new(None));
        let captured_writer = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // POST /join — capture the body, answer already-ready (no challenge).
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_request_body(&mut socket).await;
            *captured_writer.lock().unwrap() = Some(body);
            write_json_response(&mut socket, r#"{"session":"s1","status":"ready"}"#).await;

            // GET /join/s1/provision.
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_json_response(
                &mut socket,
                r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let agent_key = AgentPubKey::from_raw_36(vec![9; 36]);
        let base = format!("http://{addr}");
        let provision = join_and_provision(
            &client,
            &base,
            &agent_key,
            &NeverCalledSigner,
            "alliance",
            "v0.99.0",
        )
        .await
        .expect("join_and_provision succeeds against the fixture");

        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));

        let body = captured
            .lock()
            .unwrap()
            .clone()
            .expect("the join request body was captured");
        let json: serde_json::Value = serde_json::from_str(&body).expect("join body is JSON");
        assert_eq!(json["network"], "v0.99.0");
        assert_eq!(
            json["agent_key"],
            AgentPubKeyB64::from(agent_key).to_string()
        );
    }

    /// A rejected join (e.g. an unregistered `network`) must surface the
    /// joining service's own reason in the error, not just the bare status
    /// code — `error_for_status()` alone would discard the body carrying it.
    #[tokio::test]
    async fn join_and_provision_surfaces_the_error_bodys_reason() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_response(
                &mut socket,
                "400 Bad Request",
                r#"{"error":{"code":"unknown_network","message":"network not registered"}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let agent_key = AgentPubKey::from_raw_36(vec![9; 36]);
        let base = format!("http://{addr}");
        let err = join_and_provision(
            &client,
            &base,
            &agent_key,
            &NeverCalledSigner,
            "alliance",
            "v0.2.0",
        )
        .await
        .expect_err("a 400 from /join must fail the call");

        let msg = format!("{err:#}");
        assert!(msg.contains("unknown_network"), "{msg}");
        assert!(msg.contains("network not registered"), "{msg}");
    }

    /// The decisive regression test: a payload shaped exactly like the real
    /// `roles`-keyed endpoint round-trips a role's proof and properties through
    /// to the `Provision` the install path consumes — proving the fix reaches
    /// where the DNA hash is decided, not just the decode step.
    #[test]
    fn a_real_roles_shaped_payload_reaches_the_install_path() {
        // Deliberately NOT alphabetical — a decode through a sorted map would
        // swap these two and silently change the hash.
        let body = r#"{
            "linker_urls": [],
            "happ_bundle_url": "https://example/unyt.happ",
            "network_config": { "auth_server_url": "https://auth.example" },
            "roles": {
                "alliance": {
                    "membrane_proof": "cHJvb2Y=",
                    "dna_modifiers": {
                        "network_seed": "unyt-local-testnet-b",
                        "properties": {
                            "progenitor_pubkey": "uhCAkfake",
                            "joining_server_signer": "uhCAkalso"
                        }
                    }
                }
            }
        }"#;
        let provision =
            provision_for_role(decode(body), "alliance").expect("alliance role present");

        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));
        assert_eq!(
            provision.network_seed.as_deref(),
            Some("unyt-local-testnet-b")
        );

        // Hand-computed msgpack for that map in WIRE order — an INDEPENDENT
        // oracle. Re-encoding the same JSON through the same decoder would pass
        // even if it sorted the keys, which is precisely the failure to catch.
        // 0x82 = 2-entry fixmap; 0xb1 / 0xb5 / 0xa9 = fixstr of 17 / 21 / 9 bytes.
        let mut expected = vec![0x82_u8, 0xb1];
        expected.extend_from_slice(b"progenitor_pubkey");
        expected.push(0xa9);
        expected.extend_from_slice(b"uhCAkfake");
        expected.push(0xb5);
        expected.extend_from_slice(b"joining_server_signer");
        expected.push(0xa9);
        expected.extend_from_slice(b"uhCAkalso");
        let encoded = SerializedBytes::try_from(provision.properties.expect("properties present"))
            .expect("encode properties");
        assert_eq!(
            encoded.bytes(),
            &expected,
            "the network's properties must encode byte-identically to the wire order — \
             a sorted decode would put joining_server_signer first and change the DNA hash"
        );
    }

    /// The regression this whole fix is for: the RETIRED top-level
    /// `membrane_proofs`/`dna_modifiers` shape must not decode to an empty
    /// `roles` map that then silently yields `None`s. Pointed at the old shape,
    /// resolving any role must error, not install on absent modifiers.
    #[test]
    fn the_retired_top_level_shape_errors_instead_of_decoding_to_empty() {
        let old_shape = r#"{
            "membrane_proofs": { "alliance": "cHJvb2Y=" },
            "dna_modifiers": {
                "network_seed": "unyt-local-testnet-b",
                "properties": { "progenitor_pubkey": "uhCAkfake" }
            }
        }"#;
        let err = provision_for_role(decode(old_shape), "alliance")
            .expect_err("the old shape carries no `roles` key and must not resolve a role");
        assert!(
            format!("{err:#}").contains("alliance"),
            "the error must name the role that could not be resolved: {err:#}"
        );
    }

    /// A `roles` map present but missing the migrating role (e.g. the network
    /// configured a different role name) must error by name, not silently hand
    /// back `None`s for a role that in fact exists under a different key.
    #[test]
    fn a_roles_map_missing_the_configured_role_errors_by_name() {
        let body = r#"{ "roles": { "some_other_role": { "membrane_proof": "cHJvb2Y=" } } }"#;
        let err = provision_for_role(decode(body), "alliance")
            .expect_err("alliance is not in the roles map");
        let msg = format!("{err:#}");
        assert!(msg.contains("alliance"), "{msg}");
        assert!(msg.contains("some_other_role"), "{msg}");
    }

    /// A `roles` map carrying MORE than one role must resolve the NAMED one, not
    /// whichever entry a map iterator happens to yield first. With every other
    /// fixture in this file a single-entry map, only this one would catch a
    /// regression to an order-dependent lookup (e.g. `.values().next()`).
    #[test]
    fn provision_for_role_picks_the_named_role_out_of_several() {
        let body = r#"{
            "roles": {
                "some_other_role": { "membrane_proof": "b3RoZXI=" },
                "alliance": { "membrane_proof": "cHJvb2Y=" }
            }
        }"#;
        let provision =
            provision_for_role(decode(body), "alliance").expect("alliance role present");
        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));
    }

    /// A provision body with no modifiers at all leaves both fields unset — the
    /// install then sends no override and the manifest's values stand.
    #[test]
    fn provision_without_modifiers_yields_no_properties() {
        // Three shapes a joining service can legitimately send, all meaning "no
        // properties to apply" — the install must send no properties override for
        // each, leaving the manifest's value alone rather than overwriting it.
        let no_modifiers =
            provision_for_role(decode(r#"{ "roles": { "alliance": {} } }"#), "alliance")
                .expect("alliance role present");
        assert!(no_modifiers.properties.is_none());

        let modifiers_without_properties = provision_for_role(
            decode(r#"{ "roles": { "alliance": { "dna_modifiers": { "network_seed": "s" } } } }"#),
            "alliance",
        )
        .expect("alliance role present");
        assert!(modifiers_without_properties.properties.is_none());

        let explicit_null = provision_for_role(
            decode(r#"{ "roles": { "alliance": { "dna_modifiers": { "properties": null } } } }"#),
            "alliance",
        )
        .expect("alliance role present");
        assert!(explicit_null.properties.is_none());
    }

    /// A role present with no `membrane_proof` key is legitimate (the joining
    /// service omits it for a role with no configured DNA hash) — it must carry
    /// through as `None`, for the install-time validator to accept or reject,
    /// rather than erroring at decode time.
    #[test]
    fn a_role_with_no_membrane_proof_carries_through_as_none() {
        let provision = provision_for_role(
            decode(r#"{ "roles": { "alliance": { "dna_modifiers": { "network_seed": "s" } } } }"#),
            "alliance",
        )
        .expect("alliance role present, even with no proof");
        assert!(provision.membrane_proof.is_none());
    }

    /// An EMPTY properties map is not the same as absent: it is a real value the
    /// DNA hashes (msgpack `0x80`), and the TS installer sends it too (`{}` is
    /// truthy in JS), so parity requires carrying it through rather than
    /// collapsing it to `None`.
    #[test]
    fn an_empty_properties_map_is_carried_not_collapsed_to_none() {
        let provision = provision_for_role(
            decode(r#"{ "roles": { "alliance": { "dna_modifiers": { "properties": {} } } } }"#),
            "alliance",
        )
        .expect("alliance role present");
        let props = provision
            .properties
            .expect("an empty map is Some, not None");
        assert_eq!(
            SerializedBytes::try_from(props)
                .expect("encode properties")
                .bytes(),
            &vec![0x80_u8],
            "an empty properties map encodes as msgpack 0x80 — the same bytes the \
             TS installer's `{{}}` produces"
        );
    }
}
