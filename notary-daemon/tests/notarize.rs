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
    NotaryReadRequest, NotaryReadResponse, SummaryState, SummaryStatePayload, SummaryTx,
};

const TOKEN: &str = "test-token";
const AGENT_B64: &str = "uhCAkcMA4vDg7vY0i1Yq8Cf0o3a2Z2qFq0p2u7iI0sJv5Q4q7lq3"; // any valid AgentPubKeyB64

/// Mock conductor returning a preset response (or error) for one call.
struct MockConductor {
    ping_ok: bool,
    response: Mutex<Option<anyhow::Result<NotaryReadResponse>>>,
}

impl MockConductor {
    fn with(resp: anyhow::Result<NotaryReadResponse>) -> Arc<Self> {
        Arc::new(Self { ping_ok: true, response: Mutex::new(Some(resp)) })
    }
}

#[async_trait]
impl Conductor for MockConductor {
    async fn ping(&self) -> anyhow::Result<()> {
        if self.ping_ok { Ok(()) } else { anyhow::bail!("down") }
    }
    async fn notary_read_predecessor_close(
        &self,
        _req: NotaryReadRequest,
    ) -> anyhow::Result<NotaryReadResponse> {
        self.response.lock().unwrap().take().expect("response consumed once")
    }
}

fn state(conductor: Arc<dyn Conductor>) -> AppState {
    AppState { conductor, bearer_token: Arc::new(TOKEN.to_string()) }
}

fn notarize_req(token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri("/v1/notarize").header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(format!(r#"{{"agent_pubkey":"{AGENT_B64}"}}"#))).unwrap()
}

async fn send(conductor: Arc<dyn Conductor>, req: Request<Body>) -> (StatusCode, serde_json::Value) {
    let resp = router(state(conductor)).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
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
    let c = MockConductor::with(Ok(NotaryReadResponse::TooNew { earliest_acceptable: earliest }));
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
