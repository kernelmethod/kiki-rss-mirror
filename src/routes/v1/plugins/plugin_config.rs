//! Routes for reading and overriding a plugin's config.
//!
//! A plugin's config is the `[config]` table of its manifest (its defaults),
//! with its overrides, kept in the database, applied over it key by key.
//! These routes read and write the overrides. Like any other change to a
//! plugin, a new config takes effect when the server restarts; until then,
//! the plugin keeps running with the config it was loaded with.

use crate::db::plugins::{self as db, ConfigOverrides, PluginConfigError};
use crate::plugins::{apply_config_overrides, Plugin};
use crate::server::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rusqlite::Connection;
use serde_json::{Map, Value};
use tokio::task;
use tracing::{event, Level};

/// A plugin's config, and where each part of it comes from.
#[derive(serde::Deserialize, serde::Serialize, utoipa::ToSchema)]
pub struct PluginConfigResponse {
    /// The plugin's default config, from its manifest.
    #[schema(value_type = Object)]
    pub defaults: Map<String, Value>,
    /// The plugin's config overrides.
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
    fn new(plugin: &Plugin, overrides: ConfigOverrides) -> Self {
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

/// Runs `op` on a database connection on the blocking thread pool, for the
/// plugin named `name`, and answers with the plugin's config and the
/// overrides `op` returns.
///
/// A plugin that was not discovered when the server started is answered
/// with `404 Not Found`, and overrides too large to save with
/// `413 Payload Too Large`. Any other failure is a
/// `500 Internal Server Error`.
async fn with_plugin<F>(state: &AppState, name: String, op: F) -> Result<Response, Response>
where
    F: FnOnce(&mut Connection, &str) -> Result<ConfigOverrides, PluginConfigError> + Send + 'static,
{
    let Some(plugin) = state
        .plugins
        .plugins
        .iter()
        .find(|p| p.manifest.name == name)
    else {
        return Err((StatusCode::NOT_FOUND, "Plugin not found").into_response());
    };

    let pool = state.conn_pool.clone();
    let result = task::spawn_blocking(move || {
        let mut conn = pool.get()?;
        op(&mut conn, &name).map_err(anyhow::Error::from)
    })
    .await;

    match result {
        Ok(Ok(overrides)) => Ok(Json(PluginConfigResponse::new(plugin, overrides)).into_response()),
        Ok(Err(e)) => match e.downcast_ref::<PluginConfigError>() {
            Some(PluginConfigError::TooLarge { .. }) => {
                Err((StatusCode::PAYLOAD_TOO_LARGE, e.to_string()).into_response())
            }
            _ => {
                event!(Level::ERROR, "failed to access plugin config: {:#}", e);
                Err((StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response())
            }
        },
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

/// Get plugin config
///
/// Retrieve a plugin's config: its defaults, from its manifest; its overrides; the config
/// those add up to, which the plugin is loaded with the next time
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
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn get_plugin_config(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, Response> {
    with_plugin(&state, name, |conn, name| {
        db::get_config_overrides(conn, name)
    })
    .await
}

/// Replace plugin config overrides
///
/// Replace every one of a plugin's config overrides with the keys of the request body,
/// which must be a JSON object. Keys left out fall back to the defaults from the plugin's
/// manifest; an empty object removes every override. A key set to `null` is passed to the
/// plugin as `nil`, hiding its default.
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
    with_plugin(&state, name, move |conn, name| {
        db::set_config_overrides(conn, name, &overrides)?;
        Ok(overrides)
    })
    .await
}

/// Update plugin config overrides
///
/// Set the config overrides of a plugin named by the keys of the request body, which must
/// be a JSON object, keeping its other overrides. Each key replaces the whole of
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
    with_plugin(&state, name, move |conn, name| {
        db::update_config_overrides(conn, name, |overrides| overrides.extend(changes))
    })
    .await
}

/// Remove plugin config overrides
///
/// Remove every one of a plugin's config overrides, restoring every key to the default
/// from the plugin's manifest.
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
    with_plugin(&state, name, |conn, name| {
        let overrides = Map::new();
        db::set_config_overrides(conn, name, &overrides)?;
        Ok(overrides)
    })
    .await
}

/// Remove a plugin config override
///
/// Remove one of a plugin's config overrides, restoring it to the default from the
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
        (status = 500, description = "Internal server error"),
    ),
    tag = "plugins"
)]
#[axum::debug_handler]
pub async fn delete_plugin_config_key(
    State(state): State<AppState>,
    Path((name, key)): Path<(String, String)>,
) -> Result<Response, Response> {
    with_plugin(&state, name, move |conn, name| {
        db::update_config_overrides(conn, name, |overrides| {
            overrides.remove(&key);
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

    /// A test context with one plugin, `hello`, whose defaults are
    /// `{"a": 1, "b": 2}`, and a database, but no server yet.
    fn installed() -> Result<TestConfig> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin("hello", "", json!({"a": 1, "b": 2}))?;
        Ok(tc)
    }

    /// [`installed`], with the server running.
    fn server() -> Result<TestConfig> {
        installed()?.init_server()
    }

    /// The overrides stored in the database for `hello`.
    fn stored(tc: &TestConfig) -> Result<Value> {
        Ok(Value::Object(db::get_config_overrides(
            &tc.database_conn()?,
            "hello",
        )?))
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
    async fn test_overrides_apply_when_the_server_starts() -> Result<()> {
        let tc = installed()?;
        let overrides = json!({"b": 3});
        db::set_config_overrides(
            &tc.database_conn()?,
            "hello",
            overrides.as_object().unwrap_or(&Map::new()),
        )?;
        let tc = tc.init_server()?;
        let client = tc.client()?;

        let resp = client.get(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.active), json!({"a": 1, "b": 3}));
        assert!(!body.restart_required);

        // The plugin list shows the config in effect too.
        let resp = client
            .get("http://localhost/v1/plugins/name/hello")
            .send()
            .await?;
        let body: Value = resp.json().await?;
        assert_eq!(body["config"], json!({"a": 1, "b": 3}));

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
        assert_eq!(stored(&tc)?, json!({"b": 3, "c": [1, 2]}));

        // PUT replaces every override.
        let resp = client.put(URL).json(&json!({"a": null})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({"a": null}));
        assert_eq!(Value::Object(body.config), json!({"a": null, "b": 2}));
        assert_eq!(stored(&tc)?, json!({"a": null}));

        Ok(())
    }

    #[tokio::test]
    async fn test_put_plugin_config_rejects_non_objects() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.put(URL).json(&json!([1, 2])).send().await?;
        assert!(resp.status().is_client_error(), "{}", resp.status());
        assert_eq!(stored(&tc)?, json!({}));

        Ok(())
    }

    #[tokio::test]
    async fn test_put_plugin_config_rejects_oversized_configs() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let big = "x".repeat(crate::plugins::MAX_CONFIG_BYTES as usize);
        let resp = client.put(URL).json(&json!({"a": big})).send().await?;
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(stored(&tc)?, json!({}));

        Ok(())
    }

    #[tokio::test]
    async fn test_patch_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.put(URL).json(&json!({"a": 10})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

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
        assert_eq!(stored(&tc)?, json!({"a": 10, "b": {"y": 2}, "c": null}));

        Ok(())
    }

    #[tokio::test]
    async fn test_delete_plugin_config() -> Result<()> {
        let tc = server()?;
        let client = tc.client()?;

        let resp = client.put(URL).json(&json!({"a": 5})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = client.delete(URL).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.overrides), json!({}));
        assert_eq!(Value::Object(body.config), json!({"a": 1, "b": 2}));
        assert!(!body.restart_required);
        assert_eq!(stored(&tc)?, json!({}));

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

        let resp = client.delete(format!("{URL}/c")).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(stored(&tc)?, json!({}));

        Ok(())
    }
}
