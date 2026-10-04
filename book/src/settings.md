# Settings reference

Every setting `kiki.toml` accepts, with its default. See
[Configuration](configuration.md) for how the file works. Unknown keys are
rejected, so a typo is reported rather than silently ignored.

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
| `adaptive_fetch` | `true` | Back off from feeds whose server asks to be fetched more often than their interval, but which keep turning out to be unchanged. Each unchanged fetch doubles the wait and each changed one halves it, staying between `min_polling_cadence_seconds` and the feed's own interval. Feeds can override this. |
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

The proxy for feed fetches and asset downloads. Each key can be overridden
from the environment; see [Proxies](proxy.md).

| Key | Default | Description |
| --- | ------- | ----------- |
| `url` | unset | URL of the proxy, optionally with `user:password@`. The scheme picks the kind: `http://` or `https://` for an HTTP proxy, `socks5h://` for SOCKS5 with host names looked up by the proxy, and `socks5://` for SOCKS5 with host names looked up locally. |
| `no_proxy` | unset | Comma-separated hosts to reach directly: domains (with their subdomains), IP addresses, CIDR ranges, or `*`. Has no effect without `url`. |

## `[web_ui]`

Read by `kiki web` as it starts, so restart it after a change.

| Key | Default | Description |
| --- | ------- | ----------- |
| `allowed_hosts` | `[]` | Hosts the web UI answers to besides `localhost`, `127.0.0.1` and `::1`: exact names or addresses, `*.example.com` for every subdomain of `example.com`, or `*` for any host. See [The web UI](web-ui.md). |
