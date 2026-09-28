use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

use super::list_plugins::{discover, PluginResponse};

/// Get plugin information
///
/// Retrieve the manifest and config of an installed plugin by its name.
#[utoipa::path(
    get,
    path = "/v1/plugins/name/{name}",
    params(
        ("name" = String, Path, description = "Plugin name"),
    ),
    responses(
        (status = 200, description = "Plugin found", body = PluginResponse),
        (status = 404, description = "Plugin not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn get_plugin(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    let discovery = discover(&state).await?;
    match discovery
        .plugins
        .into_iter()
        .find(|p| p.manifest.name == name)
    {
        Some(plugin) => Ok(Json(PluginResponse::from(plugin)).into_response()),
        None => Err((StatusCode::NOT_FOUND, "Plugin not found").into_response()),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_get_plugin() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        tc.install_lua_plugin("hello", "", serde_json::json!({}))?;

        let resp = client
            .get("http://localhost/v1/plugins/name/hello")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginResponse>().await?;
        assert_eq!(body.name, "hello");
        assert_eq!(body.version, "1.0.0");

        Ok(())
    }

    #[tokio::test]
    async fn test_get_plugin_not_found() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client
            .get("http://localhost/v1/plugins/name/missing")
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.text().await?, "Plugin not found");

        Ok(())
    }
}
