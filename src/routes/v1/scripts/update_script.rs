use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use thiserror::Error;
use tokio::task;
use tracing::{event, Level};

use super::list_scripts::{ScriptResponse, SCRIPT_COLUMNS};

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct UpdateScriptRequest {
    pub engine: Option<String>,
    pub text: Option<String>,
    pub kind: Option<String>,
    /// Replaces the script's config, which is handed to its top-level chunk as its
    /// argument.
    #[schema(value_type = Option<Object>)]
    pub config: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Error, Debug)]
enum UpdateScriptTaskError {
    #[error("script not found")]
    ScriptNotFound,

    #[error("invalid update parameters")]
    InvalidUpdate,

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

/// Update a script
///
/// Update a script by its ID. Calls to this endpoint queue a script reload in all worker threads.
#[utoipa::path(
    put,
    path = "/v1/scripts/id/{id}",
    params(
        ("id" = i64, Path, description = "Script ID"),
    ),
    request_body = UpdateScriptRequest,
    responses(
        (status = 200, description = "Script updated successfully", body = ScriptResponse),
        (status = 400, description = "No update fields provided"),
        (status = 404, description = "Script not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn update_script(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<UpdateScriptRequest>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let exists: bool = conn
            .prepare("SELECT EXISTS(SELECT 1 FROM scripts WHERE id = ?1)")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .query_row([id], |row| row.get(0))?;

        if !exists {
            return Err(UpdateScriptTaskError::ScriptNotFound);
        }

        let mut updates = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![];

        if let Some(engine) = &payload.engine {
            updates.push("engine = ?".to_string());
            params.push(Box::new(engine.clone()));
        }

        if let Some(text) = &payload.text {
            updates.push("text = ?".to_string());
            params.push(Box::new(text.clone()));
        }

        if let Some(kind) = &payload.kind {
            updates.push("kind = ?".to_string());
            params.push(Box::new(kind.clone()));
        }

        if let Some(config) = &payload.config {
            updates.push("config = ?".to_string());
            params.push(Box::new(
                serde_json::Value::Object(config.clone()).to_string(),
            ));
        }

        if updates.is_empty() {
            return Err(UpdateScriptTaskError::InvalidUpdate);
        }

        params.push(Box::new(id));
        let query = format!("UPDATE scripts SET {} WHERE id = ?", updates.join(", "));

        let params = rusqlite::params_from_iter(params);
        conn.execute(&query, params).inspect_err(|e| {
            event!(Level::ERROR, "unable to execute update statement: {:?}", e);
        })?;

        let script = conn
            .prepare(&format!(
                "SELECT {SCRIPT_COLUMNS} FROM scripts WHERE id = ?1 LIMIT 1"
            ))
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare select statement: {:?}", e);
            })?
            .query_row([id], ScriptResponse::from_row)?;

