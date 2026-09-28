use crate::plugins::{Plugin, PluginEngine};
use crate::server::AppState;
use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};

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

impl From<&Plugin> for PluginResponse {
    fn from(plugin: &Plugin) -> Self {
        let m = &plugin.manifest;
        PluginResponse {
            name: m.name.clone(),
            version: m.version.clone(),
            engine: m.engine,
            engine_supported: m.engine.is_supported(),
            enabled: m.enabled,
            entrypoint: m.entrypoint().to_string(),
            directory: plugin.dir_name(),
            description: m.description.clone(),
            authors: m.authors.clone(),
            license: m.license.clone(),
            homepage: m.homepage.clone(),
            config: plugin.config.clone(),
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

/// List plugins
///
/// Return every plugin that was found in the plugins directory when the server started,
/// along with the directories in it that could not be loaded as plugins and why. Plugins
/// installed or changed since then take effect, and appear here, after a restart.
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
pub async fn list_plugins(State(state): State<AppState>) -> Response {
    let plugins: Vec<PluginResponse> = state.plugins.plugins.iter().map(Into::into).collect();
    let errors = state
        .plugins
        .errors
        .iter()
        .map(|e| PluginErrorResponse {
            directory: e
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            error: e.error.to_string(),
        })
        .collect();
    Json(ListPluginsResponse {
        count: plugins.len(),
        plugins,
        errors,
    })
    .into_response()
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::test::TestBuilder;
    use anyhow::Result;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn test_list_plugins() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin(
            "passthrough",
            r#"kiki.on("entry.ingest", function(entry) return entry end)"#,
            serde_json::json!({"x": 1}),
        )?;
        std::fs::create_dir_all(tc.plugins_dir().join("broken"))?;
        let tc = tc.init_server()?;
        let client = tc.client()?;

        // Plugins installed after the server started are not picked up.
        tc.install_lua_plugin("too-late", "", serde_json::json!({}))?;

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
        assert!(body.errors[0].error.contains("manifest.toml"));

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
