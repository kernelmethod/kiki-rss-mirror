//! Routes for reading and overriding a plugin's config.
//!
//! A plugin's config is the `[config]` table of its manifest (its defaults),
//! with its overrides, kept in the database, applied over it key by key.
//! These routes read and write the overrides. Changing them reloads the
//! plugins, so the new config takes effect at once; if the plugins fail to
//! load with it, they keep running with the config they had.

use crate::db::plugins::{self as db, ConfigOverrides, PluginConfigError};
use crate::plugins::settings::{check_config, Setting};
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
    /// The defaults with the overrides applied: the config the plugin is
    /// loaded with.
    #[schema(value_type = Object)]
    pub config: Map<String, Value>,
    /// The config the plugin is running with. The same as `config`, unless
    /// the plugins failed to reload with it (see `reload_error`).
    #[schema(value_type = Object)]
    pub active: Map<String, Value>,
    /// Whether `config` differs from `active`: the plugin is not yet running
    /// with its config, because the plugins could not be reloaded with it.
    /// Fixing the config, or the plugin, applies it. Called `restart_required`
    /// before plugins reloaded while the server runs; that name is still
    /// accepted when reading a response.
    #[serde(alias = "restart_required")]
    pub reload_failed: bool,
    /// Why the plugins could not be reloaded with the new config, if they
    /// could not. The plugins that were running keep running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reload_error: Option<String>,
    /// Descriptions of the plugin's settings, from its manifest: the type of
    /// value each holds, and a label and description for it. Keys without
    /// one may hold any value.
    #[serde(default)]
    #[schema(value_type = Vec<Object>)]
    pub settings: Vec<Setting>,
}

impl PluginConfigResponse {
    fn new(plugin: &Plugin, overrides: ConfigOverrides, reload_error: Option<String>) -> Self {
        let config = apply_config_overrides(&plugin.manifest.config, overrides.clone());
        PluginConfigResponse {
            defaults: plugin.manifest.config.clone(),
            reload_failed: config != plugin.config,
            active: plugin.config.clone(),
            overrides,
            config,
            reload_error,
            settings: plugin.manifest.settings.clone(),
        }
    }
}

/// Checks the config values `values`, about to be saved as overrides of
/// plugin `name`, against the plugin's settings, answering `422
/// Unprocessable Entity` if one does not match.
///
/// A plugin that is not loaded is let through, for [`with_plugin`] to
/// answer.
fn check_overrides(
    state: &AppState,
    name: &str,
    values: &Map<String, Value>,
) -> Result<(), Response> {
    let discovery = state.plugins.current();
    let Some(plugin) = discovery.plugins.iter().find(|p| p.manifest.name == name) else {
        return Ok(());
    };
    check_config(&plugin.manifest.settings, values).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("Invalid config: {e}"),
        )
            .into_response()
    })
}

/// Whether a route changes the overrides, and so must reload the plugins.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Write,
}

