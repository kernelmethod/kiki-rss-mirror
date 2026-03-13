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
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ListScriptsResponse {
    pub scripts: Vec<ScriptResponse>,
    pub count: usize,
}

/// Route handler for listing all Lua scripts.
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
            .prepare("SELECT id, engine, text FROM scripts ORDER BY id")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_map([], |row| {
                Ok(ScriptResponse {
                    id: row.get(0)?,
                    engine: row.get(1)?,
                    text: row.get(2)?,
                })
            })?
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
