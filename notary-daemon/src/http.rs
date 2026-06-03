//! HTTP surface: `/healthz` + `/v1/notarize`, the uniform error envelope, and
//! the bearer-auth gate. Handlers are generic over `Conductor` so tests inject a
//! mock.

use std::sync::Arc;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use holo_hash::{AgentPubKey, AgentPubKeyB64};
use rave_engine::types::entries::migration::v0_1::{NotaryReadRequest, NotaryReadResponse};
use serde::Deserialize;
use serde_json::json;

use crate::conductor::Conductor;

pub const API_VERSIONS: &[&str] = &["v1"];
pub const PROTOCOL_VERSIONS: &[&str] = &["v0_1"];

#[derive(Clone)]
pub struct AppState {
    pub conductor: Arc<dyn Conductor>,
    pub bearer_token: Arc<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/notarize", post(notarize))
        .with_state(state)
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into() } })),
    )
        .into_response()
}

fn error_with_details(
    status: StatusCode,
    code: &str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> Response {
    (
        status,
        Json(json!({ "error": { "code": code, "message": message.into(), "details": details } })),
    )
        .into_response()
}

async fn healthz(State(state): State<AppState>) -> Response {
    match state.conductor.ping().await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({
                "status": "ok",
                "api_versions": API_VERSIONS,
                "protocol_versions": PROTOCOL_VERSIONS,
            })),
        )
            .into_response(),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "internal",
            format!("conductor unreachable: {e}"),
        ),
    }
}

#[derive(Deserialize)]
struct NotarizeBody {
    // Parsed as a plain string then via AgentPubKeyB64's FromStr: holo_hash's
    // serde Deserialize for the B64 newtype does NOT round-trip its own string
    // form (it reads the chars as raw bytes → BadSize), whereas FromStr decodes
    // the base64 correctly. The router sends the standard "uhCAk…" b64 string.
    agent_pubkey: String,
}

fn check_bearer(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {expected}"))
        .unwrap_or(false)
}

async fn notarize(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !check_bearer(&headers, &state.bearer_token) {
        return error(
            StatusCode::UNAUTHORIZED,
            "auth_failed",
            "missing or invalid bearer token",
        );
    }

    let parsed: NotarizeBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "internal",
                format!("invalid request body: {e}"),
            )
        }
    };
    let agent_pubkey: AgentPubKey = match parsed.agent_pubkey.parse::<AgentPubKeyB64>() {
        Ok(k) => k.into(),
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                "internal",
                format!("invalid agent_pubkey: {e}"),
            )
        }
    };

    let result = state
        .conductor
        .notary_read_predecessor_close(NotaryReadRequest { agent_pubkey })
        .await;

    match result {
        Ok(NotaryReadResponse::Verified { payload, signature }) => (
            StatusCode::OK,
            Json(json!({ "payload": payload, "signature": signature })),
        )
            .into_response(),
        Ok(NotaryReadResponse::Warranted(warrants)) => error_with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "warranted",
            "the agent's chain carries warrants",
            json!({ "warrants": warrants }),
        ),
        Ok(NotaryReadResponse::NoCloseFound) => error(
            StatusCode::NOT_FOUND,
            "no_close_found",
            "no ClosingStateSummary on the agent's chain (close on the from-DNA first)",
        ),
        Ok(NotaryReadResponse::TooNew {
            earliest_acceptable,
        }) => error_with_details(
            StatusCode::CONFLICT,
            "too_new",
            "close action is younger than the freshness window",
            json!({ "earliest_acceptable": earliest_acceptable }),
        ),
        Ok(NotaryReadResponse::UnableToVerify) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unable_to_verify",
            "notary could not read/verify the agent's chain",
        ),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "notary_read_predecessor_close failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "internal error",
            )
        }
    }
}