/// Runs `op` on a database connection on the blocking thread pool, for the
/// plugin named `name`, and answers with the plugin's config and the
/// overrides `op` returns. When `access` is [`Access::Write`], the plugins
/// are then reloaded, so that the plugin runs with its new config.
///
/// A plugin that is not loaded is answered with `404 Not Found`, and
/// overrides too large to save with `413 Payload Too Large`. Any other
/// failure is a `500 Internal Server Error`.
async fn with_plugin<F>(
    state: &AppState,
    name: String,
    access: Access,
    op: F,
) -> Result<Response, Response>
where
    F: FnOnce(&mut Connection, &str) -> Result<ConfigOverrides, PluginConfigError> + Send + 'static,
{
    let not_found = || (StatusCode::NOT_FOUND, "Plugin not found").into_response();
    if !state
        .plugins
        .current()
        .plugins
        .iter()
        .any(|p| p.manifest.name == name)
    {
        return Err(not_found());
    }

    let runtime = state.plugins.clone();
    let result = task::spawn_blocking(move || {
        // The connection is returned before the reload, which reads the
        // config overrides on a connection of its own.
        let overrides = match access {
            Access::Read => runtime.db().read_blocking(|conn| op(conn, &name)),
            Access::Write => runtime.db().write_blocking(|conn| op(conn, &name)),
        }?
        .map_err(anyhow::Error::from)?;
        let reload_error = match access {
            Access::Read => None,
            Access::Write => runtime.reload().err().map(|e| {
                event!(
                    Level::WARN,
                    "failed to reload plugins after changing the config of {name:?}; \
                     the previous plugins keep running: {e}"
                );
                e.to_string()
            }),
        };
        Ok::<_, anyhow::Error>((name, overrides, reload_error))
    })
    .await;

    match result {
        Ok(Ok((name, overrides, reload_error))) => {
            let discovery = state.plugins.current();
            let plugin = discovery
                .plugins
                .iter()
                .find(|p| p.manifest.name == name)
                .ok_or_else(not_found)?;
            Ok(Json(PluginConfigResponse::new(plugin, overrides, reload_error)).into_response())
        }
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
/// those add up to, which the plugin is loaded with; and the config it is running with
/// now.
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
    with_plugin(&state, name, Access::Read, |conn, name| {
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
/// Keys described by the plugin's settings must hold values of the setting's type, or be
/// `null`.
///
/// The plugins are reloaded, so that the new config takes effect at once. If they fail to
/// load with it, the response says why in `reload_error`, and they keep running with the
/// config they had.
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
        (status = 422, description = "The request body is not a JSON object, or a value does not match its setting"),
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
    check_overrides(&state, &name, &overrides)?;
    with_plugin(&state, name, Access::Write, move |conn, name| {
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
/// Keys described by the plugin's settings must hold values of the setting's type, or be
/// `null`.
///
/// The plugins are reloaded, so that the new config takes effect at once. If they fail to
/// load with it, the response says why in `reload_error`, and they keep running with the
/// config they had.
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
        (status = 422, description = "The request body is not a JSON object, or a value does not match its setting"),
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
    check_overrides(&state, &name, &changes)?;
    with_plugin(&state, name, Access::Write, move |conn, name| {
        db::update_config_overrides(conn, name, |overrides| overrides.extend(changes))
    })
    .await
}

/// Remove plugin config overrides
///
/// Remove every one of a plugin's config overrides, restoring every key to the default
/// from the plugin's manifest.
///
/// The plugins are reloaded, so that the new config takes effect at once. If they fail to
/// load with it, the response says why in `reload_error`, and they keep running with the
/// config they had.
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
    with_plugin(&state, name, Access::Write, |conn, name| {
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
/// The plugins are reloaded, so that the new config takes effect at once. If they fail to
/// load with it, the response says why in `reload_error`, and they keep running with the
/// config they had.
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
    with_plugin(&state, name, Access::Write, move |conn, name| {
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
        assert!(!body.reload_failed);

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
        assert!(!body.reload_failed);

        // The plugin list shows the config in effect too.
        let resp = client
            .get("http://localhost/v1/plugins/name/hello")
            .send()
            .await?;
        let body: Value = resp.json().await?;
        assert_eq!(body["config"], json!({"a": 1, "b": 3}));

        Ok(())
    }

    /// A config the plugin fails to load with is saved, but the plugin keeps
    /// running with the config it had.
    #[tokio::test]
    async fn test_a_config_that_fails_to_load_keeps_the_old_one() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        tc.install_lua_plugin(
            "hello",
            "local config = ...\nif config.fail then error('bad config') end",
            json!({"a": 1}),
        )?;
        let tc = tc.init_server()?;
        let client = tc.client()?;

        let resp = client
            .patch(URL)
            .json(&json!({"fail": true}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.config), json!({"a": 1, "fail": true}));
        assert_eq!(Value::Object(body.active), json!({"a": 1}));
        assert!(body.reload_failed);
        assert!(
            body.reload_error
                .as_deref()
                .unwrap_or("")
                .contains("bad config"),
            "{:?}",
            body.reload_error
        );
        assert_eq!(stored(&tc)?, json!({"fail": true}));

        // Fixing the config applies it.
        let resp = client.put(URL).json(&json!({"a": 2})).send().await?;
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(Value::Object(body.active), json!({"a": 2}));
        assert!(!body.reload_failed);
        assert!(body.reload_error.is_none());

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
        // The plugins are reloaded, so the plugin runs with its new config.
        assert_eq!(
            Value::Object(body.active),
            json!({"a": 1, "b": 3, "c": [1, 2]})
        );
        assert!(!body.reload_failed);
        assert!(body.reload_error.is_none());
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

    /// Values of keys a plugin describes with a setting must match it.
    #[tokio::test]
    async fn test_overrides_must_match_settings() -> Result<()> {
        let tc = TestBuilder::default().init_database().build()?;
        let dir = tc.plugins_dir().join("hello");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("manifest.toml"),
            "name = 'hello'\nversion = '1.0.0'\nengine = 'lua'\n\
             [config]\nn = 1\n\
             [[settings]]\nname = 'n'\ntype = 'integer'\nmin = 0\n",
        )?;
        std::fs::write(dir.join("main.lua"), "")?;
        let tc = tc.init_server()?;
        let client = tc.client()?;

        let resp = client.get(URL).send().await?;
        let body = resp.json::<PluginConfigResponse>().await?;
        assert_eq!(body.settings.len(), 1);
        assert_eq!(body.settings[0].name, "n");

        for req in [client.put(URL), client.patch(URL)] {
            let resp = req.json(&json!({"n": -1})).send().await?;
            assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
            assert_eq!(resp.text().await?, "Invalid config: n: must be at least 0");
        }
        assert_eq!(stored(&tc)?, json!({}));

        let resp = client
            .patch(URL)
            .json(&json!({"n": 5, "other": "x"}))
            .send()
            .await?;
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = client.patch(URL).json(&json!({"n": null})).send().await?;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(stored(&tc)?, json!({"n": null, "other": "x"}));

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
        assert!(!body.reload_failed);

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
        assert!(!body.reload_failed);
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
