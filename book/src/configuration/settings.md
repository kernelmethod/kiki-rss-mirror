# Settings

Settings live in `kiki.toml` in the [data directory](environment.md). The
file holds only the settings that differ from the defaults, so a default
that changes in a later release still reaches you for every setting you
left alone:

```toml
[feed_fetch]
default_fetch_interval_seconds = 3600   # fetch new feeds hourly

[retention]
max_age_days = 30                       # forget entries 30 days after they leave their feed
```

A running server applies changes to the file as it notices them. If an edit
leaves the file invalid, the server logs the problem and keeps its current
settings. Unknown keys are rejected, so a typo is reported rather than
silently ignored.

The `feed_fetch`, `asset_cache` and `retention` sections can also be changed
over the API, at `/v1/settings/feed-fetch`, `/v1/settings/asset-cache` and
`/v1/settings/retention`. A `PUT` changes only the fields it is given:

```bash
curl --unix-socket "$sock" --request PUT \
    --header 'Content-Type: application/json' \
    --data '{"max_age_days": 30}' \
    http://localhost/v1/settings/retention
```

The API rewrites `kiki.toml` on every change, dropping its comments and
formatting. While the file is invalid, settings updates through the API fail
with `409 Conflict` rather than overwrite it.

<!-- Keep in sync with `Settings` and `Settings::default` in src/config/mod.rs. -->

## `[feed_fetch]`

How and how often feeds are fetched. Changes apply to the next fetch of
every feed, with no restart.

| Key | Default | Description |
| --- | ------- | ----------- |
| `timeout_seconds` | `15` | HTTP request timeout for a feed fetch, in seconds. |
| `min_polling_cadence_seconds` | `60` | The shortest time between two fetches of the same feed, in seconds, whatever the feed's server asks for. Stops a misbehaving server from making Kiki poll it constantly. |
| `default_fetch_interval_seconds` | `86400` (1 day) | The fetch interval given to newly added feeds: the longest Kiki waits between fetches, and the wait used when the server gives no hint of its own. Feeds already added keep their own interval, which can be changed per feed. |
| `max_backoff_seconds` | `86400` (1 day) | The longest Kiki backs off from a feed after errors, in seconds, and the wait after a permanent error. |
| `force_refresh_after_seconds` | `604800` (1 week) | How often to fetch a feed in full, ignoring `ETag` and `Last-Modified`, in seconds. Catches servers that keep sending the same headers after the feed has changed. |
| `max_feed_bytes` | `33554432` (32 MiB) | The largest feed Kiki reads, in bytes. Bigger responses are abandoned and recorded as an error. |

## `[asset_cache]`

The on-disk cache of entries' images and enclosures.

| Key | Default | Description |
| --- | ------- | ----------- |
| `enabled` | `true` | Whether to download and cache entries' assets at all. |
| `max_bytes` | `1073741824` (1 GiB) | The size, in bytes, the cache is trimmed down to. |

## `[retention]`

When old entries are deleted.

| Key | Default | Description |
| --- | ------- | ----------- |
| `max_age_days` | unset (keep forever) | Delete entries once their feed has stopped listing them for more than this many days. Entries still in their feed, and entries tagged `system:saved`, are never deleted. Between 1 and 36500 (100 years). |

## `[proxy]`

The HTTP(S) or SOCKS5 proxy for feed fetches and asset downloads.
`$KIKI_PROXY` and `$KIKI_NO_PROXY` override `url` and `no_proxy`; an invalid
`$KIKI_PROXY` stops `kiki serve` at startup. With no proxy URL set either
way, Kiki honours the conventional `$HTTPS_PROXY`, `$HTTP_PROXY`,
`$ALL_PROXY` and `$NO_PROXY` instead.

| Key | Default | Description |
| --- | ------- | ----------- |
| `url` | unset | URL of the proxy, optionally with `user:password@`. The scheme picks the kind: `http://` or `https://` for an HTTP proxy, `socks5h://` for SOCKS5 with host names looked up by the proxy, and `socks5://` for SOCKS5 with host names looked up locally. |
| `no_proxy` | unset | Comma-separated hosts to reach directly: domains (with their subdomains), IP addresses, CIDR ranges, or `*`. Has no effect without `url`. |

To use Tor, set `url = "socks5h://127.0.0.1:9050"`. With `socks5://`, Kiki
looks host names up itself, and those DNS queries go out directly, telling
whoever can see them which sites Kiki fetches from. To keep Kiki from
downloading images and enclosures at all for some feeds, see `skip_assets`
in the [strip-tracking plugin](../plugins/strip-tracking.md).

## `[web_ui]`

Read by `kiki web` as it starts, so restart it after a change.

| Key | Default | Description |
| --- | ------- | ----------- |
| `allowed_hosts` | `[]` | Hosts the web UI answers to besides `localhost`, `127.0.0.1` and `::1`: exact names or addresses, `*.example.com` for every subdomain of `example.com`, or `*` for any host. See [The web UI](../web-ui.md). |
| `require_login` | `false` | Whether the web UI asks for an [API token](../tokens.md) before showing anything, as `kiki web --require-login` does. See [Logging in](../web-ui.md#logging-in). |

## `[api]`

Changes apply to the next request, with no restart.

| Key | Default | Description |
| --- | ------- | ----------- |
| `anonymous_access` | `"full"` | What a request without an [API token](../tokens.md) may do: `"full"` (anything, administration included), `"read-only"` (as with a token holding only the `read` scope), or `"token-required"` (nothing but `GET /v1/health`, `GET /v1/`, `GET /v1/access`, `GET /v1/tokens/current` and `/docs`). Requests with a token are held to its scopes either way. `GET /v1/access` reports the setting to anyone. See [Anonymous access](../tokens.md#anonymous-access). |

## Per-feed settings

Each feed also has a fetch interval (`min_fetch_interval_seconds`) and, for
feeds behind HTTP authentication, credentials, changed with a `PUT` to
`/v1/feeds/id/{id}`; see the [API reference](../../api/). New feeds take
their interval from `feed_fetch.default_fetch_interval_seconds`. Backing off
from feeds that rarely change is up to the
[`adaptive-fetch`](../plugins/adaptive-fetch.md) plugin.
