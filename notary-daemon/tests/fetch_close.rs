//! HTTP↔zome mapping tests for `/v1/fetch-close` + `/healthz`, driving the real
//! `router()` with a mock `Conductor` (no Holochain conductor needed).
//!
//! Hashes come from `holo_hash`; `Signature` from `hdi::prelude` (same hdi
//! rave_engine uses), so the wire shapes are identical to the zome-call
//! payloads.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // for `oneshot`

use migration_notary::conductor::Conductor;
use migration_notary::http::{router, AppState};

use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_1::{
    MigrationInitRequest, NotarySignature, ReadCloseResponse, SummaryState, SummaryStatePayload,
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

/// Mock conductor returning a preset response (or error) for one fetch call,
/// with independently failable `ping` / `whoami` so each `/healthz` branch can
/// be driven.
struct MockConductor {
    ping_ok: bool,
    whoami_ok: bool,
    response: Mutex<Option<anyhow::Result<ReadCloseResponse>>>,
}

impl MockConductor {
    fn with(resp: anyhow::Result<ReadCloseResponse>) -> Arc<Self> {
        Arc::new(Self {
            ping_ok: true,
            whoami_ok: true,
            response: Mutex::new(Some(resp)),
        })
    }

    /// A conductor whose `ping()` fails — `/healthz`: conductor unreachable.
    fn down() -> Arc<Self> {
        Arc::new(Self {
            ping_ok: false,
            whoami_ok: false,
            response: Mutex::new(None),
        })
    }

    /// A reachable conductor whose app cell does not answer — `/healthz`:
    /// cell unresponsive.
    fn cell_wedged() -> Arc<Self> {
        Arc::new(Self {
            ping_ok: true,
            whoami_ok: false,
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
    async fn read_predecessor_close(
        &self,
        _agent: AgentPubKey,
    ) -> anyhow::Result<ReadCloseResponse> {
        self.response
            .lock()
            .unwrap()
            .take()
            .expect("response consumed once")
    }
    async fn whoami(&self) -> anyhow::Result<AgentPubKey> {
        if self.whoami_ok {
            Ok(holo_hash::AgentPubKey::from_raw_36(vec![9; 36]))
        } else {
            anyhow::bail!("cell not responding")
        }
    }
}

fn state(conductor: Arc<dyn Conductor>) -> AppState {
    AppState {
        conductor,
        bearer_token: Arc::new(TOKEN.to_string()),
    }
}

fn fetch_close_req(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/v1/fetch-close")
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

/// A `/v1/fetch-close` request with an arbitrary raw body (for the
/// malformed-input path), bearer-authed so it reaches the body parse.
fn fetch_close_req_raw(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/fetch-close")
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

fn dummy_payload() -> SummaryStatePayload {
    SummaryStatePayload {
        agent_pubkey: holo_hash::AgentPubKey::from_raw_36(vec![3; 36]),
        source_dna_hash: holo_hash::DnaHash::from_raw_36(vec![1; 36]),
        target_dna_hash: holo_hash::DnaHash::from_raw_36(vec![5; 36]),
        closing_state: dummy_summary_state(),
        chain_top: holo_hash::ActionHash::from_raw_36(vec![2; 36]),
    }
}

fn dummy_signatures() -> Vec<NotarySignature> {
    vec![
        NotarySignature {
            notary: holo_hash::AgentPubKey::from_raw_36(vec![4; 36]),
            signature: hdi::prelude::Signature([7u8; 64]),
        },
        NotarySignature {
            notary: holo_hash::AgentPubKey::from_raw_36(vec![5; 36]),
            signature: hdi::prelude::Signature([8u8; 64]),
        },
    ]
}

fn found() -> ReadCloseResponse {
    ReadCloseResponse::Found {
        payload: dummy_payload(),
        notary_signatures: dummy_signatures(),
        close_action: holo_hash::ActionHash::from_raw_36(vec![6; 36]),
    }
}

#[tokio::test]
async fn missing_bearer_is_401() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(c, fetch_close_req(None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "auth_failed");
}

#[tokio::test]
async fn wrong_bearer_is_401() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(c, fetch_close_req(Some("nope"))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "auth_failed");
}

#[tokio::test]
async fn found_is_200_with_three_field_package() {
    let c = MockConductor::with(Ok(found()));
    let (status, body) = send(c, fetch_close_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("payload").is_some());
    assert_eq!(
        body["notary_signatures"].as_array().map(|a| a.len()),
        Some(2)
    );
    assert!(body.get("close_action").is_some());
}

#[tokio::test]
async fn no_close_found_is_404() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(c, fetch_close_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "no_close_found");
}

#[tokio::test]
async fn warranted_is_422() {
    let c = MockConductor::with(Ok(ReadCloseResponse::Warranted(vec![])));
    let (status, body) = send(c, fetch_close_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "warranted");
}

#[tokio::test]
async fn unable_to_verify_is_503() {
    let c = MockConductor::with(Ok(ReadCloseResponse::UnableToVerify));
    let (status, body) = send(c, fetch_close_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "unable_to_verify");
}

#[tokio::test]
async fn zome_error_is_500_internal() {
    let c = MockConductor::with(Err(anyhow::anyhow!("boom")));
    let (status, body) = send(c, fetch_close_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal");
}

/// The retired signing route must be gone: nothing answers `/v1/notarize` (the
/// daemon has no signing capability of any kind), and no code path emits a
/// `too_new` code — the freshness window is gone from the protocol.
#[tokio::test]
async fn notarize_route_is_gone() {
    let c = MockConductor::with(Ok(found()));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/notarize")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(format!(
            r#"{{"agent_pubkey":"{}"}}"#,
            agent_b64()
        )))
        .unwrap();
    let (status, _) = send(c, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Wire round-trip smoke (the one thing the mocked HTTP tests bypass): the
/// envelope the daemon's `Found` branch serializes (payload + notary
/// signatures + close action) must decode back into the app's
/// `MigrationInitRequest` using the same `rave_engine` v0_1 types. Locks the
/// package shape against silent serde drift between the daemon output and the
/// app's `migration_init` input. The live-conductor end-to-end is
/// `tests/live_roundtrip.rs` (gated).
#[test]
fn found_envelope_round_trips_into_migration_init_request() {
    let payload = dummy_payload();
    let notary_signatures = dummy_signatures();
    let close_action = holo_hash::ActionHash::from_raw_36(vec![6; 36]);

    // Exactly what `fetch_close`'s 200 branch emits.
    let envelope = serde_json::json!({
        "payload": payload,
        "notary_signatures": notary_signatures,
        "close_action": close_action,
    });

    // Exactly what the app builds from the router's verbatim forward.
    let req: MigrationInitRequest = serde_json::from_value(envelope)
        .expect("daemon Found envelope must decode into MigrationInitRequest");

    assert_eq!(
        req.payload, payload,
        "payload must survive the wire round-trip"
    );
    assert_eq!(req.notary_signatures, notary_signatures);
    assert_eq!(req.close_action, close_action);
}

// `/healthz` reflects BOTH the conductor and the app cell: either failing makes
// the daemon unhealthy, with distinct messages so ops can tell which.

#[tokio::test]
async fn healthz_is_200_when_conductor_and_cell_up() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(c, healthz_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn healthz_is_503_when_conductor_down() {
    let (status, body) = send(MockConductor::down(), healthz_req()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "internal");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("conductor unreachable"),
        "ping failure must name the conductor: {body}"
    );
}

#[tokio::test]
async fn healthz_is_503_when_app_cell_unresponsive() {
    let (status, body) = send(MockConductor::cell_wedged(), healthz_req()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "internal");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("app cell unresponsive"),
        "whoami failure must name the cell: {body}"
    );
}

// B5 — client-side input errors short-circuit to a distinct `bad_request`
// 4xx (so the router hard-stops instead of fanning across every notary).

#[tokio::test]
async fn unparseable_body_is_400_bad_request() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(c, fetch_close_req_raw("not json{")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn invalid_agent_pubkey_is_400_bad_request() {
    let c = MockConductor::with(Ok(ReadCloseResponse::NoCloseFound));
    let (status, body) = send(
        c,
        fetch_close_req_raw(r#"{"agent_pubkey":"not-a-valid-b64-key"}"#),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}
