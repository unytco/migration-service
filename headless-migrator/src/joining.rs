//! Fresh membrane proof for the carried key from the TARGET release's joining
//! service. The old proof is never reused, since proof requirements can change
//! per version; only the agent key is continuous. The flow mirrors the fleet's
//! own (`automation/packages/unyt-deploy`), down to the `lair-sign` invocation
//! that answers the `agent_allow_list` challenge.
//!
//! `POST /join` is NOT idempotent: for a key that already holds a ready session
//! it answers `409 agent_already_joined` and names `POST /reconnect` as the way
//! through, and a ready session never expires. So [`provision_agent`] joins, or
//! reconnects when that 409 says this key is already in, the order the unyt
//! app's `recoverAlreadyJoined` runs in. The app can also reconnect BEFORE
//! joining (`tryReconnect`), off a locally held agent key; there is no such
//! state here, so only the 409 can tell us.

use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use holo_hash::{AgentPubKey, AgentPubKeyB64};
use holochain_types::prelude::{Timestamp, YamlProperties};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::dna_errors::{
    joining_code_is_retryable, ErrorEnvelope, AGENT_ALREADY_JOINED, AGENT_NOT_JOINED,
};

/// A joining failure, typed by whether trying again could ever answer
/// differently. The open service retries a transient failure without bound, so
/// a refusal only an operator can lift has to be distinguishable HERE: mapped
/// onto that arm it loops forever on a back-off line, where a nonzero exit and
/// the service's own reason belong.
///
/// Deliberately NOT a `std::error::Error`: that impl would let anyhow's blanket
/// conversion absorb a `JoinError` on any caller's `?`, silently dropping the
/// classification this type exists to carry.
#[derive(Debug)]
pub enum JoinError {
    /// The service refused this agent as it stands and will keep refusing it.
    Permanent(anyhow::Error),
    /// Transport failure, a 5xx, or a refusal scoped to one session or challenge.
    Transient(anyhow::Error),
}

impl JoinError {
    pub fn cause(&self) -> &anyhow::Error {
        match self {
            Self::Permanent(e) | Self::Transient(e) => e,
        }
    }
}

/// What the joining service returns from `provision` for the migrating role.
#[derive(Debug, Clone, Default)]
pub struct Provision {
    /// `None` where the role needs no proof: the joining service omits it for a
    /// role with no configured DNA hash.
    pub membrane_proof: Option<String>,
    pub network_seed: Option<String>,
    /// Both modifiers are hashed into the DNA hash, so the install must apply
    /// these; the happ manifest declares none. `YamlProperties` rather than a
    /// `serde_json::Value` because its IndexMap keeps the wire order, and a
    /// reordered map msgpack-encodes to a different DNA.
    pub properties: Option<YamlProperties>,
}

/// Signs with the carried key, returning the base64 ed25519 signature.
///
/// The argument is the base64 of the bytes to sign, and the signature covers
/// those DECODED bytes: that is what `lair-sign --data` does. So the join
/// passes the challenge's own base64 nonce, and the reconnect passes
/// `base64(timestamp)`, because the service verifies that one over the
/// timestamp's raw UTF-8.
pub trait NonceSigner {
    fn sign_nonce(&self, nonce_b64: &str) -> Result<String>;
}

/// The production signer: `lair-sign` against the local lair, the same command
/// the fleet's deploy runs over SSH.
pub struct LairSigner {
    pub connection_url: String,
    pub passphrase: String,
    /// The carried key's ed25519 component, base64, as `lair-sign --pub-key`
    /// expects.
    pub pub_key_ed25519_b64: String,
}

