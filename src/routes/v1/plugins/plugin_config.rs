//! Routes for reading and overriding a plugin's config.
//!
//! A plugin's config is the `[config]` table of its manifest (its defaults),
//! with the keys of the `config.json` in its directory (its overrides)
//! applied over it. These routes read and write the overrides. Like any
//! other change to a plugin, a new config takes effect when the server
//! restarts; until then, the plugin keeps running with the config it was
//! loaded with.

use crate::plugins::{apply_config_overrides, Plugin, PluginError, CONFIG_FILE_NAME};
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{Map, Value};
use tokio::task;
use tracing::{event, Level};

/// A plugin's config, and where each part of it comes from.
#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct PluginConfigResponse {
    /// The plugin's default config, from its manifest.
    #[schema(value_type = Object)]
    pub defaults: Map<String, Value>,
    /// The overrides currently saved in the plugin's `config.json`.
    #[schema(value_type = Object)]
    pub overrides: Map<String, Value>,
    /// The defaults with the overrides applied: the config the plugin will be
    /// loaded with the next time the server starts.
    #[schema(value_type = Object)]
    pub config: Map<String, Value>,
    /// The config the plugin is running with, as it was when the server
    /// started.
    #[schema(value_type = Object)]
    pub active: Map<String, Value>,
    /// Whether `config` differs from `active`, so that the server must be
    /// restarted for the plugin to see its new config.
    pub restart_required: bool,
}

impl PluginConfigResponse {
    fn new(plugin: &Plugin, overrides: Map<String, Value>) -> Self {
        let config = apply_config_overrides(&plugin.manifest.config, overrides.clone());
        PluginConfigResponse {
            defaults: plugin.manifest.config.clone(),
            restart_required: config != plugin.config,
            active: plugin.config.clone(),
            overrides,
            config,
        }
    }
}

/// Finds the plugin named `name`, runs `op` on it on the blocking thread
/// pool, and answers with the plugin's config and the overrides `op`
/// returns.
///
/// A plugin that was not discovered when the server started is answered
/// with `404 Not Found`. An error from `op` is mapped to a status code by
/// [`error_response`].
async fn with_plugin<F>(state: &AppState, name: &str, op: F) -> Result<Response, Response>
where
    F: FnOnce(&Plugin) -> Result<Map<String, Value>, PluginError> + Send + 'static,
{
    if !state
        .plugins
        .plugins
        .iter()
        .any(|p| p.manifest.name == name)
    {
        return Err((StatusCode::NOT_FOUND, "Plugin not found").into_response());
    }

    let plugins = state.plugins.clone();
    let name = name.to_string();
    let result = task::spawn_blocking(move || {
        // Checked above, and discovery never changes while the server runs.
        let plugin = plugins.plugins.iter().find(|p| p.manifest.name == name)?;
        Some(op(plugin).map(|overrides| PluginConfigResponse::new(plugin, overrides)))
    })
    .await;

    match result {
        Ok(Some(Ok(body))) => Ok(Json(body).into_response()),
        Ok(Some(Err(e))) => Err(error_response(e)),
        Ok(None) => Err((StatusCode::NOT_FOUND, "Plugin not found").into_response()),
        Err(e) => {
            event!(
                Level::ERROR,
                "task error while accessing plugin config: {:?}",
                e
            );
            Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
        }
    }
}

