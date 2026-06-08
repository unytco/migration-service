//! HTTP↔zome mapping tests for `/v1/notarize` + `/healthz`, driving the real
//! `router()` with a mock `Conductor` (no Holochain conductor needed).
//!
//! `Signature`/`Timestamp` come from `hdi::prelude` (same hdi 0.7.1 rave_engine
//! uses); hashes from `holo_hash`. Confirm these resolve on the first
//! `cargo test` against the pinned `rave_engine` branch.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

use migration_notary::conductor::Conductor;
use migration_notary::http::{router, AppState};

use rave_engine::types::entries::migration::v0_1::{
    MigrationInitRequest, NotaryReadRequest, NotaryReadResponse, SummaryState, SummaryStatePayload,
    SummaryTx,
};

const TOKEN: &str = "test-token";

/// A real, checksum-valid `AgentPubKeyB64` (36 core bytes → `uhCAk…` with the
/// correct type prefix + location). A hand-typed literal fails `AgentPubKeyB64`'s
/// deserialize (length/checksum), so the handler would reject the body as 400
/// before the mock conductor is ever consulted.
fn agent_b64() -> String {
    // from_raw_32 computes the 4-byte location; AgentPubKeyB64 deserialize verifies it.
    holo_hash::AgentPubKeyB64::from(holo_hash::AgentPubKey::from_raw_32(vec![0u8; 32])).to_string()
}

/// Mock conductor returning a preset response (or error) for one call.
struct MockConductor {
    ping_ok: bool,
    response: Mutex<Option<anyhow::Result<NotaryReadResponse>>>,
}

impl MockConductor {
    fn with(resp: anyhow::Result<NotaryReadResponse>) -> Arc<Self> {
        Arc::new(Self {
            ping_ok: true,
            response: Mutex::new(Some(resp)),
        })
    }

    /// A conductor whose `ping()` fails — drives the `/healthz` down path.
    fn down() -> Arc<Self> {
        Arc::new(Self {
            ping_ok: false,
            response: Mutex::new(None),
        })
    }
}

#[async_trait]
impl Conductor for MockConductor {
    async fn ping(&self) -> anyhow::Result<()> {
        if self.ping_ok {
            Ok(())
        } else {
            anyhow::bail!("down")
        }
    }
    async fn notary_read_predecessor_close(
        &self,
        _req: NotaryReadRequest,
    ) -> anyhow::Result<NotaryReadResponse> {
        self.response
            .lock()
            .unwrap()
            .take()
            .expect("response consumed once")
    }
}

fn state(conductor: Arc<dyn Conductor>) -> AppState {
    AppState {
        conductor,
        bearer_token: Arc::new(TOKEN.to_string()),
    }
}

fn notarize_req(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/notarize")
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(format!(
        r#"{{"agent_pubkey":"{}"}}"#,
        agent_b64()
    )))
    .unwrap()
}

/// A `/v1/notarize` request with an arbitrary raw body (for the malformed-input
/// path), bearer-authed so it reaches the body parse.
fn notarize_req_raw(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/notarize")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn healthz_req() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap()
}

async fn send(
    conductor: Arc<dyn Conductor>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let resp = router(state(conductor)).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn dummy_summary_state() -> SummaryState {
    SummaryState {
        opening_balance: Default::default(),
        opening_carry_forward_units: Default::default(),
        closing_balance: Default::default(),
        closing_carry_forward_units: Default::default(),
        summary_tx: SummaryTx {
            proposals: vec![],
            commitments: vec![],
            accepts: vec![],
            receipts: vec![],
            rejects: vec![],
            reclaims: vec![],
            spend_links: vec![],
        },
        agreement_carry_forward: vec![],
    }
}

#[tokio::test]
async fn missing_bearer_is_401() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(c, notarize_req(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "auth_failed");
}

#[tokio::test]
async fn wrong_bearer_is_401() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(c, notarize_req(Some("nope"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "auth_failed");
}

#[tokio::test]
async fn verified_is_200_with_payload_and_signature() {
    let payload = SummaryStatePayload {
        agent_pubkey: holo_hash::AgentPubKey::from_raw_36(vec![3; 36]),
        dna_hash: holo_hash::DnaHash::from_raw_36(vec![1; 36]),
        closing_state: dummy_summary_state(),
        last_action: holo_hash::ActionHash::from_raw_36(vec![2; 36]),
    };
    let signature = hdi::prelude::Signature([0u8; 64]);
    let c = MockConductor::with(Ok(NotaryReadResponse::Verified { payload, signature }));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("payload").is_some());
    assert!(body.get("signature").is_some());
}

#[tokio::test]
async fn no_close_found_is_404() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "no_close_found");
}

