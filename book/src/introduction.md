# Introduction

Kiki is an RSS/Atom feed _engine_. It is not a feed reader by itself, but the
part of one that runs behind the scenes: it keeps a list of feeds, fetches
them on a sensible schedule, stores their entries, and serves all of it over
an HTTP API. A reader's interface, a sync bridge to another reader, or a
script that consumes feeds can then be built on top of that API.

```text
  feeds on the web ──▶ kiki serve ──▶ Unix socket ──▶ your reader, scripts,
                       (fetch, store,                  or `kiki web`
                        filter, tag)
```

Kiki ships with:

- **`kiki serve`**, the server. It fetches feeds, honours their caching
  headers and backs off from misbehaving servers, caches images and
  enclosures, and runs plugins over new entries.
- **Plugins** written in Lua, three of which are installed by default: one
  [hides entries](plugins/filter.md) matching rules you set, one
  [tags entries](plugins/auto-tag.md) automatically, and one
  [strips tracking parameters](plugins/strip-tracking.md) and pixels.
- **`kiki web`**, a small [web UI](web-ui.md) for reading feeds in a browser.
- **OPML import and export**, for moving feeds in from, or out to, another
  aggregator.

## What Kiki doesn't do

Some things are deliberately left to the applications built on Kiki:

- **Read and saved state.** Kiki has no special notion of an entry being read
  or saved. Instead, entries carry tags, and three built-in _system tags_,
  `system:read`, `system:saved` and `system:hidden`, record the common cases.
  Clients build their features on these.
- **Users and authentication.** There is a single set of feeds, and every
  request to the API is trusted. Kiki only listens on a Unix socket, so
  access is controlled by who can reach that socket; see
  [Security](api.md#security).

## Where to go next

- New to Kiki? Start with [Installation](installation.md) and
  [Getting started](getting-started.md).
- Writing a client? See [The HTTP API](api.md), and the
  [interactive API reference](../api/).
- Writing a plugin? See [Writing plugins](writing-plugins.md).
- Working on Kiki itself? See [Building and testing](development.md), the
  [Rust API documentation](../docs/kiki_rss/) and the
  [test coverage report](../coverage/).
