//! HTTP surface: `/healthz` + `/v1/fetch-close`, the uniform error envelope, and
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
use rave_engine::types::entries::migration::v0_1::ReadCloseResponse;
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
        .route("/v1/fetch-close", post(fetch_close))
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

/// Healthy means BOTH the conductor answers and the app cell answers — a
/// conductor can be reachable while its cell is wedged, and the router must
/// not route fetches at either state. The two failures carry distinct
/// messages so ops can tell them apart from the probe alone.
async fn healthz(State(state): State<AppState>) -> Response {
    if let Err(e) = state.conductor.ping().await {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::INTERNAL,
            format!("conductor unreachable: {e}"),
        );
    }
    if let Err(e) = state.conductor.whoami().await {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::INTERNAL,
            format!("app cell unresponsive: {e}"),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "api_versions": API_VERSIONS,
            "protocol_versions": PROTOCOL_VERSIONS,
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct FetchCloseBody {
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

/// Serve the agent's committed closing summary — the package the
/// migration-service hands back to that agent to apply as install-time
/// `init_properties` on the successor DNA. A pure read of what the agent
/// committed (the signatures
/// inside it already carry the trust); nothing is recomputed or signed here.
async fn fetch_close(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
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
    let parsed: FetchCloseBody = match serde_json::from_str(&body) {
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

    match state.conductor.read_predecessor_close(agent_pubkey).await {
        Ok(ReadCloseResponse::Found {
            payload,
            notary_signatures,
            close_action,
        }) => (
            StatusCode::OK,
            Json(json!({
                "payload": payload,
                "notary_signatures": notary_signatures,
                "close_action": close_action,
            })),
        )
            .into_response(),
        Ok(ReadCloseResponse::Warranted(warrants)) => error_with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            codes::WARRANTED,
            "the agent's chain carries warrants",
            json!({ "warrants": warrants }),
        ),
        Ok(ReadCloseResponse::NoCloseFound) => error(
            StatusCode::NOT_FOUND,
            codes::NO_CLOSE_FOUND,
            "no ClosingStateSummary on the agent's chain (close on the from-DNA first)",
        ),
        Ok(ReadCloseResponse::UnableToVerify) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::UNABLE_TO_VERIFY,
            "notary could not (yet) read the agent's committed close",
        ),
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "read_predecessor_close failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
                "internal error",
            )
        }
    }
}
