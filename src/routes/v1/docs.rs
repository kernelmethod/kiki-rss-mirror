use crate::auth::policy::{requirement, Requirement};
use crate::auth::Scope;
use axum::http::Method;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityRequirement, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// OpenAPI documentation for the kiki-rss API.
#[derive(OpenApi)]
#[openapi(
    paths(
        crate::routes::v1::root::root,
        crate::routes::v1::health::health,
        crate::routes::v1::access::access,
        crate::routes::v1::feeds::add_feed::add_feed,
        crate::routes::v1::feeds::list_feeds::list_feeds,
        crate::routes::v1::feeds::get_feed::get_feed,
        crate::routes::v1::feeds::update_feed::update_feed,
        crate::routes::v1::feeds::delete_feed::delete_feed,
        crate::routes::v1::feeds::fetch_feed::fetch_feed,
        crate::routes::v1::feeds::fetch_all_feeds::fetch_all_feeds,
        crate::routes::v1::feeds::feed_entries::feed_entries,
        crate::routes::v1::feeds::feed_favicon::feed_favicon,
        crate::routes::v1::feeds::export_opml::export_opml,
        crate::routes::v1::feeds::import_opml::import_opml,
        crate::routes::v1::feeds::feed_tags::get_feed_tags,
        crate::routes::v1::feeds::feed_tags::set_feed_tags,
        crate::routes::v1::entries::list_entries::list_entries,
        crate::routes::v1::entries::get_entry::get_entry,
        crate::routes::v1::entries::delete_entry::delete_entry,
        crate::routes::v1::entries::entry_tags::get_entry_tags,
        crate::routes::v1::entries::entry_tags::set_entry_tags,
        crate::routes::v1::entries::entry_tags::add_entry_system_tag,
        crate::routes::v1::entries::entry_tags::remove_entry_system_tag,
        crate::routes::v1::entries::search_entries::search_entries,
        crate::routes::v1::entries::search_entries::search_entry_ids,
        crate::routes::v1::entries::batch_entries::batch_entries,
        crate::routes::v1::tags::list_tags::list_tags,
        crate::routes::v1::tags::create_tag::create_tag,
        crate::routes::v1::tags::get_tag::get_tag,
        crate::routes::v1::tags::update_tag::update_tag,
        crate::routes::v1::tags::delete_tag::delete_tag,
        crate::routes::v1::tags::tag_feeds::tag_feeds,
        crate::routes::v1::tags::tag_entries::tag_entries,
        crate::routes::v1::tags::tag_entries::add_tag_entries,
        crate::routes::v1::tags::tag_entries::remove_tag_entries,
        crate::routes::v1::plugins::list_plugins::list_plugins,
        crate::routes::v1::plugins::get_plugin::get_plugin,
        crate::routes::v1::plugins::plugin_config::get_plugin_config,
        crate::routes::v1::plugins::plugin_config::put_plugin_config,
        crate::routes::v1::plugins::plugin_config::patch_plugin_config,
        crate::routes::v1::plugins::plugin_config::delete_plugin_config,
        crate::routes::v1::plugins::plugin_config::delete_plugin_config_key,
        crate::routes::v1::settings::retention::get_retention,
        crate::routes::v1::settings::retention::put_retention,
        crate::routes::v1::settings::feed_fetch::get_feed_fetch_settings,
        crate::routes::v1::settings::feed_fetch::put_feed_fetch_settings,
        crate::routes::v1::entries::cleanup::cleanup,
        crate::routes::v1::shutdown::shutdown,
        crate::routes::v1::assets::get_asset,
        crate::routes::v1::assets::get_asset_by_url,
        crate::routes::v1::assets::delete_asset,
        crate::routes::v1::entries::entry_assets::list_entry_assets,
        crate::routes::v1::settings::assets::get_asset_cache_settings,
        crate::routes::v1::settings::assets::put_asset_cache_settings,
        crate::routes::v1::tokens::list_tokens,
        crate::routes::v1::tokens::create_token,
        crate::routes::v1::tokens::revoke_token,
        crate::routes::v1::tokens::current_token,
    ),
    components(
        schemas(
            crate::routes::v1::root::RootResponse,
            crate::routes::v1::health::HealthResponse,
            crate::routes::v1::access::AccessResponse,
            crate::config::AnonymousAccess,
            crate::server::ComponentState,
            crate::routes::v1::feeds::add_feed::AddFeedRequest,
            crate::routes::v1::feeds::add_feed::AddFeedResponse,
            crate::routes::v1::feeds::list_feeds::ListFeedsResponse,
            crate::routes::v1::feeds::get_feed::GetFeedResponse,
            crate::routes::v1::feeds::get_feed::GetFeedDetailResponse,
            crate::routes::v1::feeds::format_data::RssFeedData,
            crate::routes::v1::feeds::format_data::AtomFeedData,
            crate::routes::v1::feeds::format_data::AtomGenerator,
            crate::tasks::FetchError,
            crate::routes::v1::feeds::update_feed::UpdateFeedRequest,
            crate::routes::v1::feeds::update_feed::UpdateFeedResponse,
            crate::routes::v1::feeds::fetch_all_feeds::FetchAllFeedsResponse,
            crate::routes::v1::feeds::feed_entries::FeedEntriesResponse,
            crate::routes::v1::feeds::feed_tags::SetFeedTagsRequest,
            crate::routes::v1::feeds::feed_tags::GetFeedTagsResponse,
            crate::routes::v1::feeds::import_opml::ImportOpmlResponse,
            crate::routes::v1::entries::list_entries::ListEntriesResponseEntry,
            crate::routes::v1::entries::list_entries::ListEntriesResponse,
            crate::routes::v1::entries::get_entry::GetEntryResponse,
            crate::routes::v1::entries::format_data::RssEntryData,
            crate::routes::v1::entries::format_data::RssCategory,
            crate::routes::v1::entries::format_data::AtomEntryData,
            crate::routes::v1::entries::format_data::AtomCategory,
            crate::routes::v1::entries::entry_tags::SetEntryTagsRequest,
            crate::routes::v1::entries::entry_tags::GetEntryTagsResponse,
            crate::routes::v1::entries::search_entries::SearchEntriesRequest,
            crate::routes::v1::entries::search_entries::SearchEntriesResponse,
            crate::routes::v1::entries::search_entries::SearchEntryIdsResponse,
            crate::routes::v1::entries::batch_entries::BatchEntriesRequest,
            crate::routes::v1::entries::batch_entries::BatchEntriesResponse,
            crate::routes::v1::entries::rows::EntrySort,
            crate::routes::v1::entries::search_entries::TagFilter,
            crate::routes::v1::entries::search_entries::TagExpr,
            crate::db::tags::TagKind,
            crate::routes::v1::tags::list_tags::TagResponse,
            crate::routes::v1::tags::list_tags::ListTagsResponse,
            crate::routes::v1::tags::create_tag::CreateTagRequest,
            crate::routes::v1::tags::create_tag::CreateTagResponse,
            crate::routes::v1::tags::get_tag::GetTagResponse,
            crate::routes::v1::tags::update_tag::UpdateTagRequest,
            crate::routes::v1::tags::update_tag::UpdateTagResponse,
            crate::routes::v1::tags::tag_feeds::TagFeedsResponse,
            crate::routes::v1::tags::tag_entries::TagEntriesResponse,
            crate::routes::v1::tags::tag_entries::AddTagEntriesRequest,
            crate::routes::v1::tags::tag_entries::AddTagEntriesResponse,
            crate::routes::v1::tags::tag_entries::RemoveTagEntriesResponse,
            crate::plugins::PluginEngine,
            crate::routes::v1::plugins::list_plugins::PluginResponse,
            crate::routes::v1::plugins::list_plugins::PluginErrorResponse,
            crate::routes::v1::plugins::list_plugins::ListPluginsResponse,
            crate::routes::v1::plugins::plugin_config::PluginConfigResponse,
            crate::routes::v1::settings::retention::RetentionResponse,
            crate::routes::v1::settings::retention::RetentionRequest,
            crate::routes::v1::settings::feed_fetch::FeedFetchSettingsResponse,
            crate::routes::v1::settings::feed_fetch::FeedFetchSettingsRequest,
            crate::routes::v1::entries::cleanup::CleanupResponse,
            crate::auth::Scopes,
            crate::routes::v1::entries::entry_assets::EntryAsset,
            crate::routes::v1::entries::entry_assets::ListEntryAssetsResponse,
            crate::routes::v1::settings::assets::AssetCacheSettingsResponse,
            crate::routes::v1::settings::assets::AssetCacheSettingsRequest,
            crate::db::tokens::Token,
            crate::routes::v1::tokens::ListTokensResponse,
            crate::routes::v1::tokens::CreateTokenRequest,
            crate::routes::v1::tokens::CreateTokenResponse,
            crate::routes::v1::tokens::CurrentTokenResponse,
        )
    ),
    tags(
        (name = "meta", description = "Server status and version information"),
        (name = "feeds", description = "Manage RSS/Atom feed subscriptions"),
        (name = "entries", description = "Access and manage feed entries"),
        (name = "tags", description = "Organize feeds and entries with tags"),
        (name = "plugins", description = "Inspect installed plugins and configure them"),
        (name = "settings", description = "Global configuration settings"),
        (name = "assets", description = "Images and enclosures cached from entries"),
        (name = "tokens", description = "Manage the API tokens that grant access to the API"),
    ),
    modifiers(&TokenSecurity),
    info(
        title = "kiki-rss",
        description = "A self-hosted RSS/Atom feed aggregator API",
        version = env!("CARGO_PKG_VERSION"),
    )
)]
pub struct ApiDoc;

