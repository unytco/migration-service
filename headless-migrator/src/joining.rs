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
//!
//! Every failure leaving this module is typed by whether a later pass could
//! answer differently ([`JoinError`]), because the open service's retry of a
//! transient one is unbounded.

use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use holo_hash::{AgentPubKey, AgentPubKeyB64};
use holochain_types::prelude::YamlProperties;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::dna_errors::{joining_code_is_retryable, ErrorEnvelope};

/// A joining failure, typed by whether trying again could ever answer
/// differently. The open service retries a transient failure without bound, so
/// a refusal only an operator can lift has to be distinguishable HERE: mapped
/// onto that arm it loops forever, one repeating back-off line where a nonzero
/// exit and the joining service's own reason belong.
///
/// Deliberately NOT a `std::error::Error`. That impl would let anyhow's blanket
/// conversion absorb a `JoinError` on any caller's `?`, silently dropping the
/// classification this type exists to carry; without it, every caller has to
/// decide, and the module's own `?` cannot erase one either.
#[derive(Debug)]
pub enum JoinError {
    /// The service refused this join as it stands, and will keep refusing it:
    /// an unregistered happ_id, an agent off the allow list, a key that has
    /// already joined, or a provision response carrying no entry for the role.
    Permanent(anyhow::Error),
    /// Transport failure, a 5xx, or a refusal scoped to one session or challenge.
    Transient(anyhow::Error),
}

