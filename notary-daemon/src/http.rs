//! HTTP surface: `/healthz` + `/v1/notarize`, the uniform error envelope, and
//! the bearer-auth gate. Handlers are generic over `Conductor` so tests inject a
//! mock.

use std::str::FromStr;
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

/// Machine-readable error codes — the daemon half of the cross-service contract.
/// These MUST stay in sync with the router's `ErrorCode` union in
/// `router/src/errors.ts`, which switches on these exact strings. Defined once
/// here rather than as scattered literals so a code can't silently drift.
mod codes {
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const WARRANTED: &str = "warranted";
    pub const NO_CLOSE_FOUND: &str = "no_close_found";
    pub const TOO_NEW: &str = "too_new";
    pub const UNABLE_TO_VERIFY: &str = "unable_to_verify";
    pub const INTERNAL: &str = "internal";
    // B5/B6: client-side input errors get a distinct 4xx code so the router
    // hard-stops instead of retrying the same malformed request across notaries.
    pub const BAD_REQUEST: &str = "bad_request";
}

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
            codes::INTERNAL,
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
            codes::AUTH_FAILED,
            "missing or invalid bearer token",
        );
    }

    // B5: client-side input errors get a distinct `bad_request` code (not the
    // 5xx-classed `internal`) so the router hard-stops instead of retrying the
    // same malformed request across every notary. Two such errors:
    let parsed: NotarizeBody = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                codes::BAD_REQUEST,
                format!("invalid request body: {e}"),
            )
        }
    };
    let agent_pubkey: AgentPubKey = match AgentPubKeyB64::from_str(&parsed.agent_pubkey) {
        Ok(b64) => b64.into(),
        Err(e) => {
            return error(
                StatusCode::BAD_REQUEST,
                codes::BAD_REQUEST,
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
            codes::WARRANTED,
            "the agent's chain carries warrants",
            json!({ "warrants": warrants }),
        ),
        Ok(NotaryReadResponse::NoCloseFound) => error(
            StatusCode::NOT_FOUND,
            codes::NO_CLOSE_FOUND,
            "no ClosingStateSummary on the agent's chain (close on the from-DNA first)",
        ),
        Ok(NotaryReadResponse::TooNew {
            earliest_acceptable,
        }) => error_with_details(
            StatusCode::CONFLICT,
            codes::TOO_NEW,
            "close action is younger than the freshness window",
            json!({ "earliest_acceptable": earliest_acceptable }),
        ),
        Ok(NotaryReadResponse::UnableToVerify) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::UNABLE_TO_VERIFY,
            "notary could not read/verify the agent's chain",
        ),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "notary_read_predecessor_close failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
                "internal error",
            )
        }
    }
}
