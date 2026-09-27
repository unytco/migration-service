//! HTTP↔zome mapping tests for `/v2/attest-close` + `/healthz`, driving the real
//! `router()` with a mock `Conductor` (no Holochain conductor needed).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use migration_notary::conductor::Conductor;
use migration_notary::http::{router, AppState};

use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_2::{
    AgreementCarryForward, AttestCloseResponse, MigrationInitRequest, NotarySignature,
    SummaryState, SummaryStatePayload, SummaryTx,
};
use rave_engine::types::units::UnitMap;

const TOKEN: &str = "test-token";

/// A checksum-valid `AgentPubKeyB64`: a hand-typed literal fails its decode, so
/// the handler would answer 400 before the conductor is consulted.
fn agent_b64() -> String {
    holo_hash::AgentPubKeyB64::from(agent()).to_string()
}

fn agent() -> AgentPubKey {
    AgentPubKey::from_raw_32(vec![0u8; 32])
}

/// A conductor answering one attestation call as scripted, recording whom it
/// was asked about, with independently failable `ping` / `whoami`.
struct MockConductor {
    ping_ok: bool,
    whoami_ok: bool,
    response: Mutex<Option<anyhow::Result<AttestCloseResponse>>>,
    asked_about: Mutex<Vec<AgentPubKey>>,
}

impl MockConductor {
    fn with(resp: anyhow::Result<AttestCloseResponse>) -> Arc<Self> {
        Arc::new(Self {
            ping_ok: true,
            whoami_ok: true,
            response: Mutex::new(Some(resp)),
            asked_about: Mutex::new(vec![]),
        })
    }

    fn down() -> Arc<Self> {
        Arc::new(Self {
            ping_ok: false,
            whoami_ok: false,
            response: Mutex::new(None),
            asked_about: Mutex::new(vec![]),
        })
    }

