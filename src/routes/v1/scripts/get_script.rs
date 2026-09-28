use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

use super::list_scripts::{ScriptResponse, SCRIPT_COLUMNS};

/// Get script information
///
/// Retrieve the information and contents of a script by its ID.
#[utoipa::path(
    get,
    path = "/v1/scripts/id/{id}",
    params(
        ("id" = i64, Path, description = "Script ID"),
    ),
    responses(
        (status = 200, description = "Script found", body = ScriptResponse),
        (status = 404, description = "Script not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn get_script(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let conn = match state.conn_pool.get() {
        Ok(conn) => conn,
        Err(e) => {
            event!(Level::ERROR, "failed to get database connection: {:?}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response();
        }
    };

    let task_result = task::spawn_blocking(move || {
        let query_result = conn
            .prepare(&format!(
                "SELECT {SCRIPT_COLUMNS} FROM scripts WHERE id = ?1 LIMIT 1"
            ))
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], ScriptResponse::from_row);
        match query_result {
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            other => other.map(Some),
        }
    })
    .await;

    match task_result {
        Ok(Ok(Some(script))) => (StatusCode::OK, Json(script)).into_response(),
        Ok(Ok(None)) => (StatusCode::NOT_FOUND, "Script not found").into_response(),
        Ok(Err(e)) => {
            event!(Level::ERROR, "error in get_script: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
        Err(e) => {
            event!(Level::ERROR, "task error in get_script: {:?}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::routes::v1::scripts::add_script::{AddScriptRequest, AddScriptResponse};
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_get_script() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let text = "kiki.on(\"entry.ingest\", function(entry) return entry end)";

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: text.to_string(),
                kind: "user".to_string(),
                config: Default::default(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = resp.json::<AddScriptResponse>().await?.id;

        let resp = client
            .get(format!("http://localhost/v1/scripts/id/{id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.json::<ScriptResponse>().await?;
        assert_eq!(body.id, id);
        assert_eq!(body.engine, "lua");
        assert_eq!(body.text, text);
        assert_eq!(body.kind, "user");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_script_not_found() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .get("http://localhost/v1/scripts/id/999")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text().await?, "Script not found");

        Ok(())
    }
}
