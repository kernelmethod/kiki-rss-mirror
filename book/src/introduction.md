# Introduction

Kiki is an RSS and Atom feed aggregator, consisting of a central engine
and a [minimal web UI](web-ui.md).

Kiki ships with:

- **`kiki serve`**, the engine. It fetches feeds, honours their caching
  headers and backs off from misbehaving servers, caches images and
  enclosures, and runs plugins over new entries. This functionality is
  exposed over an HTTP API.
- **Plugins** written in Lua, four of which are installed by default: one
  [hides entries](plugins/filter.md) matching rules you set, one
  [tags entries](plugins/auto-tag.md) automatically, one
  [sanitizes entries' HTML](plugins/sanitize.md), and one
  [strips tracking parameters](plugins/privacy.md) and pixels.
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
- Curious how Kiki compares with other engines? See the [Why
  Kiki?](why-kiki.md) for more details.
