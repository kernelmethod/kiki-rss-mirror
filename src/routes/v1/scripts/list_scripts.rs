use crate::scripting::parse_script_config;
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ScriptResponse {
    pub id: i64,
    pub engine: String,
    pub text: String,
    pub kind: String,
    /// The script's config, handed to its top-level chunk as its argument.
    #[schema(value_type = Object)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

/// The columns [`ScriptResponse::from_row`] expects, in order.
pub(super) const SCRIPT_COLUMNS: &str = "id, engine, text, kind, config";

impl ScriptResponse {
    /// Builds a response from a `scripts` row selected with [`SCRIPT_COLUMNS`].
    ///
    /// # Errors
    ///
    /// Returns an error if a column cannot be read, including if the stored config is not
    /// a JSON object.
    pub(super) fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let config: String = row.get(4)?;
        let config = parse_script_config(&config).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(ScriptResponse {
            id: row.get(0)?,
            engine: row.get(1)?,
            text: row.get(2)?,
            kind: row.get(3)?,
            config,
        })
    }
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ListScriptsResponse {
    pub scripts: Vec<ScriptResponse>,
    pub count: usize,
}

/// List scripts
///
/// Return a list of all of the scripts that have been installed to the server.
#[utoipa::path(
    get,
    path = "/v1/scripts",
    responses(
        (status = 200, description = "List of scripts", body = ListScriptsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn list_scripts(State(state): State<AppState>) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let scripts = conn
            .prepare(&format!("SELECT {SCRIPT_COLUMNS} FROM scripts ORDER BY id"))
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([], ScriptResponse::from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let count = scripts.len();
        Ok::<ListScriptsResponse, rusqlite::Error>(ListScriptsResponse { scripts, count })
    })
    .await;

    match result {
        Ok(Ok(response)) => Ok(Json(response).into_response()),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in list_scripts: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error in list_scripts: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}
