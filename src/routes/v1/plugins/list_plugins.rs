use crate::plugins::{self, Plugin, PluginEngine};
use crate::server::AppState;
use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use tokio::task;
use tracing::{event, Level};

/// A plugin installed in the plugins directory.
#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct PluginResponse {
    /// The plugin's name, from its manifest.
    pub name: String,
    /// The plugin's version, from its manifest.
    pub version: String,
    /// The scripting engine the plugin is written for.
    pub engine: PluginEngine,
    /// Whether this build of Kiki can run the plugin's engine.
    pub engine_supported: bool,
    /// Whether the plugin is enabled in its manifest.
    pub enabled: bool,
    /// The file the plugin's code starts from, relative to its directory.
    pub entrypoint: String,
    /// The name of the plugin's directory inside the plugins directory.
    pub directory: String,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    /// The plugin's config: the defaults from its manifest, overridden by its
    /// `config.json`.
    #[schema(value_type = Object)]
    pub config: serde_json::Map<String, serde_json::Value>,
}

impl From<Plugin> for PluginResponse {
    fn from(plugin: Plugin) -> Self {
        let directory = plugin.dir_name();
        let entrypoint = plugin.manifest.entrypoint().to_string();
        let m = plugin.manifest;
        PluginResponse {
            name: m.name,
            version: m.version,
            engine: m.engine,
            engine_supported: m.engine.is_supported(),
            enabled: m.enabled,
            entrypoint,
            directory,
            description: m.description,
            authors: m.authors,
            license: m.license,
            homepage: m.homepage,
            config: plugin.config,
        }
    }
}

/// A directory in the plugins directory that could not be loaded as a plugin.
#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct PluginErrorResponse {
    /// The name of the directory inside the plugins directory.
    pub directory: String,
    /// Why it could not be loaded.
    pub error: String,
}

#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct ListPluginsResponse {
    /// The plugins that were found, in load order.
    pub plugins: Vec<PluginResponse>,
    /// The number of plugins in `plugins`.
    pub count: usize,
    /// The directories that could not be loaded as plugins.
    pub errors: Vec<PluginErrorResponse>,
}

/// Scan the plugins directory, off the async runtime.
///
/// # Errors
///
/// Returns an error response if the plugins directory cannot be listed.
pub(super) async fn discover(state: &AppState) -> Result<plugins::Discovery, Response> {
    let dir = state.plugins_dir.clone();
    match task::spawn_blocking(move || plugins::discover(&dir)).await {
        Ok(Ok(discovery)) => Ok(discovery),
        Ok(Err(e)) => {
            event!(Level::ERROR, "failed to scan plugins directory: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Err(e) => {
            event!(Level::ERROR, "task error while scanning plugins: {:?}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
    }
}

/// List plugins
///
/// Return every plugin installed in the plugins directory, along with the directories
/// in it that could not be loaded as plugins and why.
#[utoipa::path(
    get,
    path = "/v1/plugins",
    responses(
        (status = 200, description = "List of plugins", body = ListPluginsResponse),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn list_plugins(State(state): State<AppState>) -> Result<Response, Response> {
    let discovery = discover(&state).await?;
    let plugins: Vec<PluginResponse> = discovery.plugins.into_iter().map(Into::into).collect();
    let errors = discovery
        .errors
        .into_iter()
        .map(|e| PluginErrorResponse {
            directory: e
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            error: e.error.to_string(),
        })
        .collect();
    Ok(Json(ListPluginsResponse {
        count: plugins.len(),
        plugins,
        errors,
    })
    .into_response())
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;

    #[tokio::test]
    async fn test_list_plugins() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        tc.install_lua_plugin(
            "passthrough",
            r#"kiki.on("entry.ingest", function(entry) return entry end)"#,
            serde_json::json!({"x": 1}),
        )?;
        std::fs::create_dir_all(tc.plugins_dir().join("broken"))?;

        let resp = client.get("http://localhost/v1/plugins").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.json::<ListPluginsResponse>().await?;
        assert_eq!(body.count, 1);
        let plugin = &body.plugins[0];
        assert_eq!(plugin.name, "passthrough");
        assert_eq!(plugin.version, "1.0.0");
        assert_eq!(plugin.engine, PluginEngine::Lua);
        assert_eq!(plugin.entrypoint, "main.lua");
        assert_eq!(plugin.directory, "passthrough");
        assert!(plugin.enabled);
        assert_eq!(
            serde_json::Value::Object(plugin.config.clone()),
            serde_json::json!({"x": 1})
        );

        assert_eq!(body.errors.len(), 1);
        assert_eq!(body.errors[0].directory, "broken");
        assert!(body.errors[0].error.contains("manifest.json"));

        Ok(())
    }

    #[tokio::test]
    async fn test_list_plugins_when_none_are_installed() -> Result<()> {
        let tc = TestBuilder::all().build()?;
        let client = tc.client()?;

        let resp = client.get("http://localhost/v1/plugins").send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<ListPluginsResponse>().await?;
        assert_eq!(body.count, 0);
        assert!(body.errors.is_empty());

        Ok(())
    }
}
