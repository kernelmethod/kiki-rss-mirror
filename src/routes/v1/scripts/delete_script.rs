use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::task;
use tracing::{event, Level};

/// Route handler for deleting a script by ID.
///
/// After deleting the script, a reload is triggered so that the removal takes effect immediately.
#[utoipa::path(
    delete,
    path = "/v1/scripts/id/{id}",
    params(
        ("id" = i64, Path, description = "Script ID"),
    ),
    responses(
        (status = 204, description = "Script deleted successfully"),
        (status = 404, description = "Script not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "scripts"
)]
#[axum::debug_handler]
pub async fn delete_script(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let conn = state.conn_pool.get().map_err(|e| {
        event!(Level::ERROR, "failed to get database connection: {:?}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
    })?;

    let result = task::spawn_blocking(move || {
        let affected_rows = conn
            .prepare("DELETE FROM scripts WHERE id = ?1")
            .inspect_err(|e| {
                event!(Level::ERROR, "unable to prepare SQL statement: {:?}", e);
            })?
            .execute([id])?;

        Ok::<usize, rusqlite::Error>(affected_rows)
    })
    .await
    .inspect_err(|e| {
        event!(Level::ERROR, "task error in delete_script: {:?}", e);
    });

    match result {
        Ok(Ok(0)) => Ok((StatusCode::NOT_FOUND, "Script not found").into_response()),
        Ok(Ok(_)) => {
            use crate::tasks::TaskManagerCommand;
            if let Err(e) = state
                .task_manager_tx
                .send(TaskManagerCommand::ReloadScripts)
                .await
            {
                event!(
                    Level::ERROR,
                    "failed to send ReloadScripts command: {:?}",
                    e
                );
            }

            Ok((StatusCode::NO_CONTENT, "").into_response())
        }
        Ok(Err(_)) | Err(_) => {
            event!(
                Level::ERROR,
                "an error occurred while running delete_script"
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use crate::routes::v1::entries::ListEntriesResponse;
    use crate::routes::v1::feeds::add_feed::{AddFeedRequest, AddFeedResponse};
    use crate::routes::v1::scripts::add_script::{AddScriptRequest, AddScriptResponse};
    use crate::routes::v1::scripts::list_scripts::ListScriptsResponse;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::http::StatusCode;
    use std::time::Duration;

    #[tokio::test]
    async fn test_delete_script() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "return function(entry) return entry end".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let id = resp.json::<AddScriptResponse>().await?.id;

        // Verify the script exists.
        let resp = client.get("http://localhost/v1/scripts").send().await?;
        let body = resp.json::<ListScriptsResponse>().await?;
        assert_eq!(body.count, 1);

        // Delete it.
        let resp = client
            .delete(format!("http://localhost/v1/scripts/id/{id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Verify it's gone.
        let resp = client.get("http://localhost/v1/scripts").send().await?;
        let body = resp.json::<ListScriptsResponse>().await?;
        assert_eq!(body.count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_script_not_found() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .delete("http://localhost/v1/scripts/id/999")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text().await?, "Script not found");

        Ok(())
    }

    /// Verify that deleting a filter script triggers a reload by observing
    /// entries appear on a subsequent fetch. Start with a filter-all script,
    /// fetch (no entries), delete the script (reload fires), re-fetch, and
    /// confirm entries now appear.
    #[tokio::test]
    async fn test_delete_script_triggers_reload() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        // Create a filter-all script.
        let resp = client
            .post("http://localhost/v1/scripts/create")
            .json(&AddScriptRequest {
                engine: "lua".to_string(),
                text: "return function(entry) return nil end".to_string(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let script_id = resp.json::<AddScriptResponse>().await?.id;

        // Add a feed.
        let resp = client
            .post("http://localhost/v1/feeds/create")
            .json(&AddFeedRequest {
                title: "test feed".to_string(),
                url: tc.example_feed_url(),
            })
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let feed_id = resp.json::<AddFeedResponse>().await?.id;

        // Wait for the initial fetch.
        std::thread::sleep(Duration::from_millis(250));

        // No entries should exist because the filter script drops everything.
        let resp = client.get("http://localhost/v1/entries").send().await?;
        let body = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(body.count, 0, "filter script should have prevented entries");

        // Delete the filter script (triggers reload) and reset last_checked
        // so the next fetch isn't skipped by the 3-hour freshness check.
        let resp = client
            .delete(format!("http://localhost/v1/scripts/id/{script_id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        {
            let conn = tc.database_conn()?;
            conn.execute(
                "UPDATE feeds SET last_checked = NULL WHERE id = ?1",
                [feed_id],
            )?;
        }

        // Allow time for the reload command to be processed before fetching.
        std::thread::sleep(Duration::from_millis(250));

        // Re-fetch the feed.
        let resp = client
            .post(format!("http://localhost/v1/feeds/fetch/{feed_id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // Wait for the fetch to complete.
        std::thread::sleep(Duration::from_millis(500));

        // Entries should now appear since the filter script was removed.
        let resp = client.get("http://localhost/v1/entries").send().await?;
        let body = resp.json::<ListEntriesResponse>().await?;
        assert!(
            body.count > 0,
            "entries should appear after filter script was deleted and reload triggered"
        );

        Ok(())
    }
}
