//! HTTP surface: `/healthz` + `/v2/attest-close`, the uniform error envelope, and
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
use rave_engine::types::entries::migration::v0_2::{AttestCloseResponse, MigrationInitRequest};
use serde::Deserialize;
use serde_json::json;

use crate::conductor::Conductor;

pub const API_VERSIONS: &[&str] = &["v2"];
pub const PROTOCOL_VERSIONS: &[&str] = &["v0_2"];

/// The router reads these through `DAEMON_CODES` in
/// `migration-router/src/notary.ts` and treats any code missing there as
/// `internal`, so a code added here goes there too.
mod codes {
    pub const AUTH_FAILED: &str = "auth_failed";
    pub const WARRANTED: &str = "warranted";
    pub const NO_CLOSE_FOUND: &str = "no_close_found";
    pub const UNABLE_TO_VERIFY: &str = "unable_to_verify";
    pub const INTERNAL: &str = "internal";
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
        .route("/v2/attest-close", post(attest_close))
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

/// Healthy means BOTH the conductor and the app cell answer: a conductor can be
/// reachable while its cell is wedged. The two failures carry distinct messages
/// so ops can tell them apart. The endpoint is unauthenticated, so a failure's
/// cause goes to the log rather than the body.
async fn healthz(State(state): State<AppState>) -> Response {
    if let Err(e) = state.conductor.ping().await {
        tracing::warn!(error = %format!("{e:#}"), "healthz: conductor unreachable");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::INTERNAL,
            "conductor unreachable",
        );
    }
    if let Err(e) = state.conductor.whoami().await {
        tracing::warn!(error = %format!("{e:#}"), "healthz: app cell unresponsive");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::INTERNAL,
            "app cell unresponsive",
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
struct AttestCloseBody {
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

/// This notary's attestation of the agent's closed chain, as a package carrying
/// exactly its own signature. The router combines M of them.
async fn attest_close(State(state): State<AppState>, headers: HeaderMap, body: String) -> Response {
    if !check_bearer(&headers, &state.bearer_token) {
        tracing::warn!("attest-close refused: missing or invalid bearer token");
        return error(
            StatusCode::UNAUTHORIZED,
            codes::AUTH_FAILED,
            "missing or invalid bearer token",
        );
    }

    let parsed: AttestCloseBody = match serde_json::from_str(&body) {
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

    let agent = parsed.agent_pubkey;
    match state.conductor.notary_attest_close(agent_pubkey).await {
        Ok(AttestCloseResponse::Attested {
            payload,
            close_action,
            notary_signature,
        }) => (
            StatusCode::OK,
            Json(MigrationInitRequest {
                payload,
                notary_signatures: vec![notary_signature],
                close_action,
            }),
        )
            .into_response(),
        Ok(AttestCloseResponse::Warranted(warrants)) => error_with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            codes::WARRANTED,
            "the agent's chain carries warrants",
            json!({ "warrants": warrants }),
        ),
        Ok(AttestCloseResponse::NoCloseFound) => error(
            StatusCode::NOT_FOUND,
            codes::NO_CLOSE_FOUND,
            "the agent's chain holds no attestable close",
        ),
        Ok(AttestCloseResponse::UnableToVerify) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            codes::UNABLE_TO_VERIFY,
            "this notary cannot see the agent's closed chain yet",
        ),
        Ok(AttestCloseResponse::NotAClosingNotary) => {
            tracing::error!(%agent, "this node is not a closing notary on its DNA");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
                "this node is not a closing notary on its DNA",
            )
        }
        Err(e) => {
            tracing::error!(%agent, error = %format!("{e:#}"), "notary_attest_close failed");
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                codes::INTERNAL,
                "internal error",
            )
        }
    }
}