        Ok::<ScriptResponse, UpdateScriptTaskError>(script)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in update_script: {:?}", e);
    });

    match result {
        Ok(Ok(script)) => {
            if let Err(e) = state.reload_tx.send(()) {
                event!(Level::ERROR, "failed to send ReloadScripts signal: {:?}", e);
            }

            Ok(Json(script).into_response())
        }
        Ok(Err(UpdateScriptTaskError::ScriptNotFound)) => {
            Err((StatusCode::NOT_FOUND, "Script not found").into_response())
        }
        Ok(Err(UpdateScriptTaskError::InvalidUpdate)) => {
            Err((StatusCode::BAD_REQUEST, "No update fields provided").into_response())
        }
        Ok(Err(UpdateScriptTaskError::Database(_))) => {
            event!(Level::ERROR, "database error in update_script");
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
        Err(_) => {
            event!(
                Level::ERROR,
                "an error occurred while running update_script"
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
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
    async fn test_update_script() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "kiki.on(\"entry.ingest\", function(entry) return entry end)".to_string(),
                kind: "user".to_string(),
                config: Default::default(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = resp.json::<AddScriptResponse>().await?.id;

        let new_text = "kiki.on(\"entry.ingest\", function(entry) return nil end)";
        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{id}"))
            .json(&UpdateScriptRequest {
                engine: None,
                text: Some(new_text.to_string()),
                kind: None,
                config: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.json::<ScriptResponse>().await?;
        assert_eq!(body.id, id);
        assert_eq!(body.engine, "lua");
        assert_eq!(body.text, new_text);

        Ok(())
    }

    #[tokio::test]
    async fn test_update_script_not_found() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .put("http://localhost/v1/scripts/id/999")
            .json(&UpdateScriptRequest {
                engine: Some("lua".to_string()),
                text: None,
                kind: None,
                config: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text().await?, "Script not found");

        Ok(())
    }

    #[tokio::test]
    async fn test_update_script_no_fields() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "kiki.on(\"entry.ingest\", function(entry) return entry end)".to_string(),
                kind: "user".to_string(),
                config: Default::default(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = resp.json::<AddScriptResponse>().await?.id;

        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{id}"))
            .json(&UpdateScriptRequest {
                engine: None,
                text: None,
                kind: None,
                config: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        Ok(())
    }

    /// Verify that updating a script triggers a reload by observing a
    /// behavioral change: start with a passthrough script, fetch a feed
    /// (entries appear), then update the script to filter everything out,
    /// delete existing entries, re-fetch, and confirm no new entries appear.
    #[cfg(feature = "metrics")]
    #[tokio::test]
    async fn test_update_script_triggers_reload() -> Result<()> {
        use crate::routes::v1::entries::ListEntriesResponse;
        use crate::routes::v1::feeds::add_feed::{AddFeedRequest, AddFeedResponse};
        use std::time::Duration;

        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // Create a passthrough script.
        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "kiki.on(\"entry.ingest\", function(entry) return entry end)".to_string(),
                kind: "user".to_string(),
                config: Default::default(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let script_id = resp.json::<AddScriptResponse>().await?.id;

        // Add a feed that points to the example file.
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&AddFeedRequest {
                title: "test feed".to_string(),
                url: tc.example_feed_url(),
                ..Default::default()
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<AddFeedResponse>().await?.id;

        tc.wait_for_fetches(1).await?;

        // Entries should have been inserted (passthrough script).
        let resp = client.get("http://localhost/v1/entries").send().await?;
        let body = resp.json::<ListEntriesResponse>().await?;
        assert!(body.count > 0, "entries should exist after initial fetch");

        // Update the script to a filter-all script.
        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{script_id}"))
            .json(&UpdateScriptRequest {
                engine: None,
                text: Some("kiki.on(\"entry.ingest\", function(entry) return nil end)".to_string()),
                kind: None,
                config: None,
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        // The reload is asynchronous, and swapping one script for another
        // leaves the loaded-script count unchanged, so there is nothing to
        // wait on. Instead, re-fetch until a fetch runs the filter script
        // and stores nothing; without a reload, that never happens.
        const FILTERED: &str = "kiki_script_executions_total{outcome=\"filtered\"}";
        let conn = tc.database_conn()?;
        let start = std::time::Instant::now();
        for fetches in 2.. {
            // Delete existing entries and reset the schedule so the fetch
            // isn't skipped by the freshness check.
            conn.execute("DELETE FROM entries WHERE feed_id = ?1", [feed_id])?;
            conn.execute(
                "UPDATE feeds SET last_checked = NULL, next_fetch_at = NULL WHERE id = ?1",
                [feed_id],
            )?;

            let resp = client
                .post(format!("http://localhost/v1/feeds/refresh/{feed_id}"))
                .send()
                .await?;
            assert_eq!(resp.status(), StatusCode::ACCEPTED);
            tc.wait_for_fetches(fetches).await?;

            let resp = client.get("http://localhost/v1/entries").send().await?;
            let body = resp.json::<ListEntriesResponse>().await?;
            if body.count == 0 && tc.metric(FILTERED).await? > 0.0 {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "filter script should have prevented entries after reload"
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_update_script_config() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let id = client
            .post("http://localhost/v1/scripts/create")
            .json(&serde_json::json!({
                "engine": "lua",
                "text": "local config = ...",
                "config": {"rules": [{"field": "title", "pattern": "a"}]},
            }))
            .send()
            .await?
            .json::<AddScriptResponse>()
            .await?
            .id;

        let resp = client
            .get(format!("http://localhost/v1/scripts/id/{id}"))
            .send()
            .await?;
        let body = resp.json::<ScriptResponse>().await?;
        assert_eq!(
            serde_json::Value::Object(body.config),
            serde_json::json!({"rules": [{"field": "title", "pattern": "a"}]})
        );

        // Updating another field leaves the config alone.
        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{id}"))
            .json(&serde_json::json!({"text": "local config = ... -- v2"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<ScriptResponse>().await?;
        assert_eq!(body.config.len(), 1);

        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{id}"))
            .json(&serde_json::json!({"config": {"rules": []}}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<ScriptResponse>().await?;
        assert_eq!(
            serde_json::Value::Object(body.config),
            serde_json::json!({"rules": []})
        );

        // A config must be an object.
        let resp = client
            .put(format!("http://localhost/v1/scripts/id/{id}"))
            .json(&serde_json::json!({"config": ["not", "an", "object"]}))
            .send()
            .await?;
        assert!(resp.status().is_client_error(), "got {}", resp.status());

        Ok(())
    }
}