    fn cell_wedged() -> Arc<Self> {
        Arc::new(Self {
            ping_ok: true,
            whoami_ok: false,
            response: Mutex::new(None),
            asked_about: Mutex::new(vec![]),
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
    async fn notary_attest_close(&self, agent: AgentPubKey) -> anyhow::Result<AttestCloseResponse> {
        self.asked_about.lock().unwrap().push(agent);
        self.response
            .lock()
            .unwrap()
            .take()
            .expect("one attestation call per request")
    }
    async fn whoami(&self) -> anyhow::Result<AgentPubKey> {
        if self.whoami_ok {
            Ok(AgentPubKey::from_raw_36(vec![9; 36]))
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

fn request(uri: &str, token: Option<&str>, body: String) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body)).unwrap()
}

fn attest_req(token: Option<&str>) -> Request<Body> {
    request(
        "/v2/attest-close",
        token,
        format!(r#"{{"agent_pubkey":"{}"}}"#, agent_b64()),
    )
}

fn healthz_req() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap()
}

async fn send_raw(conductor: Arc<dyn Conductor>, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = router(state(conductor)).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

async fn send(
    conductor: Arc<dyn Conductor>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send_raw(conductor, req).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn payload() -> SummaryStatePayload {
    SummaryStatePayload {
        agent_pubkey: agent(),
        source_dna_hash: holo_hash::DnaHash::from_raw_36(vec![1; 36]),
        target_dna_hash: holo_hash::DnaHash::from_raw_36(vec![5; 36]),
        closing_state: SummaryState {
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
            agreement_carry_forward: vec![AgreementCarryForward {
                smart_agreement_hash: holo_hash::ActionHash::from_raw_36(vec![10; 36]),
                last_execution_action_hash: holo_hash::ActionHash::from_raw_36(vec![11; 36]),
                carryover: serde_json::json!({ "10": 1, "9": 2 }),
                locked: None,
                credit_limit: Some(UnitMap::from(vec![(0, "500")])),
            }],
        },
        chain_top: holo_hash::ActionHash::from_raw_36(vec![2; 36]),
    }
}

fn signature() -> NotarySignature {
    NotarySignature {
        notary: AgentPubKey::from_raw_36(vec![4; 36]),
        signature: hdi::prelude::Signature([7u8; 64]),
    }
}

fn attested() -> AttestCloseResponse {
    AttestCloseResponse::Attested {
        payload: payload(),
        close_action: holo_hash::ActionHash::from_raw_36(vec![6; 36]),
        notary_signature: signature(),
    }
}

#[tokio::test]
async fn missing_or_wrong_bearer_is_401() {
    for token in [None, Some("nope")] {
        let c = MockConductor::with(Ok(attested()));
        let (status, body) = send(c.clone(), attest_req(token)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "auth_failed");
        assert!(c.asked_about.lock().unwrap().is_empty());
    }
}

/// The 200 body is `MigrationInitRequest`'s serde_json encoding byte for byte,
/// carrying exactly this notary's signature.
#[tokio::test]
async fn attested_is_200_with_the_package_carrying_this_notarys_signature() {
    let c = MockConductor::with(Ok(attested()));
    let (status, bytes) = send_raw(c.clone(), attest_req(Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);

    let expected = MigrationInitRequest {
        payload: payload(),
        notary_signatures: vec![signature()],
        close_action: holo_hash::ActionHash::from_raw_36(vec![6; 36]),
    };
    assert_eq!(bytes, serde_json::to_vec(&expected).unwrap());

    let decoded: MigrationInitRequest = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.payload, payload());
    assert_eq!(decoded.notary_signatures, vec![signature()]);
    assert_eq!(*c.asked_about.lock().unwrap(), vec![agent()]);
}

#[tokio::test]
async fn each_verdict_maps_to_its_status_and_code() {
    let cases: Vec<(anyhow::Result<AttestCloseResponse>, StatusCode, &str)> = vec![
        (
            Ok(AttestCloseResponse::Warranted(vec![])),
            StatusCode::UNPROCESSABLE_ENTITY,
            "warranted",
        ),
        (
            Ok(AttestCloseResponse::NoCloseFound),
            StatusCode::NOT_FOUND,
            "no_close_found",
        ),
        (
            Ok(AttestCloseResponse::UnableToVerify),
            StatusCode::SERVICE_UNAVAILABLE,
            "unable_to_verify",
        ),
        (
            Ok(AttestCloseResponse::NotAClosingNotary),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
        (
            Err(anyhow::anyhow!("boom")),
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
        ),
    ];
    for (response, status, code) in cases {
        let (got_status, body) = send(MockConductor::with(response), attest_req(Some(TOKEN))).await;
        assert_eq!(
            (got_status, body["error"]["code"].as_str()),
            (status, Some(code))
        );
    }
}

#[tokio::test]
async fn warranted_carries_the_warrants() {
    let c = MockConductor::with(Ok(AttestCloseResponse::Warranted(vec![])));
    let (_, body) = send(c, attest_req(Some(TOKEN))).await;
    assert_eq!(
        body["error"]["details"],
        serde_json::json!({ "warrants": [] })
    );
}

#[tokio::test]
async fn not_a_closing_notary_says_so() {
    let c = MockConductor::with(Ok(AttestCloseResponse::NotAClosingNotary));
    let (_, body) = send(c, attest_req(Some(TOKEN))).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not a closing notary on its DNA"),
        "{body}"
    );
}

#[tokio::test]
async fn an_unparseable_body_or_key_is_400_bad_request() {
    for raw in ["not json{", r#"{"agent_pubkey":"not-a-valid-b64-key"}"#] {
        let c = MockConductor::with(Ok(attested()));
        let (status, body) = send(
            c.clone(),
            request("/v2/attest-close", Some(TOKEN), raw.to_string()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{raw}");
        assert_eq!(body["error"]["code"], "bad_request");
        assert!(c.asked_about.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn the_fetch_close_route_is_gone() {
    let c = MockConductor::with(Ok(attested()));
    let (status, _) = send(
        c,
        request(
            "/v1/fetch-close",
            Some(TOKEN),
            format!(r#"{{"agent_pubkey":"{}"}}"#, agent_b64()),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn healthz_is_200_with_v2_and_v0_2_when_conductor_and_cell_answer() {
    let (status, body) = send(MockConductor::with(Ok(attested())), healthz_req()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert_eq!(body["api_versions"], serde_json::json!(["v2"]));
    assert_eq!(body["protocol_versions"], serde_json::json!(["v0_2"]));
}

#[tokio::test]
async fn healthz_is_503_naming_whichever_of_conductor_and_cell_is_down() {
    for (conductor, expected) in [
        (MockConductor::down(), "conductor unreachable"),
        (MockConductor::cell_wedged(), "app cell unresponsive"),
    ] {
        let (status, body) = send(conductor, healthz_req()).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "internal");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{body}"
        );
    }
}
