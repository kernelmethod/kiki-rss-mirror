# kiki-rss

`kiki` is an RSS/Atom feed aggregator.

## Example usage

Start a Kiki server with

```bash
cargo run --release -- init
cargo run --release -- serve
```

The server prints the socket it is listening on, by default
`$XDG_RUNTIME_DIR/kiki/kiki.sock`.

You can add feeds to the server with e.g.

```bash
curl \
    --header 'Content-Type: application/json' \
    --data '{"title": "my feed", "url": "https://kernelmethod.org/notes/index.xml"}' \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds/create
```

You should then be able to see the server listed with

```bash
curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds

curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds/id/$id
```

The server will automatically populate its database with feed entries once
you've added some endpoints. However, you can manually trigger a feed fetch
with

```bash
curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    --request POST \
    http://localhost/v1/feeds/refresh/$id
```

To move feeds in or out of Kiki from another aggregator, use OPML. Folders in
the OPML file become tags, and feeds that are already present are skipped,
though they gain the tags of the folders they're in:

```bash
kiki opml import subscriptions.opml   # or `-` to read from stdin
kiki opml export -o subscriptions.opml
```

Both commands work on the same database `kiki serve` uses, and a running
server starts fetching imported feeds within a few seconds.

To see the installed plugins, and to read or change a plugin's config as TOML:

```bash
kiki plugin ls
kiki plugin config get hide-sponsored              # --defaults or --overrides for just one part
echo 'patterns = ["sponsored", "webinar"]' | kiki plugin config set hide-sponsored
kiki plugin config set hide-sponsored overrides.toml --replace
```

`config set` overrides the top-level keys it is given and keeps the plugin's
other overrides, unless `--replace` is passed. A running server reloads its
plugins with the new config straight away; it also reloads them whenever a
file in the plugins directory changes.

### Filtering entries

Kiki ships a `filter` plugin, in [`plugins/filter`](plugins/filter), that hides
entries (tags them `system:hidden`) when their title, URL, content, authors,
categories or GUID match regular expressions, or when they fail to match any of
a feed's "include" rules. Hidden entries are left out of the web UI, and of
`GET /v1/entries` and `GET /v1/feeds/id/{id}/entries` unless those are passed
`include_hidden=true`. It is bundled into the `kiki` binary (the
`default-plugins` Cargo feature, on by default) and `kiki init` installs it into
the plugins directory's `system/` directory (your own plugins go in `user/`),
with no rules; pass `kiki init --no-default-plugins` to
skip it. `kiki init --check` installs default plugins into a Kiki home set up
before they were bundled, and updates the ones an earlier release installed,
unless they've been edited since (their config overrides are kept in the
database, so setting config doesn't count). A default plugin you delete isn't
reinstalled; `plugins/system/.default-plugins.toml` records which were installed.
Then give it some rules:

```bash
kiki plugin config set filter <<'EOF'
[[exclude]]
fields = ["title"]
pattern = '\b(sponsored|webinar)\b'
flags = "i"

[[include]]              # for feed 3, hide everything not about Rust
fields = ["title", "categories"]
pattern = "(?i)rust"
feeds = [3]
EOF
```

Whenever its rules change, the filter also applies them to the entries already
downloaded. It never unhides entries. See
[`plugins/filter/main.lua`](plugins/filter/main.lua) for every setting.

### Tagging entries automatically

Kiki also ships an `auto-tag` plugin, in [`plugins/auto-tag`](plugins/auto-tag),
installed by default like `filter`. Each of its rules adds a tag to the entries
whose title, URL, content, authors, categories or GUID match a regular
expression, to every entry from a list of feeds, or, given both, to the entries
from those feeds that match:

```bash
kiki plugin config set auto-tag <<'EOF'
[[rules]]                # tag everything from feed 3, and from this URL
tag = "security"
feeds = [3, "https://example.com/feed.xml"]

[[rules]]                # tag entries about Rust, from any feed
tag = "rust"
fields = ["title", "categories"]
pattern = '\brust\b'
flags = "i"
EOF
```

A rule's tag may also be `system:saved`, `system:read` or `system:hidden`.
Whenever its rules change, the plugin also applies them to the entries already
downloaded. It never removes tags. See
[`plugins/auto-tag/main.lua`](plugins/auto-tag/main.lua) for every setting.

### Stripping tracking parameters

Kiki also ships a `privacy` plugin, in
[`plugins/privacy`](plugins/privacy), installed by default like
`filter`. It removes tracking parameters, such as `utm_source`, `fbclid` and
`gclid`, from the query strings (and query-like fragments, such as
`#xtor=RSS-1`) of new entries' URLs, and of the links in their content. It
also removes tracking pixels from their content: images declared 1×1 or
smaller, and images from known trackers, such as WordPress.com's stats and
FeedBurner, so that Kiki never downloads them. It only cleans entries as they
are downloaded, not the ones already stored. To strip other parameters or
trackers, or to leave entries' content alone:

```bash
kiki plugin config get privacy --defaults > privacy.toml
# edit privacy.toml: add names to `params` ("prefix_*" matches a prefix)
# or image sources to `trackers` ("*.example.com", "example.com/pixel"),
# or set `content = false` or `pixels = false`
kiki plugin config set privacy privacy.toml
```

To keep Kiki from downloading any images or enclosures for some feeds, so
that the sites serving them never hear from it, list the feeds, by id or by
URL, in `skip_assets`:

```bash
echo 'skip_assets = [3, "https://example.com/feed.xml"]' | kiki plugin config set privacy
```

See [`plugins/privacy/main.lua`](plugins/privacy/main.lua) for the details.