impl LairSigner {
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

/// The ed25519 portion of a holo_hash agent key, base64, for `lair-sign`'s
/// `--pub-key`: the 32 core bytes, without the prefix or the location suffix.
pub fn agent_key_to_ed25519_b64(agent_key: &AgentPubKey) -> String {
    base64_of(agent_key.get_raw_32())
}

fn base64_of(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
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

/// Pull `role_name`'s data out of the decoded response. A body shaped for a
/// different wire contract (the retired top-level `membrane_proofs` keys)
/// decodes to an EMPTY `roles` map rather than failing, so a missing role has
/// to error here instead of reaching the install as `None`s.
fn provision_for_role(
    mut response: ProvisionResponse,
    role_name: &str,
) -> std::result::Result<Provision, JoinError> {
    let role = response.roles.remove(role_name).ok_or_else(|| {
        JoinError::Permanent(anyhow!(
            "joining-service provision response has no entry for role '{role_name}' in its \
             roles map (roles present: {:?}), so its membrane proof and DNA modifiers are unknown",
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

/// A failed call, keeping what the answer WAS structurally rather than only as
/// message text, because a caller may act on it: `agent_already_joined` routes
/// to the reconnect, and the status is how [`reconnect_outcome`] tells a missing
/// route from a refusal. Both are `None` on a transport failure, and `code`
/// alone is `None` when the body carried no error envelope.
struct WireFailure {
    status: Option<StatusCode>,
    code: Option<String>,
    error: JoinError,
}

impl WireFailure {
    fn is(&self, code: &str) -> bool {
        self.code.as_deref() == Some(code)
    }

    /// The service answered, but in nobody's envelope, so no code can be
    /// attributed to it: a proxy's page, or a framework's own default handler.
    fn unattributable(&self) -> bool {
        self.code.is_none() && self.status.is_some()
    }

    fn transient(e: anyhow::Error) -> Self {
        Self {
            status: None,
            code: None,
            error: JoinError::Transient(e),
        }
    }
}

/// Every step but the join wants only the classification, so `?` drops the code.
impl From<WireFailure> for JoinError {
    fn from(failure: WireFailure) -> Self {
        failure.error
    }
}

/// Sends `req` and decodes a 2xx as `T`. A non-2xx is reported WITH its body:
/// `error_for_status()` alone discards it, and it is the only place the service
/// carries a rejection reason.
async fn send_json<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
    what: &str,
) -> std::result::Result<T, WireFailure> {
    let resp = req.send().await.map_err(|e| {
        WireFailure::transient(anyhow::Error::new(e).context(format!("{what} request")))
    })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|e| format!("<response body unreadable: {e}>"));
        return Err(refusal_outcome(what, status, &body));
    }
    // A 200 that won't decode is our-side drift: retryable, since a flaky body
    // must not kill the migration.
    resp.json().await.map_err(|e| {
        WireFailure::transient(anyhow::Error::new(e).context(format!("decoding {what} response")))
    })
}

/// Classify a non-2xx by the joining service's OWN error code, the way [`fetch`]
/// classifies the router's, rather than by the HTTP status alone.
///
/// Two signals have to agree before a failure counts as permanent. The body must
/// carry the service's error envelope, since a bare 404 is also what the tunnel
/// in front of it answers from its `http_status:404` catch-all while the route
/// is coming up, and that is nobody refusing anything. The status must be a
/// client error, since a 5xx is the service failing rather than judging.
///
/// [`fetch`]: crate::fetch
fn refusal_outcome(what: &str, status: StatusCode, body: &str) -> WireFailure {
    let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(body) else {
        return WireFailure {
            status: Some(status),
            code: None,
            error: JoinError::Transient(anyhow!("{what} returned {status}: {body}")),
        };
    };
    let code = envelope.error.code;
    let message = if envelope.error.message.is_empty() {
        code.clone()
    } else {
        envelope.error.message
    };
    let reported = anyhow!("{what} returned {status} {code}: {message}");
    let error = if status.is_client_error() && !joining_code_is_retryable(&code) {
        JoinError::Permanent(reported)
    } else {
        JoinError::Transient(reported)
    };
    WireFailure {
        status: Some(status),
        code: Some(code),
        error,
    }
}

/// The `POST /join` body. `network` is the service's own field name for the
/// happ_id a joiner asks to join, so the wire key stays theirs while our side
/// carries the name `automation` uses for the same value.
#[derive(Serialize)]
struct JoinRequest<'a> {
    agent_key: &'a str,
    #[serde(rename = "network")]
    joining_service_happ_id: &'a str,
}

/// Get `role_name`'s membrane proof and DNA modifiers for `agent_key`, joining
/// or reconnecting as the service's answer requires. Errors if the provision
/// response's `roles` map carries no entry for `role_name`.
pub async fn provision_agent(
    client: &reqwest::Client,
    joining_url: &str,
    agent_key: &AgentPubKey,
    signer: &dyn NonceSigner,
    role_name: &str,
    joining_service_happ_id: &str,
) -> std::result::Result<Provision, JoinError> {
    let base = joining_url.trim_end_matches('/');
    let agent_b64 = AgentPubKeyB64::from(agent_key.clone()).to_string();

    let join = send_json::<JoinResponse>(
        client.post(format!("{base}/join")).json(&JoinRequest {
            agent_key: &agent_b64,
            joining_service_happ_id,
        }),
        "POST /join",
    )
    .await;

    let session = match join {
        Ok(join) => session_from_join(client, base, join, signer).await?,
        Err(failure) if failure.is(AGENT_ALREADY_JOINED) => {
            tracing::info!(
                network = joining_service_happ_id,
                "this key has already joined; reconnecting for a fresh provision"
            );
            reconnect_session(client, base, &agent_b64, signer, joining_service_happ_id).await?
        }
        Err(failure) => return Err(failure.into()),
    };

    let provision: ProvisionResponse = send_json(
        client.get(format!("{base}/join/{session}/provision")),
        "GET /join/:session/provision",
    )
    .await?;

    provision_for_role(provision, role_name)
}

async fn session_from_join(
    client: &reqwest::Client,
    base: &str,
    join: JoinResponse,
    signer: &dyn NonceSigner,
) -> std::result::Result<String, JoinError> {
    match join.status.as_str() {
        "ready" => Ok(join.session),
        "pending" => {
            // The challenge set comes from the network's configured auth methods,
            // so a set this flow cannot answer is the same on every pass.
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

            let verify: VerifyResponse = send_json(
                client
                    .post(format!("{base}/join/{}/verify", join.session))
                    .json(&serde_json::json!({
                        "challenge_id": challenge.id,
                        "response": signature,
                    })),
                "POST /join/:session/verify",
            )
            .await?;
            // Neither other status changes on the next pass: a `rejected`
            // verify is the service's no, and a `pending` one means a second
            // challenge this single-challenge flow never answers.
            if verify.status != "ready" {
                return Err(JoinError::Permanent(anyhow!(
                    "join verify status {} (expected ready)",
                    verify.status
                )));
            }
            Ok(join.session)
        }
        // The service's considered no arrives on a 2xx, and needs an operator:
        // not on the allow list, no auth method that admits it, a wrong invite.
        "rejected" => {
            let detail = join
                .reason
                .map(|r| format!(" (reason: {r})"))
                .unwrap_or_default();
            Err(JoinError::Permanent(anyhow!(
                "the joining service rejected this join{detail}"
            )))
        }
        // `status` is a fixed enum upstream, so an unknown value is contract
        // drift: surfaced rather than retried forever.
        other => {
            let detail = join
                .reason
                .map(|r| format!(" (reason: {r})"))
                .unwrap_or_default();
            Err(JoinError::Permanent(anyhow!(
                "unexpected join status {other}{detail}"
            )))
        }
    }
}

/// The `POST /reconnect` body. `network` must be the SAME value the join sent:
/// the service scopes its session lookup by it, and another network answers
/// `agent_not_joined` for a key that has plainly joined.
#[derive(Serialize)]
struct ReconnectRequest<'a> {
    agent_key: &'a str,
    timestamp: &'a str,
    signature: &'a str,
    #[serde(rename = "network")]
    joining_service_happ_id: &'a str,
}

#[derive(Deserialize)]
struct ReconnectResponse {
    #[serde(default)]
    session: Option<String>,
}

/// Recover the ready session of a key the service has already admitted. Proof
/// of possession is a signature over a fresh timestamp rather than a
/// server-issued nonce, so nothing cached from the original join is needed.
async fn reconnect_session(
    client: &reqwest::Client,
    base: &str,
    agent_b64: &str,
    signer: &dyn NonceSigner,
    joining_service_happ_id: &str,
) -> std::result::Result<String, JoinError> {
    let timestamp = reconnect_timestamp();
    let signature = signer
        .sign_nonce(&base64_of(timestamp.as_bytes()))
        .map_err(JoinError::Transient)?;

    let reconnected: ReconnectResponse = send_json(
        client
            .post(format!("{base}/reconnect"))
            .json(&ReconnectRequest {
                agent_key: agent_b64,
                timestamp: &timestamp,
                signature: &signature,
                joining_service_happ_id,
            }),
        "POST /reconnect",
    )
    .await
    .map_err(reconnect_outcome)?;

    reconnected.session.ok_or_else(|| {
        JoinError::Transient(anyhow!(
            "the join was refused as already joined but the reconnect found no ready session on \
             network '{joining_service_happ_id}'; retrying"
        ))
    })
}

/// Classify a reconnect refusal. Two answers read differently here than under
/// the shared rules, because reaching this step at all is evidence they do not
/// have: `POST /join` answered in the service's own envelope on this same pass,
/// so the service, its router and the tunnel are all up.
///
/// A 404 or 405 is therefore an absent route, not the warming tunnel
/// [`refusal_outcome`] keeps retryable, and no retry creates a route.
///
/// `agent_not_joined` is permanent as an ANSWER but not as a SITUATION, since it
/// contradicts the 409 that sent us here. Upstream's joined-at-all lookup prefers
/// a ready session over a pending one across every network, and its scoped lookup
/// is byte-identical to the join's, so the two can only disagree while the store
/// is changing under us.
fn reconnect_outcome(failure: WireFailure) -> JoinError {
    let route_is_absent = failure.unattributable()
        && matches!(
            failure.status,
            Some(StatusCode::NOT_FOUND) | Some(StatusCode::METHOD_NOT_ALLOWED)
        );
    if route_is_absent {
        return JoinError::Permanent(cause_of(failure.error).context(
            "this joining service has no reconnect route, so a key it has already admitted can \
             never be provisioned again: either its `reconnect.enabled` is false or it predates \
             the route",
        ));
    }
    if failure.is(AGENT_NOT_JOINED) {
        return JoinError::Transient(cause_of(failure.error).context(
            "the join and the reconnect disagree on whether this key has joined; retrying",
        ));
    }
    failure.into()
}

/// The failure itself, dropping a classification about to be replaced.
fn cause_of(error: JoinError) -> anyhow::Error {
    match error {
        JoinError::Permanent(e) | JoinError::Transient(e) => e,
    }
}

/// The reconnect's freshness token, whole seconds because the service parses it
/// with JavaScript's `Date`, whose specified format stops at millisecond
/// precision.
fn reconnect_timestamp() -> String {
    let whole_seconds = crate::state_file::now_us().div_euclid(1_000_000);
    Timestamp::from_micros(whole_seconds * 1_000_000).to_string()
}

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

    /// Derives the signature from the nonce, so a test can assert WHICH nonce
    /// was signed without a keystore.
    struct EchoSigner;
    impl NonceSigner for EchoSigner {
        fn sign_nonce(&self, nonce_b64: &str) -> Result<String> {
            Ok(format!("signed:{nonce_b64}"))
        }
    }

    /// One request as a fixture sees it. The request LINE is the only place the
    /// session a step was addressed to appears.
    struct Request {
        line: String,
        body: String,
    }

    async fn read_request(socket: &mut TcpStream) -> Request {
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
                let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                let content_length: usize = head
                    .to_lowercase()
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
                return Request {
                    line: head.lines().next().unwrap_or_default().to_string(),
                    body: String::from_utf8_lossy(&buf[body_start..body_start + content_length])
                        .to_string(),
                };
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

    async fn captured_join_body(joining_service_happ_id: &str) -> serde_json::Value {
        let captured = Arc::new(Mutex::new(None));
        let captured_writer = captured.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let body = read_request(&mut socket).await.body;
            *captured_writer.lock().unwrap() = Some(body);
            write_json_response(&mut socket, r#"{"session":"s1","status":"ready"}"#).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_json_response(
                &mut socket,
                r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let base = format!("http://{addr}");
        let provision = provision_agent(
            &client,
            &base,
            &test_agent_key(),
            &NeverCalledSigner,
            "alliance",
            joining_service_happ_id,
        )
        .await
        .expect("provision_agent succeeds against the fixture");

        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));

        let body = captured
            .lock()
            .unwrap()
            .clone()
            .expect("the join request body was captured");
        serde_json::from_str(&body).expect("join body is JSON")
    }

    async fn join_failure(status_line: &'static str, body: &'static str) -> JoinError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_response(&mut socket, status_line, body).await;
        });

        let client = http_client().unwrap();
        provision_agent(
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

    /// Both shapes a fleet configures go through verbatim: a prod happ_id that
    /// merely reads like a version, and local-testnet's literal `unyt`.
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

    /// The service's considered no comes back on a 2xx, not an error status.
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

    #[tokio::test]
    async fn a_4xx_without_the_services_error_envelope_stays_transient() {
        let err = join_failure("404 Not Found", "<html><body>404 not found</body></html>").await;
        assert!(
            matches!(err, JoinError::Transient(_)),
            "an unattributable 404 must stay retryable: {:#}",
            err.cause()
        );
    }

    #[tokio::test]
    async fn an_unreachable_joining_service_is_transient() {
        // Bind then drop, so the port is one nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let client = http_client().unwrap();
        let err = provision_agent(
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

    /// Two a network's auth-method config produces on every pass, no
    /// `agent_allow_list` challenge and no nonce, plus a status upstream added
    /// after this binary was built.
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

    #[tokio::test]
    async fn a_refusal_at_the_provision_step_propagates() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_json_response(&mut socket, r#"{"session":"s1","status":"ready"}"#).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_response(
                &mut socket,
                "403 Forbidden",
                r#"{"error":{"code":"agent_revoked","message":"Agent has been blocked by administrator"}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let err = provision_agent(
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

    /// The fleet's real path, which no other test drives: a wrong challenge id
    /// or an unsigned nonce otherwise fails only against a live service.
    #[tokio::test]
    async fn the_allow_list_challenge_is_signed_verified_and_provisioned() {
        let verify_body = Arc::new(Mutex::new(None));
        let captured = verify_body.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_response(
                &mut socket,
                "201 Created",
                r#"{"session":"s1","status":"pending","challenges":[{"id":"ch_1","type":"agent_allow_list","metadata":{"nonce":"bm9uY2U="}}]}"#,
            )
            .await;

            let (mut socket, _) = listener.accept().await.unwrap();
            *captured.lock().unwrap() = Some(read_request(&mut socket).await.body);
            write_json_response(&mut socket, r#"{"status":"ready"}"#).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_json_response(
                &mut socket,
                r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#,
            )
            .await;
        });

        let client = http_client().unwrap();
        let provision = provision_agent(
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

    // ── The already-joined recovery ───────────────────────────────────────

    /// What every recovery fixture asks for, join and reconnect alike.
    const NETWORK: &str = "v0.99.0";

    /// Runs the whole flow against a fixture answering `script` in order, handing
    /// back the outcome and the request line and body of everything that arrived.
    async fn provision_against(
        script: Vec<(&'static str, String)>,
    ) -> (
        std::result::Result<Provision, JoinError>,
        Vec<(String, String)>,
    ) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let recorder = received.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for (status_line, body) in script {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request = read_request(&mut socket).await;
                recorder.lock().unwrap().push((request.line, request.body));
                write_response(&mut socket, status_line, &body).await;
            }
        });

        let client = http_client().unwrap();
        let outcome = provision_agent(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &EchoSigner,
            "alliance",
            NETWORK,
        )
        .await;
        let received = received.lock().unwrap().clone();
        (outcome, received)
    }

    /// Upstream's own refusal of a re-join, verbatim: the status, code and
    /// wording its `POST /v1/join` guard renders (`joining-service/src/app.ts`).
    fn already_joined_409() -> (&'static str, String) {
        (
            "409 Conflict",
            r#"{"error":{"code":"agent_already_joined","message":"This agent key has already completed joining this network. Use POST /v1/reconnect instead."}}"#
                .to_string(),
        )
    }

    fn ok(body: &str) -> (&'static str, String) {
        ("200 OK", body.to_string())
    }

    /// The scenario the reconnect exists for: the key joined on an earlier pass
    /// whose install then failed, and the run's bounded GD wait is spent
    /// re-attempting exactly this.
    #[tokio::test]
    async fn a_key_that_already_joined_reconnects_for_a_usable_provision() {
        let (outcome, received) = provision_against(vec![
            already_joined_409(),
            ok(r#"{"linker_urls":[],"http_gateways":[],"session":"s_reconnected"}"#),
            ok(r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#),
        ])
        .await;

        let provision = outcome.unwrap_or_else(|e| {
            panic!(
                "an already-joined key must still be provisioned: {:#}",
                e.cause()
            )
        });
        assert_eq!(provision.membrane_proof.as_deref(), Some("cHJvb2Y="));

        let paths: Vec<&str> = received.iter().map(|(line, _)| line.as_str()).collect();
        assert_eq!(
            paths.len(),
            3,
            "join, then reconnect, then provision: {paths:?}"
        );
        assert!(paths[0].starts_with("POST /join "), "{paths:?}");
        assert!(paths[1].starts_with("POST /reconnect "), "{paths:?}");
        assert!(
            paths[2].starts_with("GET /join/s_reconnected/provision "),
            "the proof must come from the session the reconnect handed back: {paths:?}"
        );
    }

    #[tokio::test]
    async fn the_reconnect_proves_key_possession_over_its_timestamp() {
        let (outcome, received) = provision_against(vec![
            already_joined_409(),
            ok(r#"{"linker_urls":[],"http_gateways":[],"session":"s_reconnected"}"#),
            ok(r#"{"roles":{"alliance":{"membrane_proof":"cHJvb2Y="}}}"#),
        ])
        .await;
        assert!(outcome.is_ok());

        let sent: serde_json::Value =
            serde_json::from_str(&received[1].1).expect("the reconnect body is JSON");
        assert_eq!(
            sent["agent_key"],
            AgentPubKeyB64::from(test_agent_key()).to_string()
        );
        assert_eq!(
            sent["network"], NETWORK,
            "a reconnect naming another network answers agent_not_joined"
        );
        let timestamp = sent["timestamp"].as_str().expect("a timestamp was sent");
        assert_eq!(
            sent["signature"],
            format!("signed:{}", base64_of(timestamp.as_bytes())),
            "the signer is handed the base64 of the timestamp, so lair signs the timestamp itself"
        );
    }

    #[test]
    fn the_reconnect_timestamp_is_a_whole_second_utc_instant() {
        let rendered = reconnect_timestamp();
        assert_eq!(
            rendered.len(),
            "2026-08-23T12:34:56Z".len(),
            "expected YYYY-MM-DDTHH:MM:SSZ: {rendered}"
        );
        assert!(rendered.ends_with('Z'), "must be UTC: {rendered}");
        assert!(
            !rendered.contains('.'),
            "sub-second digits are outside the format the service parses: {rendered}"
        );

        let parsed: Timestamp = rendered.parse().expect("a date the service can parse");
        let drift = (crate::state_file::now_us() - parsed.as_micros()).abs();
        assert!(
            drift < Duration::from_secs(5).as_micros() as i64,
            "the timestamp must be now, not a fixed instant: {rendered}"
        );
    }

    #[tokio::test]
    async fn a_reconnect_the_service_refuses_is_permanent() {
        let (outcome, _) = provision_against(vec![
            already_joined_409(),
            (
                "400 Bad Request",
                r#"{"error":{"code":"invalid_signature","message":"Signature does not verify against agent key"}}"#.to_string(),
            ),
        ])
        .await;

        let err = outcome.expect_err("a rejected signature cannot be provisioned");
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Permanent(_)), "{msg}");
        assert!(msg.contains("invalid_signature"), "{msg}");
        assert!(
            msg.contains("POST /reconnect"),
            "the failure must name the step it happened at, not the join: {msg}"
        );
    }

    /// Both shapes a reconnect that finds no session takes: the explicit
    /// refusal, and a 200 naming no session.
    #[tokio::test]
    async fn a_reconnect_that_finds_no_session_re_joins_on_the_next_pass() {
        for (second, says) in [
            (
                (
                    "403 Forbidden",
                    r#"{"error":{"code":"agent_not_joined","message":"This agent key has not joined network \"v0.99.0\""}}"#.to_string(),
                ),
                "disagree on whether this key has joined",
            ),
            (
                ("200 OK", r#"{"linker_urls":[],"http_gateways":[]}"#.to_string()),
                "found no ready session",
            ),
        ] {
            let (outcome, _) = provision_against(vec![already_joined_409(), second]).await;
            let err = outcome.expect_err("no session means no provision");
            let msg = format!("{:#}", err.cause());
            assert!(
                matches!(err, JoinError::Transient(_)),
                "a contradiction between the two calls must be retried, not fatal: {msg}"
            );
            // Transient is the fallthrough for almost everything here, so the
            // arm alone would pass for a body that simply failed to decode.
            assert!(msg.contains(says), "expected {says:?} in: {msg}");
        }
    }

    #[tokio::test]
    async fn a_reconnect_hitting_an_outage_stays_transient() {
        let (outcome, _) = provision_against(vec![
            already_joined_409(),
            (
                "503 Service Unavailable",
                r#"{"error":{"code":"service_unavailable","message":"Auth service check failed"}}"#
                    .to_string(),
            ),
        ])
        .await;

        let err = outcome.expect_err("an outage serves no provision");
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Transient(_)), "{msg}");
        assert!(
            msg.contains("service_unavailable"),
            "it must be the 5xx that was waited out, not an undecodable body: {msg}"
        );
    }

    #[tokio::test]
    async fn a_clock_the_service_will_not_accept_yet_waits_rather_than_ending_the_run() {
        let (outcome, _) = provision_against(vec![
            already_joined_409(),
            (
                "400 Bad Request",
                r#"{"error":{"code":"timestamp_out_of_range","message":"Timestamp is 431s from server time (max 300s)"}}"#.to_string(),
            ),
        ])
        .await;

        let err = outcome.expect_err("a rejected timestamp serves no provision");
        let msg = format!("{:#}", err.cause());
        assert!(
            matches!(err, JoinError::Transient(_)),
            "a clock that may still step must not hard-stop the run: {msg}"
        );
        assert!(
            msg.contains("431s from server time"),
            "the wait must carry the drift the service measured, so the repeating line \
             diagnoses itself: {msg}"
        );
    }

    /// An absent route arrives as a bare 404 in nobody's envelope, which the
    /// shared rule keeps retryable and `reconnect_outcome` must not.
    #[tokio::test]
    async fn a_joining_service_with_no_reconnect_route_ends_the_run() {
        for absent in [
            ("404 Not Found", "404 Not Found".to_string()),
            ("405 Method Not Allowed", String::new()),
        ] {
            let (outcome, _) = provision_against(vec![already_joined_409(), absent]).await;
            let err = outcome.expect_err("no route means no provision");
            let msg = format!("{:#}", err.cause());
            assert!(
                matches!(err, JoinError::Permanent(_)),
                "an absent reconnect route must not be waited on forever: {msg}"
            );
            assert!(
                msg.contains("no reconnect route"),
                "the operator has to be told what is missing: {msg}"
            );
        }
    }

    /// The opposite reading of the same shape, asserted beside the reconnect
    /// case so the two never drift into one rule.
    #[tokio::test]
    async fn an_unattributable_404_at_the_join_step_still_waits() {
        let err = join_failure("404 Not Found", "<html><body>404 not found</body></html>").await;
        assert!(matches!(err, JoinError::Transient(_)), "{:#}", err.cause());
    }

    #[tokio::test]
    async fn a_signer_that_cannot_reach_lair_yet_stays_transient() {
        struct FailingSigner;
        impl NonceSigner for FailingSigner {
            fn sign_nonce(&self, _nonce_b64: &str) -> Result<String> {
                bail!("lair-sign failed (exit status: 1): connection refused")
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (status_line, body) = already_joined_409();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_request(&mut socket).await;
            write_response(&mut socket, status_line, &body).await;
        });

        let client = http_client().unwrap();
        let err = provision_agent(
            &client,
            &format!("http://{addr}"),
            &test_agent_key(),
            &FailingSigner,
            "alliance",
            NETWORK,
        )
        .await
        .expect_err("nothing can be signed");
        let msg = format!("{:#}", err.cause());
        assert!(matches!(err, JoinError::Transient(_)), "{msg}");
        assert!(msg.contains("lair-sign failed"), "{msg}");
    }

    #[test]
    fn a_real_roles_shaped_payload_reaches_the_install_path() {
        // Deliberately NOT alphabetical: a decode through a sorted map would
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

        // Hand-computed msgpack in WIRE order, an INDEPENDENT oracle: re-encoding
        // the same JSON through the same decoder would pass even if it sorted the
        // keys. 0x82 = 2-entry fixmap; 0xb1 / 0xb5 / 0xa9 = fixstr of 17 / 21 / 9.
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

    /// Permanent because the service serves the same roles map to every later
    /// pass, so naming the role carefully is wasted if the caller retries.
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

    /// Every other fixture here is a single-entry map, so only this one catches
    /// a regression to an order-dependent lookup (e.g. `.values().next()`).
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

    /// Three shapes that all mean "no properties to apply", so the install sends
    /// no override and the manifest's value stands.
    #[test]
    fn provision_without_modifiers_yields_no_properties() {
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

    /// A role can legitimately carry no proof, so this must reach the
    /// install-time validator rather than erroring at decode.
    #[test]
    fn a_role_with_no_membrane_proof_carries_through_as_none() {
        let provision = provision_for_role(
            decode(r#"{ "roles": { "alliance": { "dna_modifiers": { "network_seed": "s" } } } }"#),
            "alliance",
        )
        .expect("alliance role present, even with no proof");
        assert!(provision.membrane_proof.is_none());
    }

    /// An empty map is a real value the DNA hashes, and the TS installer sends
    /// it too, so parity requires carrying it rather than collapsing it.
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
