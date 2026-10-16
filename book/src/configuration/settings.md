# Settings

Settings live in `kiki.toml` in the [data directory](environment.md). The
file holds only the settings that differ from the defaults, so a default
that changes in a later release still reaches you for every setting you
left alone:

```toml
[feed_fetch]
default_fetch_interval_seconds = 3600   # fetch new feeds hourly

[asset_cache]
enabled = false                         # don't download entries' images
```

A running server applies changes to the file as it notices them. If an edit
leaves the file invalid, the server logs the problem and keeps its current
settings. Unknown keys are rejected, so a typo is reported rather than
silently ignored.

## Changing settings from the command line

`kiki config` reads and changes the settings in the data directory, whether
or not the server is running:

```bash
kiki config get                                  # every setting in effect
kiki config get feed_fetch                       # one section
kiki config get api.anonymous_access             # one setting: "full"
kiki config get --overrides                      # just what kiki.toml sets

kiki config set api.anonymous_access read-only
kiki config set http.allowed_hosts '["kiki.example.com"]'
kiki config unset api.anonymous_access           # back to the default
```

Values are TOML: `3600`, `false`, `'["a", "b"]'`. A value that is not valid
TOML is taken as a string, so `read-only` needs no quotes. `get` prints
values in TOML, and prints nothing for a setting that is unset.
`$KIKI_PROXY` and `$KIKI_NO_PROXY` are not applied to what it prints.

`kiki config set --file FILE` sets every setting in a TOML file laid out as
`kiki.toml` is (`-` reads standard input), and keeps the other overrides.
With `--replace`, it removes the overrides the file leaves out as well, so
`kiki.toml` ends up holding exactly that file's settings. That suits a
config kept in Ansible, NixOS or a container image, applied on each deploy:

```bash
kiki config set --replace --file /etc/kiki/settings.toml
```

Every change is checked before it is saved, and nothing is saved if it is
invalid. `set` and `unset` report on standard error whether they changed
anything, starting their report with `No change` when they did not, and
leave the file alone in that case.

Run them as the user Kiki runs as, such as
`sudo -u kiki env KIKI_HOME=/var/lib/kiki kiki config set ...`: they
refuse to change the settings in a data directory that belongs to another
user, since the server could then be left unable to read the file they save.

`kiki web` reads `[http]`, `[web_ui]` and `api.anonymous_access` only as it
starts, so restart it after changing those; everything else applies to a
running server as it notices the change.

## Changing settings over the API

The `feed_fetch` and `asset_cache` sections can also be changed over the
API, at `/v1/settings/feed-fetch` and `/v1/settings/asset-cache`. A `PUT`
changes only the fields it is given:

```bash
curl --unix-socket "$sock" --request PUT \
    --header 'Content-Type: application/json' \
    --data '{"enabled": false}' \
    http://localhost/v1/settings/asset-cache
```

The API and `kiki config` rewrite `kiki.toml` whenever they change it,
dropping its comments and formatting. Each save re-reads the file and
holds a lock on `.kiki.toml.lock` beside it while it does, so a change
saved through one is never lost to a save through the other at the same
moment. While the file is invalid, changes through either fail
(the API answers `409 Conflict`) rather than overwrite it.

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

Once the cache grows past `max_bytes`, the assets used least recently are
deleted until it is back under. Separately, when the server starts and
every six hours after, Kiki deletes the assets nothing uses any more: those
of entries that have been deleted, such as by the
[`retention`](../plugins/retention.md) plugin, and the favicons of feeds
that have been removed.

## `[retention]`

Retired. Deleting old entries is now up to the
[`retention`](../plugins/retention.md) plugin. A `max_age_days` left in
`kiki.toml` from an earlier release is moved into that plugin's config when
the server starts, and removed from the file.

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
in the [privacy plugin](../plugins/privacy.md).

## `[http]`

Shared by the web UI and the API. `kiki web` reads it as it starts; the API
applies changes to the next request.

| Key | Default | Description |
| --- | ------- | ----------- |
| `allowed_hosts` | `[]` | Hosts the web UI and the API answer to besides `localhost`, `127.0.0.1` and `::1`: exact names or addresses, `*.example.com` for every subdomain of `example.com`, or `*` for any host. Requests naming another host in their `Host` header are refused, so that a site cannot reach Kiki through DNS rebinding. Empty, the web UI answers only to the localhost names, and the API to any host. `allowed_hosts` under `[web_ui]` or `[api]` replaces this list for one of them. See [The web UI](../web-ui.md#reaching-it-by-another-name) and [Exposing Kiki over the network](../deployment.md#exposing-kiki-over-the-network). |

## `[web_ui]`

Read by `kiki web` as it starts, so restart it after a change.

| Key | Default | Description |
| --- | ------- | ----------- |
| `allowed_hosts` | unset | Hosts the web UI answers to besides `localhost`, `127.0.0.1` and `::1`, in place of `http.allowed_hosts`. Unset, the web UI uses the `[http]` list. See [The web UI](../web-ui.md#reaching-it-by-another-name). |
| `require_login` | `false` | Whether the web UI asks for an [API token](../tokens.md) before showing anything, as `kiki web --require-login` does. See [Logging in](../web-ui.md#logging-in). |

## `[api]`

Changes apply to the next request, with no restart.

| Key | Default | Description |
| --- | ------- | ----------- |
| `anonymous_access` | `"full"` | What a request without an [API token](../tokens.md) may do: `"full"` (anything, administration included), `"read-only"` (as with a token holding only the `read` scope), or `"token-required"` (nothing but `GET /v1/health`, `GET /v1/`, `GET /v1/access`, `GET /v1/tokens/current` and `/docs`). Requests with a token are held to its scopes either way. `GET /v1/access` reports the setting to anyone. See [Anonymous access](../tokens.md#anonymous-access). |
| `allowed_hosts` | unset | Hosts the API answers to besides `localhost`, `127.0.0.1` and `::1`, in place of `http.allowed_hosts`. Unset, the API uses the `[http]` list. An empty list, set here or inherited, turns the check off, and the API answers any host. Otherwise a request naming another host in its `Host` header is refused with `400 Bad Request`. See [Exposing Kiki over the network](../deployment.md#exposing-kiki-over-the-network). |

## `[plugins]`

Changes apply to the next call a plugin makes, with no restart.

| Key | Default | Description |
| --- | ------- | ----------- |
| `allow_network` | `false` | Whether plugins whose manifest asks for the [`network` permission](../writing-plugins.md#permissions) may fetch web pages, as the [`full-content`](../plugins/full-content.md) plugin does. The pages are fetched by the feed fetcher, through the `[proxy]`, if one is set. Off, their fetches fail. |

## Per-feed settings

Each feed also has a fetch interval (`min_fetch_interval_seconds`) and, for
feeds behind HTTP authentication, credentials, changed with a `PUT` to
`/v1/feeds/id/{id}`; see the [API reference](../../api/). New feeds take
their interval from `feed_fetch.default_fetch_interval_seconds`. Backing off
from feeds that rarely change is up to the
[`adaptive-fetch`](../plugins/adaptive-fetch.md) plugin.
