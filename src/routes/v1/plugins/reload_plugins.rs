use crate::server::AppState;
use axum::{extract::State, http::StatusCode, response::IntoResponse};
use tracing::error;

/// Reload all plugins
///
/// Queue a rescan of the plugins directory, picking up any plugins that have been
/// installed, removed or edited. Calls to this endpoint are usually unnecessary, as the
/// server watches the plugins directory and reloads plugins whenever it changes.
#[utoipa::path(
    post,
    path = "/v1/plugins/reload",
    responses(
        (status = 202, description = "Plugin reload queued successfully"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn reload_plugins(State(state): State<AppState>) -> impl IntoResponse {
    if let Err(e) = state.reload_tx.send(()) {
        error!("failed to send plugin reload signal: {:?}", e);
        return StatusCode::INTERNAL_SERVER_ERROR;
    }
    StatusCode::ACCEPTED
}

#[cfg(all(test, feature = "lua", feature = "metrics"))]
mod test {
    use super::*;
    use crate::routes::v1::entries::ListEntriesResponse;
    use crate::routes::v1::feeds::add_feed::{AddFeedRequest, AddFeedResponse};
    use crate::test::TestBuilder;
    use anyhow::Result;

    /// Installing a plugin is picked up without an explicit reload, because
    /// the server watches the plugins directory; so is removing it. Start
    /// with a filter-all plugin, fetch (no entries), remove the plugin,
    /// re-fetch, and confirm entries now appear.
    #[tokio::test]
    async fn test_plugin_changes_are_picked_up() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let dir = tc.install_lua_plugin(
            "drop-everything",
            r#"kiki.on("entry.ingest", function(entry) return nil end)"#,
            serde_json::json!({}),
        )?;
        tc.wait_for_metric("kiki_scripts_loaded", |n| n == 1.0)
            .await?;

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

        let resp = client.get("http://localhost/v1/entries").send().await?;
        let body = resp.json::<ListEntriesResponse>().await?;
        assert_eq!(body.count, 0, "filter plugin should have prevented entries");

        // Remove the plugin, and reset last_checked so the next fetch isn't
        // skipped by the freshness check.
        std::fs::remove_dir_all(dir)?;
        tc.database_conn()?.execute(
            "UPDATE feeds SET last_checked = NULL, next_fetch_at = NULL WHERE id = ?1",
            [feed_id],
        )?;
        tc.wait_for_metric("kiki_scripts_loaded", |n| n == 0.0)
            .await?;

        let resp = client
            .post(format!("http://localhost/v1/feeds/refresh/{feed_id}"))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        tc.wait_for_fetches(2).await?;

        let resp = client.get("http://localhost/v1/entries").send().await?;
        let body = resp.json::<ListEntriesResponse>().await?;
        assert!(
            body.count > 0,
            "entries should appear after the filter plugin was removed"
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_reload_plugins() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .post("http://localhost/v1/plugins/reload")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        Ok(())
    }
}
