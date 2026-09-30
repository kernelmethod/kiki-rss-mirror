use crate::routes::v1::entries::rows::{attach_tags, entry_columns, entry_from_row};
use crate::routes::v1::entries::ListEntriesResponseEntry;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
            let ids = serde_json::to_string(&request.ids)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            let mut entries = conn
                .prepare(&format!(
                    "SELECT {} FROM entries e
                     WHERE e.id IN (SELECT value FROM json_each(?1))",
                    entry_columns()
                ))
                .inspect_err(|e| {
                    event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
                })?
                .query_map([ids], entry_from_row)?
                .collect::<Result<Vec<_>, _>>()?;

            let mut position = HashMap::new();
            for (i, id) in request.ids.iter().enumerate() {
                position.entry(*id).or_insert(i);
            }
            entries.sort_by_key(|e| position.get(&e.id).copied());
            attach_tags(conn, &mut entries)?;

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