#[tokio::test]
async fn warranted_is_422() {
    let c = MockConductor::with(Ok(NotaryReadResponse::Warranted(vec![])));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "warranted");
}

#[tokio::test]
async fn too_new_is_409() {
    let earliest = hdi::prelude::Timestamp::from_micros(1);
    let c = MockConductor::with(Ok(NotaryReadResponse::TooNew {
        earliest_acceptable: earliest,
    }));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "too_new");
}

#[tokio::test]
async fn unable_to_verify_is_503() {
    let c = MockConductor::with(Ok(NotaryReadResponse::UnableToVerify));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "unable_to_verify");
}

#[tokio::test]
async fn zome_error_is_500_internal() {
    let c = MockConductor::with(Err(anyhow::anyhow!("boom")));
    let (status, body) = send(c, notarize_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal");
}

/// Wire round-trip smoke (the one thing the mocked HTTP tests bypass): the
/// envelope the daemon's `Verified` branch serializes (a `payload` plus a
/// `signature`) must decode back into the app's `MigrationInitRequest` using
/// the same `rave_engine` v0_1 types. Locks the agent-bound payload shape
/// (`agent_pubkey` and `closing_state.agreement_carry_forward`) against silent
/// serde drift between the daemon output and the app's
/// `migration_init_with_signature` input. A live-conductor end-to-end smoke
/// remains BACKLOG B26.
#[test]
fn verified_envelope_round_trips_into_migration_init_request() {
    let payload = SummaryStatePayload {
        agent_pubkey: holo_hash::AgentPubKey::from_raw_36(vec![3; 36]),
        dna_hash: holo_hash::DnaHash::from_raw_36(vec![1; 36]),
        closing_state: dummy_summary_state(),
        last_action: holo_hash::ActionHash::from_raw_36(vec![2; 36]),
    };
    let signature = hdi::prelude::Signature([7u8; 64]);

    // Exactly what `notarize`'s 200 branch emits.
    let envelope = serde_json::json!({ "payload": payload, "signature": signature });

    // Exactly what the app builds from the router's verbatim forward.
    let req: MigrationInitRequest = serde_json::from_value(envelope)
        .expect("daemon Verified envelope must decode into MigrationInitRequest");

    assert_eq!(
        req.payload, payload,
        "payload must survive the wire round-trip"
    );
    assert_eq!(req.signature, signature);
}

// C3 — `/healthz` reflects conductor liveness.

#[tokio::test]
async fn healthz_is_200_when_conductor_up() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(c, healthz_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn healthz_is_503_when_conductor_down() {
    let (status, body) = send(MockConductor::down(), healthz_req()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "internal");
}

// C3 + B5 — client-side input errors short-circuit to a distinct `bad_request`
// 4xx (so the router hard-stops instead of fanning across every notary).

#[tokio::test]
async fn unparseable_body_is_400_bad_request() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(c, notarize_req_raw("not json{")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn invalid_agent_pubkey_is_400_bad_request() {
    let c = MockConductor::with(Ok(NotaryReadResponse::NoCloseFound));
    let (status, body) = send(
        c,
        notarize_req_raw(r#"{"agent_pubkey":"not-a-valid-b64-key"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}
