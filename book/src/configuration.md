# Configuration

Kiki has built-in defaults for every setting, so it needs no configuration
to run. To change a setting, either use the settings API or edit
`kiki.toml`, which lives beside the database (see
[Where Kiki keeps its files](files.md)).

## The config file

`kiki.toml` holds only the settings that differ from the defaults, grouped
into the same sections as the [settings reference](settings.md):

```toml
[feed_fetch]
default_fetch_interval_seconds = 3600   # fetch new feeds hourly

[asset_cache]
max_bytes = 268435456                   # cache at most 256 MiB of images

[retention]
max_age_days = 30                       # forget entries 30 days after they leave their feed
```

Because the file lists only what you changed, a default that changes in a
later release still reaches you for every setting you left alone.

A running server notices when the file changes and applies the new
settings. If an edit leaves the file invalid, the server logs the problem and
keeps its current settings.

## The settings API

The `feed_fetch`, `asset_cache` and `retention` sections can also be read
and changed over the API, at `/v1/settings/feed-fetch`,
`/v1/settings/asset-cache` and `/v1/settings/retention`. A `PUT` changes the
fields it is given and leaves the rest alone:

```bash
curl --unix-socket "$sock" --request PUT \
    --header 'Content-Type: application/json' \
    --data '{"max_age_days": 30}' \
    http://localhost/v1/settings/retention
```

The API rewrites `kiki.toml` on every change, so comments and formatting in
the file are not kept. While the file is invalid, every settings update
through the API fails with `409 Conflict` rather than overwriting it; fix or
remove the file first.

## Environment variables

| Variable            | Effect                                                                 |
| ------------------- | ---------------------------------------------------------------------- |
| `KIKI_HOME`         | Kiki's data directory; see [Where Kiki keeps its files](files.md).    |
| `KIKI_RUNTIME_DIR`  | The directory holding the socket.                                     |
| `KIKI_SOCKET`       | The exact path of the socket.                                         |
| `KIKI_PROXY`        | Overrides `proxy.url`; see [Proxies](proxy.md).                       |
| `KIKI_NO_PROXY`     | Overrides `proxy.no_proxy`.                                           |
| `RUST_LOG`          | What is logged, such as `warn` or `kiki_rss::process=debug,info`. The default is everything at `info` and above. |

## Per-feed settings

Each feed also has settings of its own, changed with a `PUT` to
`/v1/feeds/id/{id}`: its fetch interval (`min_fetch_interval_seconds`),
whether it uses adaptive fetching, and credentials for feeds behind HTTP
authentication. New feeds take their interval and adaptive fetching from
the global settings above. See the
[API reference](../api/) for the fields.
