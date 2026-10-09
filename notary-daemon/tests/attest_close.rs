//! HTTP↔zome mapping tests for `/v2/attest-close` + `/healthz`, driving the real
//! `router()` with a mock `Conductor` (no Holochain conductor needed).

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::http::StatusCode;
use holo_hash::AgentPubKey;
use rave_engine::types::entries::migration::v0_2::{AttestCloseResponse, MigrationInitRequest};

use common::{
    agent, agent_b64, attest_req, attested, healthz_req, payload, request, send, send_raw,
    signature, TOKEN,
};
use migration_notary::conductor::Conductor;

/// A conductor answering one attestation call as scripted, recording whom it
/// was asked about, with independently failable `ping` / `whoami`.
struct MockConductor {
    ping_ok: bool,
    whoami_ok: bool,
    response: Mutex<Option<anyhow::Result<AttestCloseResponse>>>,
    asked_about: Mutex<Vec<AgentPubKey>>,
}

impl MockConductor {
    fn new(
        ping_ok: bool,
        whoami_ok: bool,
        response: Option<anyhow::Result<AttestCloseResponse>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ping_ok,
            whoami_ok,
            response: Mutex::new(response),
            asked_about: Mutex::new(vec![]),
        })
    }

    fn with(resp: anyhow::Result<AttestCloseResponse>) -> Arc<Self> {
        Self::new(true, true, Some(resp))
    }

    fn down() -> Arc<Self> {
        Self::new(false, false, None)
    }

    fn cell_wedged() -> Arc<Self> {
        Self::new(true, false, None)
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
