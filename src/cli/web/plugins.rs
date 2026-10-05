use super::api::{encode_path_segment, fetch_optional, fetch_plugins, plugin_api_url};
use super::feeds::safe_link;
use super::layout::{render_form_page, render_page, server_unavailable, to_json, to_json_pretty};
use super::login::Api;
use super::settings;
use crate::plugins::settings::{Setting, SettingType};
use crate::routes::v1::plugins::list_plugins::{ListPluginsResponse, PluginResponse};
use crate::routes::v1::plugins::plugin_config::PluginConfigResponse;
use axum::{
    extract::{Form, Path as UrlPath},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use quick_xml::escape::escape;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

/// Render the list of installed plugins, and of the directories in the
/// plugins directory that could not be loaded as plugins.
pub(super) async fn plugins_page(api: Api) -> Response {
    match fetch_plugins(&api).await {
        Ok(plugins) => render_page(StatusCode::OK, "Plugins - Kiki", &render_plugins(&plugins)),
        Err(e) => server_unavailable(&e),
    }
}

/// Render the page for plugin `name`: what its manifest says about it, and
/// its config, with a form to change each setting.
pub(super) async fn plugin_page(api: Api, UrlPath(name): UrlPath<String>) -> Response {
    render_plugin_config_page(&api, &name, StatusCode::OK, None).await
}

/// What a form on a plugin's page asks to do to its config.
#[derive(Clone, Copy)]
pub(super) enum ConfigAction {
    /// Override setting `key` with the value of the form's fields for it;
    /// see [`settings::parse_input`].
    Save,
    /// Override setting `key` with `value`, written as JSON.
    Set,
    /// Remove the override of setting `key`, restoring its default.
    Reset,
    /// Remove every override, restoring every setting to its default.
    ResetAll,
}

impl ConfigAction {
    /// The action named `name` by a form's `action` field.
    pub(super) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "save" => Self::Save,
            "set" => Self::Set,
            "reset" => Self::Reset,
            "reset_all" => Self::ResetAll,
            _ => return None,
        })
    }
}

/// Change plugin `name`'s config as a form on its page asks, then send the
/// browser back to the page.
///
/// The form's `action` field says what to do (see [`ConfigAction`]) and its
/// `key` field names the setting to change. A value that does not match its
/// setting or is not valid JSON, or a config too large for the server to
/// save, is reported on the plugin's page, and nothing is changed. Forms
/// submitted from other sites are refused with `403 Forbidden`; see
/// [`is_same_origin`].
pub(super) async fn update_plugin_config(
    api: Api,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    Form(pairs): Form<Vec<(String, String)>>,
) -> Response {
    if !is_same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            "Forms may only be submitted from the web UI's own pages.",
        )
            .into_response();
    }

    let form = settings::FormValues::new(pairs);
    let key = form.first("key").unwrap_or_default().to_owned();
    let Some(action) = form.first("action").and_then(ConfigAction::from_name) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "Unknown form action.").into_response();
    };
    let invalid = |error: String| {
        let api = api.clone();
        let name = name.clone();
        async move {
            render_plugin_config_page(&api, &name, StatusCode::UNPROCESSABLE_ENTITY, Some(&error))
                .await
        }
    };
    if key.is_empty() && !matches!(action, ConfigAction::ResetAll) {
        return invalid("Give the setting a name.".into()).await;
    }

    let config_url = plugin_api_url(&name, &["config"]);
    let req = match action {
        ConfigAction::Save => {
            let config =
                match fetch_optional::<PluginConfigResponse>(&api, config_url.clone()).await {
                    Ok(Some(config)) => config,
                    Ok(None) => return plugin_not_found(),
                    Err(e) => return server_unavailable(&e),
                };
            let setting = setting_for(&config, &key);
            let value = match settings::parse_input(&setting, &key, &form) {
                Ok(value) => value,
                Err(e) => {
                    return invalid(format!("{} was not saved: {e}.", setting.label())).await;
                }
            };
            let mut changes = Map::new();
            changes.insert(key, value);
            api.patch(config_url).json(&changes)
        }
        ConfigAction::Set => {
            let text = form.first("value").unwrap_or_default();
            let value: Value = match serde_json::from_str(text) {
                Ok(value) => value,
                Err(e) => {
                    return invalid(format!(
                        "The value for {key} is not valid JSON ({e}). Strings must be in \
                         double quotes."
                    ))
                    .await;
                }
            };
            let mut changes = Map::new();
            changes.insert(key, value);
            api.patch(config_url).json(&changes)
        }
        ConfigAction::Reset => api.delete(plugin_api_url(&name, &["config", &key])),
        ConfigAction::ResetAll => api.delete(config_url),
    };

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => return server_unavailable(&e.into()),
    };
    match resp.status() {
        StatusCode::OK => {
            let reload_error = resp
                .json::<PluginConfigResponse>()
                .await
                .ok()
                .and_then(|c| c.reload_error);
            match reload_error {
                None => Redirect::to(&format!("/plugins/{}", encode_path_segment(&name)))
                    .into_response(),
                Some(e) => {
                    let error = format!(
                        "The config was saved, but the plugins failed to load with it: {e}"
                    );
                    render_plugin_config_page(
                        &api,
                        &name,
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Some(&error),
                    )
                    .await
                }
            }
        }
        StatusCode::NOT_FOUND => plugin_not_found(),
        StatusCode::UNPROCESSABLE_ENTITY => {
            let error = resp.text().await.unwrap_or_default();
            let error = error.trim();
            invalid(if error.is_empty() {
                "The config was not saved.".to_owned()
            } else {
                format!("The config was not saved. {error}.")
            })
            .await
        }
        StatusCode::PAYLOAD_TOO_LARGE => {
            render_plugin_config_page(
                &api,
                &name,
                StatusCode::PAYLOAD_TOO_LARGE,
                Some("The config is too large to save."),
            )
            .await
        }
        status => {
            tracing::warn!(%status, plugin = name, "failed to update plugin config");
            render_plugin_config_page(
                &api,
                &name,
                StatusCode::BAD_GATEWAY,
                Some("The config could not be saved."),
            )
            .await
        }
    }
}

