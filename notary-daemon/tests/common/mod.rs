use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use holo_hash::AgentPubKey;
use http_body_util::BodyExt;
use rave_engine::types::entries::migration::v0_2::{
    AgreementCarryForward, AttestCloseResponse, NotarySignature, SummaryState, SummaryStatePayload,
    SummaryTx,
};
use rave_engine::types::units::UnitMap;
use tower::ServiceExt;

use migration_notary::conductor::Conductor;
use migration_notary::http::{router, AppState};

pub const TOKEN: &str = "test-token";

/// A checksum-valid `AgentPubKeyB64`: a hand-typed literal fails its decode, so
/// the handler would answer 400 before the conductor is consulted.
pub fn agent_b64() -> String {
    holo_hash::AgentPubKeyB64::from(agent()).to_string()
}

pub fn agent() -> AgentPubKey {
    AgentPubKey::from_raw_32(vec![0u8; 32])
}

pub fn state(conductor: Arc<dyn Conductor>) -> AppState {
    AppState {
        conductor,
        bearer_token: Arc::new(TOKEN.to_string()),
    }
}

pub fn request(uri: &str, token: Option<&str>, body: String) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.body(Body::from(body)).unwrap()
}

pub fn attest_req(token: Option<&str>) -> Request<Body> {
    request(
        "/v2/attest-close",
        token,
        format!(r#"{{"agent_pubkey":"{}"}}"#, agent_b64()),
    )
}

pub fn healthz_req() -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri("/healthz")
        .body(Body::empty())
        .unwrap()
}

pub async fn send_raw(conductor: Arc<dyn Conductor>, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let resp = router(state(conductor)).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec())
}

pub async fn send(
    conductor: Arc<dyn Conductor>,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let (status, bytes) = send_raw(conductor, req).await;
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

pub fn payload() -> SummaryStatePayload {
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

pub fn signature() -> NotarySignature {
    NotarySignature {
        notary: AgentPubKey::from_raw_36(vec![4; 36]),
        signature: hdi::prelude::Signature([7u8; 64]),
    }
}

pub fn attested() -> AttestCloseResponse {
    AttestCloseResponse::Attested {
        payload: payload(),
        close_action: holo_hash::ActionHash::from_raw_36(vec![6; 36]),
        notary_signature: signature(),
    }
}