/// Maps an error reading or writing a plugin's config file to a response.
///
/// A config file on disk that cannot be parsed is answered with
/// `409 Conflict`, since it must be replaced or removed before it can be
/// edited; a new config too large to save with `413 Payload Too Large`; and
/// anything else with `500 Internal Server Error`.
fn error_response(e: PluginError) -> Response {
    match e {
        PluginError::InvalidConfig(_) | PluginError::NotUtf8(_) | PluginError::TooLarge { .. } => {
            event!(Level::WARN, "plugin config file is invalid: {}", e);
            (
                StatusCode::CONFLICT,
                format!(
                    "the plugin's {CONFIG_FILE_NAME} is invalid; replace it with PUT or \
                     remove it with DELETE: {e}"
                ),
            )
                .into_response()
        }
        PluginError::ConfigTooLarge { .. } => {
            (StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response()
        }
        e => {
            event!(Level::ERROR, "failed to access plugin config: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
        }
    }
}

/// Get plugin config
///
/// Retrieve a plugin's config: its defaults, from its manifest; the overrides saved in its
/// `config.json`; the config those add up to, which the plugin is loaded with the next time
/// the server starts; and the config it is running with now.
#[utoipa::path(
    get,
    path = "/v1/plugins/name/{name}/config",
    params(
        ("name" = String, Path, description = "Plugin name"),
    ),
    responses(
        (status = 200, description = "The plugin's config", body = PluginConfigResponse),
        (status = 404, description = "Plugin not found"),
        (status = 409, description = "The plugin's config.json is invalid"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn get_plugin_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    with_plugin(&state, &name, |plugin| plugin.read_config_overrides()).await
}

/// Replace plugin config overrides
///
/// Replace every override in a plugin's `config.json` with the keys of the request body,
/// which must be a JSON object. Keys left out fall back to the defaults from the plugin's
/// manifest; an empty object removes every override. A key set to `null` is passed to the
/// plugin as `nil`, hiding its default. Replaces the file even if it was invalid.
///
/// The new config takes effect the next time the server starts.
#[utoipa::path(
    put,
    path = "/v1/plugins/name/{name}/config",
    params(
        ("name" = String, Path, description = "Plugin name"),
    ),
    request_body(content = Object, description = "The plugin's new config overrides"),
    responses(
        (status = 200, description = "The plugin's updated config", body = PluginConfigResponse),
        (status = 404, description = "Plugin not found"),
        (status = 413, description = "The overrides are too large"),
        (status = 422, description = "The request body is not a JSON object"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn put_plugin_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(overrides): Json<Map<String, Value>>,
) -> Result<Response, Response> {
    with_plugin(&state, &name, move |plugin| {
        plugin.replace_config_overrides(&overrides)?;
        Ok(overrides)
    })
    .await
}

/// Update plugin config overrides
///
/// Set the overrides in a plugin's `config.json` named by the keys of the request body,
/// which must be a JSON object, keeping its other overrides. Each key replaces the whole of
/// its override; nested objects are not merged. A key set to `null` is saved as `null`,
/// which is passed to the plugin as `nil`, hiding its default; to restore a key's default,
/// delete its override instead.
///
/// The new config takes effect the next time the server starts.
#[utoipa::path(
    patch,
    path = "/v1/plugins/name/{name}/config",
    params(
        ("name" = String, Path, description = "Plugin name"),
    ),
    request_body(content = Object, description = "The config overrides to set"),
    responses(
        (status = 200, description = "The plugin's updated config", body = PluginConfigResponse),
        (status = 404, description = "Plugin not found"),
        (status = 409, description = "The plugin's config.json is invalid"),
        (status = 413, description = "The overrides are too large"),
        (status = 422, description = "The request body is not a JSON object"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn patch_plugin_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(changes): Json<Map<String, Value>>,
) -> Result<Response, Response> {
    with_plugin(&state, &name, move |plugin| {
        plugin.update_config_overrides(|overrides| {
            overrides.extend(changes);
            Ok(())
        })
    })
    .await
}

/// Remove plugin config overrides
///
/// Remove a plugin's `config.json`, restoring every key to the default from the plugin's
/// manifest. Removes the file even if it was invalid.
///
/// The new config takes effect the next time the server starts.
#[utoipa::path(
    delete,
    path = "/v1/plugins/name/{name}/config",
    params(
        ("name" = String, Path, description = "Plugin name"),
    ),
    responses(
        (status = 200, description = "The plugin's updated config", body = PluginConfigResponse),
        (status = 404, description = "Plugin not found"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn delete_plugin_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    with_plugin(&state, &name, |plugin| {
        let overrides = Map::new();
        plugin.replace_config_overrides(&overrides)?;
        Ok(overrides)
    })
    .await
}

/// Remove a plugin config override
///
/// Remove one key from a plugin's `config.json`, restoring it to the default from the
/// plugin's manifest, if it has one. Removing a key that is not overridden changes nothing.
///
/// The new config takes effect the next time the server starts.
#[utoipa::path(
    delete,
    path = "/v1/plugins/name/{name}/config/{key}",
    params(
        ("name" = String, Path, description = "Plugin name"),
        ("key" = String, Path, description = "Config key"),
    ),
    responses(
        (status = 200, description = "The plugin's updated config", body = PluginConfigResponse),
        (status = 404, description = "Plugin not found"),
        (status = 409, description = "The plugin's config.json is invalid"),
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn delete_plugin_config_key(
    State(state): State<AppState>,
    Path((name, key)): Path<(String, String)>,
) -> Result<Response, Response> {
    with_plugin(&state, &name, move |plugin| {
        plugin.update_config_overrides(|overrides| {
            overrides.remove(&key);
            Ok(())
        })
    })
    .await
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod test {
    use super::*;
    use crate::test::{TestBuilder, TestConfig};
    use anyhow::Result;
    use serde_json::json;

    const URL: &str = "http://localhost/v1/plugins/name/hello/config";

    /// A server with one plugin, `hello`, whose defaults are `{"a": 1, "b": 2}`.
    fn server() -> Result<TestConfig> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin("hello", "", json!({"a": 1, "b": 2}))?;
        tc.init_server()
    }

    fn config_file(tc: &TestConfig) -> std::path::PathBuf {
        tc.plugins_dir().join("hello").join(CONFIG_FILE_NAME)
    }

    #[tokio::test]
    async fn test_get_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.get(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.defaults), json!({"a": 1, "b": 2}));
        assert_eq!(Value::Object(body.overrides), json!({}));
        assert_eq!(Value::Object(body.config), json!({"a": 1, "b": 2}));
        assert_eq!(Value::Object(body.active), json!({"a": 1, "b": 2}));
        assert!(!body.restart_required);

        Ok(())
    }

    #[tokio::test]
    async fn test_plugin_config_not_found() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;
        let url = "http://localhost/v1/plugins/name/nonexistent/config";

        assert_eq!(
            client.get(url).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client.put(url).json(&json!({})).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client.patch(url).json(&json!({})).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client.delete(url).send().await?.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            client.delete(format!("{url}/a")).send().await?.status(),
            StatusCode::NOT_FOUND
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_put_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client
            .put(URL)
            .json(&json!({"b": 3, "c": [1, 2]}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({"b": 3, "c": [1, 2]}));
        assert_eq!(
            Value::Object(body.config),
            json!({"a": 1, "b": 3, "c": [1, 2]})
        );
        // The running plugin keeps its old config until a restart.
        assert_eq!(Value::Object(body.active), json!({"a": 1, "b": 2}));
        assert!(body.restart_required);

        // PUT replaces every override.
        let resp = client.put(URL).json(&json!({"a": null})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({"a": null}));
        assert_eq!(Value::Object(body.config), json!({"a": null, "b": 2}));

        // The overrides are saved where the plugin loader reads them.
        let plugin = Plugin::load(&tc.plugins_dir().join("hello"))?;
        assert_eq!(Value::Object(plugin.config), json!({"a": null, "b": 2}));

        Ok(())
    }

    #[tokio::test]
    async fn test_put_plugin_config_rejects_non_objects() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.put(URL).json(&json!([1, 2])).send().await?;
        assert!(resp.status().is_client_error(), "{}", resp.status());
        assert!(!config_file(&tc).exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_put_plugin_config_rejects_oversized_configs() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let big = "x".repeat(crate::plugins::MAX_MANIFEST_BYTES as usize);
        let resp = client.put(URL).json(&json!({"a": big})).send().await?;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(!config_file(&tc).exists());

        Ok(())
    }

    #[tokio::test]
    async fn test_patch_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        // Overrides made by hand are kept.
        std::fs::write(config_file(&tc), r#"{"a": 10}"#)?;

        let resp = client
            .patch(URL)
            .json(&json!({"b": {"x": 1}, "c": null}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(
            Value::Object(body.overrides),
            json!({"a": 10, "b": {"x": 1}, "c": null})
        );
        assert_eq!(
            Value::Object(body.config),
            json!({"a": 10, "b": {"x": 1}, "c": null})
        );
        assert!(body.restart_required);

        // Nested objects are replaced, not merged.
        let resp = client
            .patch(URL)
            .json(&json!({"b": {"y": 2}}))
            .send()
            .await?;
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(body.overrides["b"], json!({"y": 2}));

        Ok(())
    }

    #[tokio::test]
    async fn test_patch_refuses_an_invalid_config_file() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        std::fs::write(config_file(&tc), "[1, 2]")?;

        let resp = client.patch(URL).json(&json!({"a": 3})).send().await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(config_file(&tc))?, "[1, 2]");

        let resp = client.get(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // PUT replaces the broken file.
        let resp = client.put(URL).json(&json!({"a": 3})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = client.get(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.put(URL).json(&json!({"a": 5})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(config_file(&tc).exists());

        let resp = client.delete(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({}));
        assert_eq!(Value::Object(body.config), json!({"a": 1, "b": 2}));
        assert!(!body.restart_required);
        assert!(!config_file(&tc).exists());

        // Deleting again is harmless.
        let resp = client.delete(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_plugin_config_key() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client
            .put(URL)
            .json(&json!({"a": 5, "c": 6}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = client.delete(format!("{URL}/a")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({"c": 6}));
        assert_eq!(Value::Object(body.config), json!({"a": 1, "b": 2, "c": 6}));

        // Removing a key that is not overridden changes nothing.
        let resp = client.delete(format!("{URL}/b")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({"c": 6}));

        // Removing the last override removes the file.
        let resp = client.delete(format!("{URL}/c")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(!config_file(&tc).exists());

        Ok(())
    }
}