impl JoinError {
    /// The failure itself, for a caller adding context or rendering the chain.
    pub fn cause(&self) -> &anyhow::Error {
        match self {
            Self::Permanent(e) | Self::Transient(e) => e,
        }
    }
}

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
fn provision_for_role(
    mut response: ProvisionResponse,
    role_name: &str,
) -> std::result::Result<Provision, JoinError> {
    let role = response.roles.remove(role_name).ok_or_else(|| {
        JoinError::Permanent(anyhow!(
            "joining-service provision response has no entry for role '{role_name}' in its \
             roles map (roles present: {:?}) — its membrane proof and DNA modifiers are unknown",
            response.roles.keys().collect::<Vec<_>>()
        ))
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
) -> std::result::Result<T, JoinError> {
    let resp = req.send().await.map_err(|e| {
        JoinError::Transient(anyhow::Error::new(e).context(format!("{what} request")))
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("<response body unreadable: {e}>"));
        return Err(refusal_outcome(what, status, &body));
    }
    // A 200 that won't decode is our-side drift, surfaced but retryable, exactly
    // as `fetch` treats the router's: a flaky body must not kill the migration.
    resp.json().await.map_err(|e| {
        JoinError::Transient(anyhow::Error::new(e).context(format!("decoding {what} response")))
    })
}

/// Classify a non-2xx by the joining service's OWN error code, the way [`fetch`]
/// classifies the router's, rather than by the HTTP status alone.
///
/// Two signals have to agree before a failure counts as permanent. The body must
/// carry the service's error envelope, since a 404 is also what the tunnel in
/// front of it answers while its route is still coming up, and that is nobody
/// refusing anything. The status must be a client error, since a 5xx is the
/// service failing rather than judging the request. Anything else retries.
///
/// [`fetch`]: crate::fetch
fn refusal_outcome(what: &str, status: StatusCode, body: &str) -> JoinError {
    let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(body) else {
        return JoinError::Transient(anyhow!("{what} returned {status}: {body}"));
    };
    let code = envelope.error.code;
    let message = if envelope.error.message.is_empty() {
        code.clone()
    } else {
        envelope.error.message
    };
    let reported = anyhow!("{what} returned {status} {code}: {message}");
    if status.is_client_error() && !joining_code_is_retryable(&code) {
        JoinError::Permanent(reported)
    } else {
        JoinError::Transient(reported)
    }
}

/// The `POST /join` body. `network` is the joining service's OWN field name for
/// the happ_id a joiner asks to join, so the wire key stays theirs while our
/// side carries the name automation uses for the same value.
#[derive(Serialize)]
struct JoinRequest<'a> {
    agent_key: &'a str,
    #[serde(rename = "network")]
    joining_service_happ_id: &'a str,
}

/// Run the full join + provision flow against `joining_url` for `agent_key`,
/// signing the challenge nonce with `signer`. `joining_service_happ_id` is the
/// happ_id the release registered on that service
/// (`publish-joining-modifiers.sh`), sent as the `network` field on `POST /join`
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
    joining_service_happ_id: &str,
) -> std::result::Result<Provision, JoinError> {
    let base = joining_url.trim_end_matches('/');
    let agent_b64 = AgentPubKeyB64::from(agent_key.clone()).to_string();

    // Step 1: POST /join.
    let join: JoinResponse = send_json(
        client.post(format!("{base}/join")).json(&JoinRequest {
            agent_key: &agent_b64,
            joining_service_happ_id,
        }),
        "POST /join",
    )
    .await?;

    let session =
        match join.status.as_str() {
            // Already cleared (e.g. an allow-list with no challenge) → provision.
            "ready" => join.session.clone(),
            "pending" => {
                // The challenge set comes from the network's configured auth
                // methods, so a set this flow cannot answer is the same set on
                // every pass: permanent, like an unparseable one.
                let challenge = join
                    .challenges
                    .iter()
                    .find(|c| c.challenge_type == "agent_allow_list")
                    .context("no agent_allow_list challenge in /join response")
                    .map_err(JoinError::Permanent)?;
                let nonce = challenge
                    .metadata
                    .as_ref()
                    .and_then(|m| m.nonce.as_deref())
                    .context("agent_allow_list challenge missing nonce")
                    .map_err(JoinError::Permanent)?;
                // Retryable on purpose, unlike its neighbours: this is the local
                // lair, which the droplet may still be bringing up.
                let signature = signer.sign_nonce(nonce).map_err(JoinError::Transient)?;

                // Step 3: POST /join/:session/verify.
                let verify: VerifyResponse = send_json(
                client.post(format!("{base}/join/{}/verify", join.session)).json(
                    &serde_json::json!({ "challenge_id": challenge.id, "response": signature }),
                ),
                "POST /join/:session/verify",
            )
            .await?;
                // `ready` is the only status this flow can carry forward: a
                // `rejected` verify is the service's no, and a `pending` one
                // means a second challenge our single-challenge flow never
                // answers. Neither changes on the next pass.
                if verify.status != "ready" {
                    return Err(JoinError::Permanent(anyhow!(
                        "join verify status {} (expected ready)",
                        verify.status
                    )));
                }
                join.session.clone()
            }
            // A 2xx `rejected` is the service's considered no: the agent is not on
            // the network's allow list, no configured auth method can admit it, or
            // its invite code is wrong. Each needs an operator, so none of them
            // clears by asking again.
            "rejected" => {
                let detail = join
                    .reason
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                return Err(JoinError::Permanent(anyhow!(
                    "the joining service rejected this join{detail}"
                )));
            }
            // `status` is a fixed enum upstream, so an unknown value is contract
            // drift. Surfaced rather than retried, the same call `fetch` makes
            // for a router error code this binary has never seen.
            other => {
                let detail = join
                    .reason
                    .map(|r| format!(" (reason: {r})"))
                    .unwrap_or_default();
                return Err(JoinError::Permanent(anyhow!(
                    "unexpected join status {other}{detail}"
                )));
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

    /// Stands in for lair, deriving the signature from the nonce so a test can
    /// assert WHICH nonce was signed without a keystore.
    struct EchoSigner;
    impl NonceSigner for EchoSigner {
        fn sign_nonce(&self, nonce_b64: &str) -> Result<String> {
            Ok(format!("signed:{nonce_b64}"))
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

    fn test_agent_key() -> AgentPubKey {
        AgentPubKey::from_raw_36(vec![9; 36])
    }

    /// Drives the happy path against a fixture answering an already-ready join
    /// and then a one-role provision, returning the `POST /join` body it sent.
    async fn captured_join_body(joining_service_happ_id: &str) -> serde_json::Value {
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
        let base = format!("http://{addr}");
        let provision = join_and_provision(
            &client,
            &base,
            &test_agent_key(),
            &NeverCalledSigner,
            "alliance",
            joining_service_happ_id,
        )
        .await
        .expect("join_and_provision succeeds against the fixture");

        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));

        let body = captured
            .lock()
            .unwrap()
            .clone()
            .expect("the join request body was captured");
        serde_json::from_str(&body).expect("join body is JSON")
    }

    /// Runs `POST /join` against a fixture answering `status_line` with `body`,
    /// and returns the failure that came back.
    async fn join_failure(status_line: &'static str, body: &'static str) -> JoinError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_response(&mut socket, status_line, body).await;
        });

        let client = http_client().unwrap();
        join_and_provision(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &NeverCalledSigner,
            "alliance",
            "v0.2.0",
        )
        .await
        .expect_err("the fixture answers a failure")
    }

    /// The wire-level proof for this change: `join_and_provision` must send the
    /// release's registered happ_id under the joining service's own `network`
    /// key, alongside the agent key. Omitting it is exactly what makes that
    /// service silently resolve its static default network instead of the
    /// release's own. Both shapes a fleet really configures are covered: prod
    /// registers under `release_version` verbatim (a happ_id that merely reads
    /// like a version), local-testnet under the static literal `unyt`.
    #[tokio::test]
    async fn join_sends_the_configured_happ_id_as_the_services_network_field() {
        for happ_id in ["v0.99.0", "unyt"] {
            let body = captured_join_body(happ_id).await;
            assert_eq!(body["network"], happ_id);
            assert_eq!(
                body["agent_key"],
                AgentPubKeyB64::from(test_agent_key()).to_string()
            );
        }
    }

    /// A 4xx names a configuration this joining service will go on refusing (an
    /// unregistered happ_id, a key off the allow list), so it must come back
    /// PERMANENT: on the caller's retry arm the open service asks again forever.
    /// It must also still carry the service's own reason: `error_for_status()`
    /// alone would discard the body holding it.
    #[tokio::test]
    async fn a_4xx_join_is_permanent_and_keeps_the_error_bodys_reason() {
        let err = join_failure(
            "400 Bad Request",
            r#"{"error":{"code":"unknown_network","message":"network not registered"}}"#,
        )
        .await;
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Permanent(_)), "{msg}");
        assert!(msg.contains("unknown_network"), "{msg}");
        assert!(msg.contains("network not registered"), "{msg}");
    }

    /// The joining service's considered no comes back on a 2xx: an agent off the
    /// release's allow list produces no challenges, which it reports as a
    /// `rejected` status with a reason. Only an operator can lift it.
    #[tokio::test]
    async fn a_rejected_join_status_is_permanent() {
        let err = join_failure(
            "201 Created",
            r#"{"session":"s1","status":"rejected","reason":"Agent is not eligible for this auth method"}"#,
        )
        .await;
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Permanent(_)), "{msg}");
        assert!(msg.contains("Agent is not eligible"), "{msg}");
    }

    /// A service failing, rather than judging the request, keeps the unbounded
    /// retry it has always had: its own 5xx, and the codes scoped to one session
    /// or challenge that the next pass re-creates from `POST /join`.
    #[tokio::test]
    async fn a_server_side_or_session_scoped_refusal_stays_transient() {
        for (status_line, body) in [
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"service_unavailable","message":"Auth service check failed"}}"#,
            ),
            (
                "500 Internal Server Error",
                r#"{"error":{"code":"internal_error","message":"Internal server error"}}"#,
            ),
            (
                "429 Too Many Requests",
                r#"{"error":{"code":"rate_limited","message":"Too many verification attempts"}}"#,
            ),
            (
                "401 Unauthorized",
                r#"{"error":{"code":"invalid_session","message":"Session not found or expired"}}"#,
            ),
            (
                "410 Gone",
                r#"{"error":{"code":"challenge_expired","message":"Challenge has expired"}}"#,
            ),
            (
                "403 Forbidden",
                r#"{"error":{"code":"not_ready","message":"Session status is pending"}}"#,
            ),
        ] {
            let err = join_failure(status_line, body).await;
            assert!(
                matches!(err, JoinError::Transient(_)),
                "{status_line} must stay retryable: {:#}",
                err.cause()
            );
        }
    }

    /// The tunnel in front of the joining service answers 404 from its catch-all
    /// while its route is still coming up, and a proxy answers HTML. Neither is
    /// the service refusing anything, and the missing error envelope is what says
    /// so: a 4xx nobody can attribute to the service must not end the run.
    #[tokio::test]
    async fn a_4xx_without_the_services_error_envelope_stays_transient() {
        let err = join_failure("404 Not Found", "<html><body>404 not found</body></html>").await;
        assert!(
            matches!(err, JoinError::Transient(_)),
            "an unattributable 404 must stay retryable: {:#}",
            err.cause()
        );
    }

    /// A joining service that isn't answering yet (the new droplet coming up
    /// beside it) is the transient case the retry arm exists for.
    #[tokio::test]
    async fn an_unreachable_joining_service_is_transient() {
        // Bind then drop, so the port is one nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let client = http_client().unwrap();
        let err = join_and_provision(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &NeverCalledSigner,
            "alliance",
            "unyt",
        )
        .await
        .expect_err("nothing is listening on that port");
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Transient(_)), "{msg}");
        assert!(
            msg.contains("POST /join request"),
            "it must fail at the transport, not somewhere later: {msg}"
        );
    }

    /// A network configured with auth methods this flow cannot answer issues a
    /// challenge set with no `agent_allow_list` entry, and issues the same set on
    /// every pass. Same for a challenge carrying no nonce, and for a status
    /// upstream has added since this binary was built.
    #[tokio::test]
    async fn a_join_this_flow_cannot_carry_forward_is_permanent() {
        for body in [
            r#"{"session":"s1","status":"pending","challenges":[{"id":"c1","type":"invite_code"}]}"#,
            r#"{"session":"s1","status":"pending","challenges":[{"id":"c1","type":"agent_allow_list"}]}"#,
            r#"{"session":"s1","status":"some_status_added_upstream"}"#,
        ] {
            let err = join_failure("201 Created", body).await;
            assert!(
                matches!(err, JoinError::Permanent(_)),
                "must not retry a join it can never carry forward: {:#}",
                err.cause()
            );
        }
    }

    /// The last step has its own refusals, and their classification has to reach
    /// the caller the way `POST /join`'s does rather than being swallowed or
    /// re-wrapped on the way out.
    #[tokio::test]
    async fn a_refusal_at_the_provision_step_propagates() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_json_response(&mut socket, r#"{"session":"s1","status":"ready"}"#).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_response(
                &mut socket,
                "403 Forbidden",
                r#"{"error":{"code":"agent_revoked","message":"Agent has been blocked by administrator"}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let err = join_and_provision(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &NeverCalledSigner,
            "alliance",
            "unyt",
        )
        .await
        .expect_err("a revoked agent cannot be provisioned");
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Permanent(_)), "{msg}");
        assert!(msg.contains("agent_revoked"), "{msg}");
        assert!(msg.contains("provision"), "{msg}");
    }

    /// The fleet's real path, which no other test drives: a pending join whose
    /// `agent_allow_list` nonce is signed with the carried key, verified, then
    /// provisioned. It pins what each step sends, since a wrong challenge id or
    /// an unsigned nonce otherwise fails only against a live service.
    #[tokio::test]
    async fn the_allow_list_challenge_is_signed_verified_and_provisioned() {
        let verify_body = Arc::new(Mutex::new(None));
        let captured = verify_body.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_response(
                &mut socket,
                "201 Created",
                r#"{"session":"s1","status":"pending","challenges":[{"id":"ch_1","type":"agent_allow_list","metadata":{"nonce":"bm9uY2U="}}]}"#,
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            *captured.lock().unwrap() = Some(read_request_body(&mut socket).await);
            write_json_response(&mut socket, r#"{"status":"ready"}"#).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request_body(&mut socket).await;
            write_json_response(
                &mut socket,
                r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let provision = join_and_provision(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &EchoSigner,
            "alliance",
            "unyt",
        )
        .await
        .expect("the signed challenge clears the join");
        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));

        let body = verify_body
            .lock()
            .unwrap()
            .clone()
            .expect("verify was sent");
        let json: serde_json::Value = serde_json::from_str(&body).expect("verify body is JSON");
        assert_eq!(json["challenge_id"], "ch_1");
        assert_eq!(
            json["response"], "signed:bm9uY2U=",
            "the nonce from THAT challenge is what gets signed"
        );
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
            format!("{:#}", err.cause()).contains("alliance"),
            "the error must name the role that could not be resolved: {:#}",
            err.cause()
        );
        assert!(matches!(err, JoinError::Permanent(_)));
    }

    /// A `roles` map present but missing the migrating role (e.g. the network
    /// configured a different role name) must error by name, not silently hand
    /// back `None`s for a role that in fact exists under a different key. It is
    /// PERMANENT: the service serves the same roles map to every later pass, so
    /// naming the role carefully is wasted if the caller then retries forever.
    #[test]
    fn a_roles_map_missing_the_configured_role_is_a_permanent_error_by_name() {
        let body = r#"{ "roles": { "some_other_role": { "membrane_proof": "cHJvb2Y=" } } }"#;
        let err = provision_for_role(decode(body), "alliance")
            .expect_err("alliance is not in the roles map");
        let msg = format!("{:#}", err.cause());
        assert!(msg.contains("alliance"), "{msg}");
        assert!(msg.contains("some_other_role"), "{msg}");
        assert!(matches!(err, JoinError::Permanent(_)), "{msg}");
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
