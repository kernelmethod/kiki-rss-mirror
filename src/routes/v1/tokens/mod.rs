//! Routes for managing API tokens; see [`crate::auth`].

use crate::auth::{Principal, Scopes};
use crate::db::tokens::{self, Token, TokenError};
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tracing::{event, Level};

pub fn create_router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tokens).post(create_token))
        .route("/current", get(current_token))
        .route("/id/{id}", delete(revoke_token))
}

fn internal_error(context: &str, e: impl std::fmt::Display) -> Response {
    event!(Level::ERROR, "error in {context}: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct ListTokensResponse {
    pub tokens: Vec<Token>,
}

/// List API tokens
///
/// Every API token, oldest first. Tokens' secrets are never stored, so
/// are not listed.
#[utoipa::path(
    get,
    path = "/v1/tokens",
    responses(
        (status = 200, description = "The tokens", body = ListTokensResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tokens"
)]
#[axum::debug_handler]
pub async fn list_tokens(State(state): State<AppState>) -> Response {
    match state.db.read(|conn| tokens::list(conn)).await {
        Ok(Ok(tokens)) => Json(ListTokensResponse { tokens }).into_response(),
        Ok(Err(e)) => internal_error("list_tokens", e),
        Err(e) => internal_error("list_tokens", e),
    }
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct CreateTokenRequest {
    /// A name to tell the token apart by, unique among tokens.
    pub name: String,
    /// What the token may do: scope names, or the presets `reader`,
    /// `curator` and `manager`.
    pub scopes: Scopes,
    /// Unix timestamp after which the token is refused. The token never
    /// expires if this is left out.
    #[serde(default)]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct CreateTokenResponse {
    /// The token's details.
    #[serde(flatten)]
    pub details: Token,
    /// The token itself, to send as `Authorization: Bearer <token>`. It is
    /// shown only this once.
    pub token: String,
}

/// Create an API token
///
/// Create a token granting the given scopes. The response holds the token
/// itself, which cannot be retrieved again.
#[utoipa::path(
    post,
    path = "/v1/tokens",
    request_body = CreateTokenRequest,
    responses(
        (status = 201, description = "Token created", body = CreateTokenResponse),
        (status = 400, description = "Invalid name, scopes or expiry"),
        (status = 409, description = "A token with this name already exists"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tokens"
)]
#[axum::debug_handler]
pub async fn create_token(
    State(state): State<AppState>,
    Json(request): Json<CreateTokenRequest>,
) -> Response {
    let now = chrono::Utc::now().timestamp();
    if request.expires_at.is_some_and(|t| t <= now) {
        return (StatusCode::BAD_REQUEST, "expires_at is in the past").into_response();
    }
    let result = state
        .db
        .write(move |conn| tokens::create(conn, &request.name, request.scopes, request.expires_at))
        .await;
    match result {
        Ok(Ok((details, token))) => {
            event!(
                Level::INFO,
                id = details.id,
                name = %details.name,
                scopes = %details.scopes,
                "API token created"
            );
            (
                StatusCode::CREATED,
                Json(CreateTokenResponse { details, token }),
            )
                .into_response()
        }
        Ok(Err(e @ TokenError::NameTaken(_))) => {
            (StatusCode::CONFLICT, e.to_string()).into_response()
        }
        Ok(Err(e @ (TokenError::InvalidName | TokenError::NoScopes))) => {
            (StatusCode::BAD_REQUEST, e.to_string()).into_response()
        }
        Ok(Err(e)) => internal_error("create_token", e),
        Err(e) => internal_error("create_token", e),
    }
}

/// Revoke an API token
///
/// Delete a token, which is refused from then on.
#[utoipa::path(
    delete,
    path = "/v1/tokens/id/{id}",
    params(("id" = i64, Path, description = "Token ID")),
    responses(
        (status = 204, description = "Token revoked"),
        (status = 404, description = "No such token"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "tokens"
)]
#[axum::debug_handler]
pub async fn revoke_token(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state.db.write(move |conn| tokens::revoke(conn, id)).await {
        Ok(Ok(true)) => {
            event!(Level::INFO, id, "API token revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Ok(false)) => (StatusCode::NOT_FOUND, "Token not found").into_response(),
        Ok(Err(e)) => internal_error("revoke_token", e),
        Err(e) => internal_error("revoke_token", e),
    }
}

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct CurrentTokenResponse {
    /// The token the request was made with, or null for a request over the
    /// Unix socket without one.
    pub token: Option<Token>,
    /// What the request may do.
    pub scopes: Scopes,
}

/// Show the current token
///
/// The token the request was made with, and what it may do. Any valid
/// token may ask, so a client can check a token before using it.
#[utoipa::path(
    get,
    path = "/v1/tokens/current",
    responses(
        (status = 200, description = "The current token", body = CurrentTokenResponse),
        (status = 401, description = "No valid token was given"),
    ),
    tag = "tokens"
)]
#[axum::debug_handler]
pub async fn current_token(principal: Principal) -> Response {
    let scopes = principal.scopes();
    let token = match principal {
        Principal::Token(token) => Some(token),
        _ => None,
    };
    Json(CurrentTokenResponse { token, scopes }).into_response()
}

#[cfg(test)]
mod tests;
