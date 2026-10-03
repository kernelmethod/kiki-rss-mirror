use crate::routes::v1::entries::rows::load_entries;
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tracing::{event, Level};

/// The most entries one batch request may ask for.
pub const MAX_BATCH_IDS: usize = 1000;

/// Request body for the batch entry endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct BatchEntriesRequest {
    /// IDs of the entries to get, at most 1000.
    pub ids: Vec<i64>,
}

/// Response from the batch entry endpoint.
#[derive(Debug, Deserialize, Serialize, utoipa::ToSchema)]
pub struct BatchEntriesResponse {
    /// The entries that exist, in the order their IDs were given, each once.
    pub entries: Vec<ListEntriesResponseEntry>,
}

/// Get many entries
///
/// Retrieve the entries with the given IDs in one request, in the order the IDs were given.
/// IDs of entries that do not exist are left out of the response, and an ID given more than
/// once is returned once. Entries tagged `system:hidden` are returned like any other, since
/// they were asked for by ID. At most 1000 IDs may be given.
#[utoipa::path(
    post,
    path = "/v1/entries/batch",
    request_body = BatchEntriesRequest,
    responses(
        (status = 200, description = "The entries that exist", body = BatchEntriesResponse),
        (status = 400, description = "Too many IDs"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "entries"
)]
pub async fn batch_entries(
    State(state): State<AppState>,
    Json(request): Json<BatchEntriesRequest>,
) -> Result<Response, Response> {
    if request.ids.len() > MAX_BATCH_IDS {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("at most {MAX_BATCH_IDS} entries may be requested at once"),
        )
            .into_response());
    }

    let result = state
        .db
        .read(move |conn| {
            let entries = load_entries(conn, &request.ids)?;

            Ok::<BatchEntriesResponse, rusqlite::Error>(BatchEntriesResponse { entries })
        })
        .await
        .inspect_err(|e| {
            event!(Level::ERROR, "task error in batch_entries: {:?}", e);
        });

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in batch_entries: {:?}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()),
    }
}
