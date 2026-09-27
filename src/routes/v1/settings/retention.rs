use super::update_config;
use crate::server::AppState;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

const SECTION: &str = "retention";

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct RetentionResponse {
    pub max_age_days: Option<i64>,
}

#[derive(Serialize, Deserialize, utoipa::ToSchema)]
pub struct RetentionRequest {
    /// Delete entries published more than this many days ago. Must be at
    /// least 1. `null` disables retention, keeping entries forever.
    pub max_age_days: Option<i64>,
}

/// Get retention policy
///
/// Retrieve settings for the current retention policy for RSS/Atom entries.
#[utoipa::path(
    get,
    path = "/v1/settings/retention",
    responses(
        (status = 200, description = "Current retention settings", body = RetentionResponse),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn get_retention(State(state): State<AppState>) -> Response {
    let max_age_days = state.config.current().retention.max_age_days;
    Json(RetentionResponse { max_age_days }).into_response()
}

/// Update retention policy
///
/// Update the settings for the retention policy for RSS/Atom entries.
#[utoipa::path(
    put,
    path = "/v1/settings/retention",
    request_body = RetentionRequest,
    responses(
        (status = 200, description = "Updated retention settings", body = RetentionResponse),
        (status = 400, description = "Invalid value"),
        (status = 409, description = "The config file on disk is invalid"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "settings"
)]
#[axum::debug_handler]
pub async fn put_retention(
    State(state): State<AppState>,
    Json(payload): Json<RetentionRequest>,
) -> Result<Response, Response> {
    let settings = update_config(&state, move |o| {
        match payload.max_age_days {
            Some(days) => o.set(SECTION, "max_age_days", days)?,
            None => o.unset(SECTION, "max_age_days"),
        }
        Ok(())
    })
    .await?;

    let max_age_days = settings.retention.max_age_days;
    Ok(Json(RetentionResponse { max_age_days }).into_response())
}
