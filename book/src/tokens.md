# API tokens

API tokens let programs, devices and people use Kiki with only the access
they need: a phone app that may read and mark entries read, a script that
refreshes feeds, a Prometheus server that scrapes metrics. Each token has a
set of **scopes**, and the API refuses anything outside them.

Tokens are how people log in to the web UI when it requires a login, and
any client of the API can send one to limit itself to the token's scopes.

## Scopes

| Scope | Allows |
| ----- | ------ |
| `read` | Reading feeds, entries, tags and cached assets, searching, and exporting OPML. |
| `state` | Marking entries read, saved or hidden. |
| `tags` | Creating, renaming and deleting tags, and tagging entries and feeds. Includes `state`. |
| `feeds` | Adding, changing, refreshing and deleting feeds, importing OPML, and deleting entries. |
| `metrics` | Scraping the Prometheus metrics at `/metrics`. |
| `admin` | Everything: settings, plugins and their config, managing tokens, deleting cached assets and shutting the server down, as well as every other scope. |

Scopes don't include `read` unless you give it, so a token can be limited
to, say, refreshing feeds. Three presets cover the usual combinations:

| Preset | Scopes |
| ------ | ------ |
| `reader` | `read`, `state` |
| `curator` | `read`, `tags` (and so `state`) |
| `manager` | `read`, `tags`, `feeds` |

The [API reference](../api/) lists the scope each endpoint needs.
`GET /v1/health`, `GET /v1/`, `GET /v1/access`, `/docs` and
`GET /v1/tokens/current` need no particular scope, and are open even to
requests without a token.

## Creating, listing and revoking tokens

```bash
kiki token create phone --scopes reader
kiki token create backup-script --scopes read,feeds --expires 90d
kiki token create prometheus --scopes metrics
kiki token ls
kiki token revoke phone        # by name, or by the id `kiki token ls` shows
```

`kiki token create` prints the token on standard output, by itself, so a
script can capture it. Copy it somewhere safe: Kiki keeps only a hash of
it, so it can't be shown again. `--expires` takes a number of hours, days,
weeks or years (`12h`, `90d`, `6w`, `1y`); without it, the token never
expires.

These commands work on the database directly, so they work whether or not
the server is running, and a running server accepts or refuses a token as
soon as it is created or revoked. With an `admin` token, the same can be
done through the API, under `/v1/tokens`.

Tokens look like `kiki_12_…`: the token's id, then 43 random characters.

## Using a token

Send the token in an `Authorization` header:

```bash
curl --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
  -H "Authorization: Bearer $TOKEN" http://localhost/v1/feeds
```

A request with a malformed, expired or revoked token gets
`401 Unauthorized`, as does one without a token that needs a scope
[anonymous access](#anonymous-access) doesn't grant. One whose token lacks the scope it needs gets
`403 Forbidden`, naming the scope.

`GET /v1/tokens/current` shows the token a request was made with and its
scopes, and works with any valid token, so a client can check a token
before using it. Without a token, it shows the scopes anonymous requests
get.

## Anonymous access

The API is served on Kiki's Unix socket, and by default a token there is
optional. Anyone who can open the socket can already read and write Kiki's
database, so a request without a token may do anything, just as before
tokens existed. A request that does carry a token is held to that token's
scopes; this is how the web UI acts for someone who logged in with a token.

What a request without a token may do is set by `anonymous_access` under
`[api]` in `kiki.toml`:

```toml
[api]
anonymous_access = "read-only"
```

| Value | A request without a token may |
| ----- | ----------------------------- |
| `full` (the default) | Do anything, administration included. |
| `read-only` | Only read, as with a token holding just the `read` scope. |
| `token-required` | Reach only the routes that need no scope. |

A request without a token that needs more gets `401 Unauthorized`, naming
the scope it needs. The change takes effect with the next request. Whatever
the setting, `GET /v1/access` tells anyone, with or without a token, what a
request without one may do:

```bash
curl --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" http://localhost/v1/access
```

```json
{ "anonymous_access": "read-only", "anonymous_scopes": ["read"] }
```

Unless `anonymous_access` is `token-required`, tokens limit what a client
may do only if it sends one. A reverse proxy that forwards requests to the
socket passes on whatever `Authorization` header the client sent, and a
request without one gets the anonymous access. Unless the proxy
authenticates requests itself, or `anonymous_access` is `token-required`,
think twice before exposing the API through it; see
[Exposing Kiki over the network](deployment.md#exposing-kiki-over-the-network).

The web UI sends no token unless someone logged in, so it is held to the
anonymous access too: with `read-only` it only shows entries, and with
`token-required` it requires logging in. It reads the setting as it
starts. `kiki plugin config set` sends no token either, so under
`read-only` or `token-required` the server picks up a plugin's new config
only when it restarts.

## Logging in to the web UI

`kiki web --require-login`, or `require_login = true` under `[web_ui]` in
`kiki.toml`, makes the web UI ask for a token before showing anything. See
[Logging in](web-ui.md#logging-in).
