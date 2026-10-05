# API tokens

API tokens let programs, devices and people use Kiki with only the access
they need: a phone app that may read and mark entries read, a script that
refreshes feeds, a Prometheus server that scrapes metrics. Each token has a
set of **scopes**, and the API refuses anything outside them.

Tokens are needed to use the API over TCP, which `kiki serve --api-listen`
turns on, and to log in to the web UI when it requires a login.

## Scopes

| Scope | Allows |
| ----- | ------ |
| `read` | Reading feeds, entries, tags and cached assets, searching, and exporting OPML. |
| `state` | Marking entries read, saved or hidden. |
| `tags` | Creating, renaming and deleting tags, and tagging entries and feeds. Includes `state`. |
| `feeds` | Adding, changing, refreshing and deleting feeds, importing OPML, deleting entries, and running cleanup. |
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
`GET /v1/health`, `GET /v1/` and `/docs` need no token at all.

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
curl -H "Authorization: Bearer $TOKEN" http://127.0.0.1:8081/v1/feeds
```

A request with a missing, malformed, expired or revoked token gets
`401 Unauthorized`. One whose token lacks the scope it needs gets
`403 Forbidden`, naming the scope.

`GET /v1/tokens/current` shows the token a request was made with and its
scopes, and works with any valid token, so a client can check a token
before using it.

## Serving the API over TCP

`kiki serve --api-listen ADDR` serves the API over TCP as well as on the
Unix socket. Every request on it needs a token, except for the public
routes above:

```bash
kiki serve --api-listen 127.0.0.1:8081
```

`kiki web` takes the same option.

The listener speaks plain HTTP, so anyone who can watch the network
between a client and Kiki can read the tokens it sends. Keep it on
loopback, or on a private network such as a WireGuard or Tailscale
interface, and to reach it from elsewhere put a reverse proxy that
terminates TLS in front of it; see
[Exposing Kiki over the network](deployment.md#exposing-kiki-over-the-network).

## Tokens on the Unix socket

On the Unix socket a token is optional. Anyone who can open the socket can
already read and write Kiki's database, so a request without a token may do
anything, just as before tokens existed. A request that does carry a token
is held to that token's scopes; this is how the web UI acts for someone who
logged in with a token.

This is also why a reverse proxy should forward to the TCP listener rather
than to the socket: requests it forwards to the socket without a token would
be allowed everything.

## Logging in to the web UI

`kiki web --require-login`, or `require_login = true` under `[web_ui]` in
`kiki.toml`, makes the web UI ask for a token before showing anything. See
[Logging in](web-ui.md#logging-in).