### Sanitizing entries' HTML

The `sanitize` plugin, in [`plugins/sanitize`](plugins/sanitize), is also
installed by default. It rewrites each new entry's content so that what Kiki
stores, and hands to API clients, is safe to show: it keeps an allowlist of
formatting elements and attributes, unwraps other elements, and removes
scripts, styles, frames, embedded objects, comments, event handlers, and links
and images whose URLs use a scheme other than `http`, `https` or `mailto`.
It only sanitizes entries as they are downloaded, not the ones already stored.
To keep more, or less, start from its defaults:

```bash
kiki plugin config get sanitize --defaults > sanitize.toml
# edit sanitize.toml: add names to `elements` or `attributes` ("a:href" keeps
# href on <a> only), or schemes to `url_schemes`
kiki plugin config set sanitize sanitize.toml
```

See [`plugins/sanitize/main.lua`](plugins/sanitize/main.lua) for the details.

## Where Kiki keeps its files

| | Default |
| --- | --- |
| Database and cached assets | `$XDG_DATA_HOME/kiki` (`~/.local/share/kiki`) |
| Unix domain socket | `$XDG_RUNTIME_DIR/kiki/kiki.sock` (macOS: beside the database) |

Both are per-user, so one server runs per user with no configuration needed.

Two environment variables name these directly. Each *is* the directory —
neither gets a `kiki/` subdirectory appended, unlike the shared platform
locations they replace:

| | Names |
| --- | --- |
| `$KIKI_HOME` | Kiki's home: the database, settings, and cached assets |
| `$KIKI_RUNTIME_DIR` | Kiki's runtime directory: the socket |

`$KIKI_HOME` alone moves the whole instance, socket included — a directory
Kiki was pointed at keeps the socket too:

```bash
export KIKI_HOME=/srv/kiki
kiki init && kiki serve   # /srv/kiki/kiki.db, /srv/kiki/kiki.sock
```

`$KIKI_RUNTIME_DIR` alone splits the socket back out, wherever the database
happens to live:

```bash
export KIKI_RUNTIME_DIR=/run/kiki
kiki serve   # ~/.local/share/kiki/kiki.db, /run/kiki/kiki.sock
```

Set both and each goes where it was told. `kiki init` takes no arguments —
it always sets up `$KIKI_HOME` when that is set, and the platform data
directory otherwise.

Serving from a directory that already holds a `kiki.db` uses that directory
the same way, so `cd`-ing into one and running `kiki serve` keeps its socket
at `./kiki.sock` unless `$KIKI_RUNTIME_DIR` says otherwise.

To pin the socket to an exact path rather than a directory, pass
`--uds PATH` or set `$KIKI_SOCKET`; both beat `$KIKI_RUNTIME_DIR`.

### Settings

Settings live in `kiki.toml` next to the database. Kiki has built-in
defaults for everything, and the file holds only the settings that differ
from them. It is managed through the `/v1/settings/*` API, which rewrites
the file on every change. Changes made to the file directly are picked up
by the running server; an invalid edit is logged and ignored. While the
file is invalid, every settings update through the API fails with
`409 Conflict` rather than overwriting it, so fix or remove it first.

#### Proxy

To send feed fetches and asset downloads through an HTTP(S) or SOCKS5
proxy, set it in `kiki.toml`:

```toml
[proxy]
url = "http://user:password@proxy.example:3128"
no_proxy = "localhost, .internal.example, 10.0.0.0/8"   # optional
```

or with environment variables, which take precedence over the file:

| | Overrides |
| --- | --- |
| `$KIKI_PROXY` | `proxy.url` |
| `$KIKI_NO_PROXY` | `proxy.no_proxy` |

A `socks5h://` URL sends everything through a SOCKS5 proxy, host name
lookups included, so it is the one to use with Tor:

```toml
[proxy]
url = "socks5h://127.0.0.1:9050"
```

With `socks5://`, Kiki looks host names up itself, and those DNS queries go
out directly, telling whoever can see them which sites Kiki fetches from.

`no_proxy` is a comma-separated list of hosts to reach directly: domains
(which include their subdomains), IP addresses, CIDR ranges, or `*`. When
no proxy URL is set either way, Kiki honors the conventional
`$HTTPS_PROXY`, `$HTTP_PROXY`, `$ALL_PROXY` and `$NO_PROXY` instead.
Changes to the file take effect without a restart; an invalid
`$KIKI_PROXY` stops `kiki serve` at startup.

Kiki serves over a Unix socket and nothing else — it does not listen on
TCP. A socket is reachable only by processes that can reach its path, which
is access control Kiki does not have to implement or authenticate. To expose
it over the network, put a reverse proxy in front of the socket and let that
own the TLS and authentication the job needs; nginx spells it
`proxy_pass http://unix:/run/user/1000/kiki/kiki.sock:;`.

#### Web UI

`kiki web` serves the web UI on `127.0.0.1:8080` (change it with
`--listen`). The web UI has no login, so it answers only requests whose
`Host` header names `localhost`, `127.0.0.1` or `::1`. That way a site
can't reach it by pointing its own domain at your machine (DNS rebinding).
To reach the web UI by another name, such as a LAN host name or address,
allow that name in `kiki.toml`:

```toml
[web_ui]
allowed_hosts = ["kiki.lan", "192.168.1.5", "*.home.example"]
```

or with `--allowed-host`, which can be given more than once and adds to
the file's list. `*.home.example` allows every subdomain of `home.example`,
and `*` allows any host at all, which turns the check off. `kiki web`
reads the file as it starts, so restart it after changing the list.
