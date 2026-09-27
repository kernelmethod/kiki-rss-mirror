use crate::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use tokio::task;
use tracing::{event, Level};

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct AddScriptRequest {
    pub engine: String,
    pub text: String,
    #[serde(default = "default_kind")]
    pub kind: String,
}

fn default_kind() -> String {
    "user".to_string()
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct AddScriptResponse {
    pub id: i64,
}

/// Add a script
#[utoipa::path(
    post,
    path = "/v1/scripts/create",
    request_body = AddScriptRequest,
    responses(
        (status = 201, description = "Script created successfully", body = AddScriptResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn add_script(
    State(state): State<AppState>,
    Json(payload): Json<AddScriptRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let result = task::spawn_blocking(move || {
        conn.prepare("INSERT INTO scripts (engine, text, kind) VALUES (?1, ?2, ?3) RETURNING id")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([payload.engine, payload.text, payload.kind], |row| {
                row.get(0)
            })
    })
    .await;

    match result {
        Ok(Ok(id)) => {
            event!(Level::INFO, "created new script with id={}", id);

            if let Err(e) = state.reload_tx.send(()) {
                event!(Level::ERROR, "failed to send ReloadScripts signal: {:?}", e);
            }

            Ok((StatusCode::CREATED, Json(AddScriptResponse { id })))
        }
        Ok(Err(e)) => {
            event!(
                Level::ERROR,
                "failed to insert script into database: {:?}",
                e
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            event!(Level::ERROR, "task error in add_script: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::routes::v1::scripts::list_scripts::{ListScriptsResponse, ScriptResponse};
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_add_script() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "kiki.on(\"entry.ingest\", function(entry) return entry end)".to_string(),
                kind: "user".to_string(),
            })
            .send()
            .await?;

        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = resp.json::<AddScriptResponse>().await?;
        assert!(body.id > 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_add_script_appears_in_list() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let text = "kiki.on(\"entry.ingest\", function(entry) return entry end)";

        client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: text.to_string(),
                kind: "user".to_string(),
            })
            .send()
            .await?;

        let resp = client.get("http://localhost/v1/scripts").send().await?;

        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.json::<ListScriptsResponse>().await?;
        assert_eq!(body.count, 1);
        assert_eq!(body.scripts[0].engine, "lua");
        assert_eq!(body.scripts[0].text, text);
        assert_eq!(body.scripts[0].kind, "user");

        Ok(())
    }

    #[tokio::test]
    async fn test_add_script_with_explicit_kind() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "kiki.on(\"entry.ingest\", function(entry) return entry end)".to_string(),
                kind: "system".to_string(),
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
        assert_eq!(body.kind, "system");

        Ok(())
    }
}
