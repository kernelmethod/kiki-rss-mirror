//! Reporting what requests without an API token may do; see
//! [`crate::config::AnonymousAccess`].

use crate::auth::Scopes;
use crate::config::AnonymousAccess;
use crate::server::AppState;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct AccessResponse {
    /// What a request without a token may do: `full` (anything),
    /// `read-only`, or `token-required` (nothing beyond the routes that
    /// need no scope, such as this one).
    pub anonymous_access: AnonymousAccess,
    /// The scopes a request without a token holds.
    pub anonymous_scopes: Scopes,
}

/// Show anonymous access
///
/// What a request that presents no API token may do, as the
/// `api.anonymous_access` setting decides. Anyone may ask, with or
/// without a token, whatever the setting.
#[utoipa::path(
    get,
    path = "/v1/access",
    responses(
        (status = 200, description = "What requests without a token may do", body = AccessResponse),
    ),
    tag = "meta"
)]
#[axum::debug_handler]
pub async fn access(State(state): State<AppState>) -> Json<AccessResponse> {
    let anonymous_access = state.config.current().api.anonymous_access;
    Json(AccessResponse {
        anonymous_access,
        anonymous_scopes: anonymous_access.scopes(),
    })
}