/// Documents the API token each operation requires, from
/// [`crate::auth::policy`]: as a bearer security requirement naming the
/// scope, and in the operation's description.
struct TokenSecurity;

impl Modify for TokenSecurity {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "token",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "An API token, created with `kiki token create`. A request with a token \
                         may do only what the token's scopes allow. What a request without one \
                         may do is set by the `api.anonymous_access` setting, which \
                         `GET /v1/access` reports: by default, anything.",
                    ))
                    .build(),
            ),
        );

        for (path, item) in openapi.paths.paths.iter_mut() {
            for (method, op) in [
                (Method::GET, &mut item.get),
                (Method::PUT, &mut item.put),
                (Method::POST, &mut item.post),
                (Method::DELETE, &mut item.delete),
                (Method::PATCH, &mut item.patch),
            ] {
                let Some(op) = op else { continue };
                let (scopes, note) =
                    match requirement(&method, path).unwrap_or(Requirement::Scope(Scope::Admin)) {
                        Requirement::Any => continue,
                        Requirement::Scope(scope) => (
                            vec![scope.name()],
                            format!(
                                "A request with an API token needs the `{scope}` scope; one \
                                 without needs `api.anonymous_access` to grant it."
                            ),
                        ),
                    };
                op.security = Some(vec![SecurityRequirement::new("token", scopes)]);
                op.description = Some(match op.description.take() {
                    Some(d) if !d.is_empty() => format!("{d}\n\n{note}"),
                    _ => note,
                });
            }
        }
    }
}