/// Whether a form or request was sent from one of the web UI's own pages,
/// going by the headers the browser sent with it, `headers`.
///
/// The web UI has no login unless asked for one, and even then its session
/// cookie is all a form needs, so any site open in the same browser could
/// otherwise submit a form to it and change a plugin's config, or save an
/// entry. Browsers say
/// where a form came from in `Sec-Fetch-Site` or, failing that, `Origin`;
/// a request with neither did not come from a browser that would submit a
/// form for another site, and is let through.
pub(super) fn is_same_origin(headers: &HeaderMap) -> bool {
    if let Some(site) = headers.get("sec-fetch-site") {
        return site == "same-origin";
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    origin
        .to_str()
        .ok()
        .and_then(|o| {
            o.strip_prefix("http://")
                .or_else(|| o.strip_prefix("https://"))
        })
        .is_some_and(|authority| Some(authority) == host)
}

/// Render the page for plugin `name` with `status`, showing `error` above
/// its config if there is one.
pub(super) async fn render_plugin_config_page(
    api: &Api,
    name: &str,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let (plugin, config) = tokio::join!(
        fetch_optional::<PluginResponse>(api, plugin_api_url(name, &[])),
        fetch_optional::<PluginConfigResponse>(api, plugin_api_url(name, &["config"])),
    );
    match (plugin, config) {
        (Ok(Some(plugin)), Ok(Some(config))) => render_form_page(
            status,
            &format!("{} - Plugins - Kiki", plugin.name),
            &render_plugin_page(&plugin, &config, error),
        ),
        (Err(e), _) | (_, Err(e)) => server_unavailable(&e),
        _ => plugin_not_found(),
    }
}

/// Render the page for a plugin that is not installed.
pub(super) fn plugin_not_found() -> Response {
    render_page(
        StatusCode::NOT_FOUND,
        "Plugin not found - Kiki",
        "<p>Plugin not found.</p>\n<p><a href=\"/plugins\">&larr; Back to plugins</a></p>\n",
    )
}

/// Render the plugin count, each plugin in `resp` with what its manifest
/// says about it, and the directories that could not be loaded as plugins.
pub(super) fn render_plugins(resp: &ListPluginsResponse) -> String {
    let mut html = format!(
        "<h2>Plugins</h2>\n<p class=\"count\">{} {}</p>\n",
        resp.count,
        if resp.count == 1 { "plugin" } else { "plugins" }
    );

    if resp.plugins.is_empty() {
        html.push_str("<p>No plugins are installed.</p>\n");
    } else {
        html.push_str("<ol class=\"plugins\">\n");
        for plugin in &resp.plugins {
            html.push_str("<li>");
            html.push_str(&render_plugin(plugin));
            html.push_str("</li>\n");
        }
        html.push_str("</ol>\n");
    }

    if !resp.errors.is_empty() {
        html.push_str("<h3>Could not be loaded</h3>\n<ol class=\"plugins\">\n");
        for error in &resp.errors {
            html.push_str(&format!(
                "<li><strong>{}</strong><span class=\"meta\">{}</span></li>\n",
                escape(&error.directory),
                escape(&error.error)
            ));
        }
        html.push_str("</ol>\n");
    }

    html.push_str(
        "<p class=\"meta\">Plugins are reloaded whenever the plugins directory or a \
         plugin's config changes.</p>\n\
         <p><a href=\"/settings\">&larr; Back to settings</a></p>\n",
    );
    html
}

/// Render a single plugin in the list: its name, linked to its page, and
/// version, then [`render_plugin_details`].
pub(super) fn render_plugin(plugin: &PluginResponse) -> String {
    format!(
        "<a href=\"/plugins/{}\"><strong>{}</strong></a> <span class=\"version\">v{}</span>{}",
        encode_path_segment(&plugin.name),
        escape(&plugin.name),
        escape(&plugin.version),
        render_plugin_details(plugin)
    )
}

/// Render a plugin's description, and a line with its engine, whether it is
/// a system or user plugin, whether it runs, its authors, license and
/// homepage.
pub(super) fn render_plugin_details(plugin: &PluginResponse) -> String {
    let mut html = String::new();
    if let Some(description) = plugin
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
    {
        html.push_str(&format!(
            "<p class=\"description\">{}</p>",
            escape(description)
        ));
    }

    let mut parts = vec![
        plugin.engine.name().to_owned(),
        format!("{} plugin", plugin.source.name()),
    ];
    parts.push(
        if !plugin.engine_supported {
            "engine not supported by this build"
        } else if plugin.enabled {
            "enabled"
        } else {
            "disabled"
        }
        .to_owned(),
    );
    if !plugin.authors.is_empty() {
        let authors: Vec<_> = plugin.authors.iter().map(escape).collect();
        parts.push(format!("by {}", authors.join(", ")));
    }
    if let Some(license) = &plugin.license {
        parts.push(escape(license).into_owned());
    }
    if let Some(url) = plugin.homepage.as_deref().and_then(safe_link) {
        parts.push(format!(
            "<a href=\"{}\" rel=\"noopener noreferrer\">Homepage</a>",
            escape(url)
        ));
    }
    html.push_str(&format!(
        "<span class=\"meta\">{}</span>",
        parts.join(" &middot; ")
    ));
    html
}

/// Render the page for `plugin`: its name, version and
/// [`render_plugin_details`], then its config, `config`, with a form for
/// each setting, a form to add one and a form to reset them all. `error`,
/// if there is one, is shown above the config.
///
/// The settings the plugin's manifest describes come first, in the order
/// it gives them, then the others, by name.
pub(super) fn render_plugin_page(
    plugin: &PluginResponse,
    config: &PluginConfigResponse,
    error: Option<&str>,
) -> String {
    let action = format!("/plugins/{}/config", encode_path_segment(&plugin.name));
    let mut html = format!(
        "<article class=\"plugin\">\n<h2>{} <small class=\"version\">v{}</small></h2>\n{}\n",
        escape(&plugin.name),
        escape(&plugin.version),
        render_plugin_details(plugin)
    );
    if let Some(error) = error {
        html.push_str(&format!("<p class=\"error\">{}</p>\n", escape(error)));
    }
    if config.reload_failed {
        html.push_str(
            "<p class=\"notice\">The plugin is not running with this config: the plugins \
             failed to load with it, so they keep running with the config they had. The \
             server log says why.</p>\n",
        );
    }

    html.push_str("<h3>Config</h3>\n");
    // Settings the plugin is running with but that are no longer set are
    // listed too, until the plugins reload without them.
    let described: Vec<&str> = config.settings.iter().map(|s| s.name.as_str()).collect();
    let others: BTreeSet<&str> = config
        .config
        .keys()
        .chain(config.active.keys())
        .map(String::as_str)
        .filter(|k| !described.contains(k))
        .collect();
    let keys: Vec<&str> = described.into_iter().chain(others).collect();
    if keys.is_empty() {
        html.push_str("<p>This plugin has no settings.</p>\n");
    }
    for (i, key) in keys.into_iter().enumerate() {
        html.push_str(&render_config_setting(&action, key, config, i));
    }

    let add_form = format!(
        "<form method=\"post\" action=\"{action}\" class=\"config-form\">\
         <input type=\"hidden\" name=\"action\" value=\"set\">\
         <input type=\"text\" name=\"key\" placeholder=\"name\" aria-label=\"Name\" required>\
         <input type=\"text\" name=\"value\" placeholder=\"value, as JSON\" aria-label=\"Value\" required>\
         <button type=\"submit\">Add</button></form>\n\
         <p class=\"meta\">Values are JSON: strings go in double quotes, as in \
         <code>\"hello\"</code>; numbers, <code>true</code>, <code>false</code>, \
         <code>null</code>, lists and objects are written as they are.</p>\n"
    );
    if config.settings.is_empty() {
        html.push_str(&format!("<h3>Add a setting</h3>\n{add_form}"));
    } else {
        // Plugins that describe their settings rarely need others.
        html.push_str(&format!(
            "<details class=\"add-setting\"><summary>Add a setting the plugin does not \
             describe</summary>\n{add_form}</details>\n"
        ));
    }
    if !config.overrides.is_empty() {
        html.push_str(&format!(
            "<form method=\"post\" action=\"{action}\" class=\"config-form\">\
             <button type=\"submit\" name=\"action\" value=\"reset_all\">\
             Reset every setting to its default</button></form>\n"
        ));
    }
    html.push_str("<p class=\"meta\">Changes take effect at once.</p>\n");
    html.push_str("</article>\n<p><a href=\"/plugins\">&larr; Back to plugins</a></p>\n");
    html
}

/// The setting that describes config key `key` of `config`: the one the
/// plugin's manifest gives, or else one guessed from the key's default, or
/// its value if it has no default. See [`guess_setting_type`].
pub(super) fn setting_for(config: &PluginConfigResponse, key: &str) -> Setting {
    if let Some(setting) = config.settings.iter().find(|s| s.name == key) {
        return setting.clone();
    }
    let example = config
        .defaults
        .get(key)
        .or_else(|| config.config.get(key))
        .or_else(|| config.active.get(key));
    Setting {
        name: key.to_owned(),
        label: None,
        description: None,
        required: false,
        kind: example.map(guess_setting_type).unwrap_or(SettingType::Json),
    }
}

/// The type of setting that `example`, a value of it, suggests: booleans,
/// numbers and strings are edited as such, lists of strings or of integers
/// one item per line, and anything else as JSON.
pub(super) fn guess_setting_type(example: &Value) -> SettingType {
    let list_of = |items: SettingType| SettingType::List {
        items: Box::new(items),
    };
    match example {
        Value::Bool(_) => SettingType::Boolean,
        Value::Number(n) if n.is_i64() => SettingType::Integer {
            min: None,
            max: None,
        },
        Value::Number(_) => SettingType::Number {
            min: None,
            max: None,
        },
        Value::String(s) => SettingType::String {
            multiline: s.contains('\n'),
        },
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_string) => {
            list_of(SettingType::String { multiline: false })
        }
        Value::Array(items) if !items.is_empty() && items.iter().all(Value::is_i64) => {
            list_of(SettingType::Integer {
                min: None,
                max: None,
            })
        }
        _ => SettingType::Json,
    }
}

/// Render the section of the plugin page for setting `key` of `config`,
/// the `index`th on the page: its label and description, a form, submitted
/// to `action`, to change its value or restore its default, and where its
/// value comes from.
///
/// The form has fields that fit the setting (see [`setting_for`]), with a
/// second form to edit the value as JSON; a value that does not fit them is
/// only shown as JSON.
pub(super) fn render_config_setting(
    action: &str,
    key: &str,
    config: &PluginConfigResponse,
    index: usize,
) -> String {
    let setting = setting_for(config, key);
    let default = config.defaults.get(key);
    let overridden = config.overrides.contains_key(key);
    let value = config.config.get(key);
    let active = config.active.get(key);
    let shown = value.or(active);
    let key_html = escape(key);
    let id = format!("setting-{index}");

    let mut html = format!("<section class=\"setting\" id=\"{id}\">\n<h4>");
    if setting.label() != key {
        html.push_str(&format!("{} ", escape(setting.label())));
    }
    html.push_str(&format!("<code>{key_html}</code></h4>\n"));
    if let Some(description) = setting.description.as_deref() {
        html.push_str(&format!(
            "<p class=\"description\">{}</p>\n",
            escape(description)
        ));
    }

    let reset = match (overridden, default.is_some()) {
        (true, true) => {
            "<button type=\"submit\" name=\"action\" value=\"reset\">Reset to default</button>"
        }
        (true, false) => "<button type=\"submit\" name=\"action\" value=\"reset\">Remove</button>",
        (false, _) => "",
    };
    let form = |fields: &str, save: &str| {
        format!(
            "<form method=\"post\" action=\"{action}\" class=\"setting-form\">\
             <input type=\"hidden\" name=\"key\" value=\"{key_html}\">{fields}\
             <div class=\"buttons\"><button type=\"submit\" name=\"action\" value=\"{save}\">\
             Save</button>{reset}</div></form>\n"
        )
    };

    // Lists and objects get room to be written out over several lines.
    let json_field = if shown.is_some_and(|v| v.is_array() || v.is_object()) {
        format!(
            "<textarea name=\"value\" rows=\"4\" class=\"json\" aria-label=\"Value of {key_html}, as JSON\">{}</textarea>",
            escape(shown.map(to_json_pretty).unwrap_or_default())
        )
    } else {
        format!(
            "<input type=\"text\" name=\"value\" value=\"{}\" class=\"json\" aria-label=\"Value of {key_html}, as JSON\">",
            escape(shown.map(to_json).unwrap_or_default())
        )
    };
    let typed = match setting.kind {
        SettingType::Json => None,
        _ => settings::render_input(&setting, shown, &id),
    };
    let mut notes = Vec::new();
    match typed {
        Some(fields) => {
            html.push_str(&form(&fields, "save"));
            html.push_str(&format!(
                "<details class=\"as-json\"><summary>Edit as JSON</summary>\n{}</details>\n",
                form(&json_field, "set")
            ));
        }
        None => {
            html.push_str(&form(&json_field, "set"));
            if !matches!(setting.kind, SettingType::Json) {
                notes.push(
                    "shown as JSON, since the value does not fit the setting's fields".to_owned(),
                );
            }
        }
    }

    notes.insert(
        0,
        match (overridden, default) {
            (true, Some(default)) => {
                let default = to_json(default);
                if default.len() <= 80 {
                    format!("overrides the default, <code>{}</code>", escape(default))
                } else {
                    "overrides the default".to_owned()
                }
            }
            (true, None) => "set here; the manifest has no default".to_owned(),
            (false, Some(_)) => "default".to_owned(),
            (false, None) => "no longer set".to_owned(),
        },
    );
    if value.is_some_and(Value::is_null) {
        notes.push("set to <code>null</code>, hiding the default".to_owned());
    }
    if value != active {
        notes.push(match active {
            Some(active) => format!(
                "running with <code>{}</code>, since the plugins failed to load with this value",
                escape(to_json(active))
            ),
            None => "not set in the config the plugin is running with".to_owned(),
        });
    }
    html.push_str(&format!(
        "<span class=\"meta\">{}</span>\n</section>\n",
        notes.join(" &middot; ")
    ));
    html
}
