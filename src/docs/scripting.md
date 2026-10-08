# Writing plugins for Kiki

Plugins hook into Kiki's server events: transforming incoming entries,
reacting to feed lifecycle changes, tagging and deleting stored entries, or
logging when something interesting happens.

A plugin is a [WebAssembly component](https://component-model.bytecodealliance.org/),
written in Rust or any other language that compiles to one. Kiki compiles it
to native code when it loads, and runs it in a sandbox of its own, in a
separate process from the database.

## Plugins

Plugins live in the `plugins/` directory in Kiki's home (next to `kiki.db`;
`kiki init` creates it), which holds two directories: `system/`, for the
plugins bundled with Kiki, which `kiki init` installs and updates, and
`user/`, for the plugins you install yourself. A plugin is a directory
inside one of them, holding a manifest, `manifest.toml`, and its code, the
compiled component:

```text
plugins/
├── system/
│   └── filter/
│       ├── manifest.toml
│       └── plugin.wasm
└── user/
    └── hide-sponsored/
        ├── manifest.toml
        └── plugin.wasm
```

Both kinds load, run and take config the same way. The API shows which kind
each plugin is in its `source` field, `"system"` or `"user"`, and
`kiki plugin ls` and the web UI show it too.

The manifest is a [TOML](https://toml.io) file declaring the plugin's name,
its version, and the engine its code is written for:

```toml
name = "hide-sponsored"
version = "1.0.0"
engine = "wasm"
description = "Hide sponsored posts"
authors = ["Jane Doe <jane@example.com>"]
license = "MIT"
homepage = "https://example.com/hide-sponsored"
entrypoint = "plugin.wasm"
enabled = true

[config]
patterns = ['\bsponsored\b']
```

| Field         | Required | Meaning |
|---------------|----------|---------|
| `name`        | Yes      | The plugin's name: lowercase letters, digits, `-` and `_`, starting with a letter or digit, at most 64 characters. Must be unique among installed plugins. |
| `version`     | Yes      | The plugin's version, as `MAJOR.MINOR.PATCH` with an optional pre-release or build suffix ([Semantic Versioning](https://semver.org)). |
| `engine`      | Yes      | The engine the plugin's code is written for: always `"wasm"`. |
| `entrypoint`  | No       | The plugin's component, relative to the plugin directory. Defaults to `plugin.wasm`. It may be at most 7 MiB. |
| `description`, `authors`, `license`, `homepage` | No | Informational; shown by the API. |
| `enabled`     | No       | Set to `false` to keep a plugin installed without running it. Defaults to `true`. |
| `time_budget_ms` | No    | How long each call of one of the plugin's handlers may run, in milliseconds, or `"unlimited"`; see [Resource limits](#resource-limits). Defaults to `100`. |
| `permissions` | No       | What the plugin may do beyond what every plugin can, such as `["entries.delete"]`; see [Permissions](#permissions). Defaults to none. |
| `config`      | No       | A table holding the plugin's default config; see [Plugin config](#plugin-config). |
| `settings`    | No       | An array describing the keys of `config`: their types, labels and descriptions; see [Describing settings](#describing-settings). |

Other fields are ignored, so plugins may carry metadata of their own. Kiki
may give meaning to new fields in later versions, as it did to `settings`,
so a plugin's own metadata is best kept under a name unlikely to clash, such
as a table named after the plugin.

To install a plugin, copy its directory into `plugins/user/`; to remove one,
delete its directory. A directory placed directly in `plugins/` isn't loaded,
and is listed under `errors` in `GET /v1/plugins`. A running server watches the plugins directory and reloads its
plugins whenever a file in it changes (hidden files, such as editors' swap
files, are ignored), and whenever a plugin's config is changed. A reload
rebuilds every plugin: each is made afresh from its config, and then the
[`plugin.load`](#events) handlers run. If the plugins fail to load, say
because a component is broken or a config has a bad regex, the plugins that
were running keep running, as they were, and the error is logged.
Plugins load in the order of their directory names, system and user plugins
together (a system plugin first, if two directories have the same name), so
prefixing directory names with numbers (`10-filter`, `20-tag`) controls the
order their handlers run in. Two plugins with the same name can't both be
installed: the one that loads second is skipped.

A plugin whose manifest is missing or invalid is skipped with a warning in the
server log, and listed with the reason under `errors` in `GET /v1/plugins`;
the other plugins still load. `GET /v1/plugins/name/{name}` shows one plugin's
manifest and config. Both show the plugins as they were last loaded.

When the server runs sandboxed (the default), it can only read files inside
Kiki's home, so a plugin directory that is a symbolic link to somewhere else
cannot be loaded.

## Writing a plugin in Rust

The `kiki-plugin` crate, in
[`sdk/rust/kiki-plugin`](https://github.com/kernelmethod/kiki-rss/tree/main/sdk/rust/kiki-plugin)
in Kiki's source, generates the bindings to Kiki and wraps them in a `Plugin`
trait and a `#[plugin]` attribute: implement `Plugin` to make your plugin
from its config, and put `#[plugin]` on an `impl` block holding its handlers,
each a method marked with the event it handles, such as `#[on(entry.ingest)]`.
Make a library crate whose `Cargo.toml` has

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
kiki-plugin = { git = "https://github.com/kernelmethod/kiki-rss" }
serde = { version = "1", features = ["derive"] }

[profile.release]
opt-level = "s"
lto = true
strip = true
panic = "abort"
```

and in `src/lib.rs`, a plugin that hides entries whose title matches one of
the patterns in its config:

```rust,ignore
use kiki_plugin::regex::Regex;
use kiki_plugin::{parse_config, plugin, Entry, Plugin};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    patterns: Vec<String>,
}

struct HideMatching {
    patterns: Vec<Regex>,
}

impl Plugin for HideMatching {
    fn new(config: &str) -> Result<Self, String> {
        let config: Config = parse_config(config)?;
        let patterns = config
            .patterns
            .iter()
            .map(|p| Regex::compile(p, "i"))
            .collect::<Result<_, _>>()?;
        Ok(HideMatching { patterns })
    }
}

#[plugin]
impl HideMatching {
    #[on(entry.ingest)]
    fn hide_matching(&mut self, mut entry: Entry) -> Option<Entry> {
        if self.patterns.iter().any(|re| re.is_match(&entry.title)) {
            entry.tags.push("system:hidden".to_string());
        }
        Some(entry)
    }
}
```

Build it for the `wasm32-wasip2` target, and install the result as the
plugin's `plugin.wasm`:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/hide_matching.wasm \
    ~/.local/share/kiki/plugins/user/hide-matching/plugin.wasm
```

For fuller plugins, with manifests describing their settings, see the ones
Kiki installs by default, whose source is in
[`plugins/`](https://github.com/kernelmethod/kiki-rss/tree/main/plugins) in
Kiki's source: [`filter`](plugins/filter.md), [`auto-tag`](plugins/auto-tag.md),
[`sanitize`](plugins/sanitize.md), [`privacy`](plugins/privacy.md),
[`adaptive-fetch`](plugins/adaptive-fetch.md) and
[`retention`](plugins/retention.md).

`Plugin::new` is called once when the plugin loads, and every handler is
called on the value it returns, so a plugin keeps what it needs, such as
compiled regexes, in its own fields. An error from `Plugin::new` fails the
load. A handler takes `&mut self` or `&self`, then the event's arguments:

| `#[on(...)]`     | Arguments                         | Returns         |
|------------------|-----------------------------------|-----------------|
| `entry.parsed`   | `entry: Entry`                    |                 |
| `entry.ingest`   | `entry: Entry`                    | `Option<Entry>` |
| `fetch.success`  | `event: FetchSuccess`             |                 |
| `fetch.error`    | `event: FetchError`               |                 |
| `feed.added`     | `feed: FeedEvent`                 |                 |
| `feed.removed`   | `feed: FeedEvent`                 |                 |
| `plugin.load`    |                                   |                 |
| `fetch.schedule` | `schedule: FetchSchedule`         | `Option<u64>`   |
| `timer`          | `id: u32`                         |                 |
| `scan.entry`     | `scan: u64, entry: Entry`         | `Option<Entry>` |
| `scan.done`      | `scan: u64, summary: ScanSummary` |                 |

The first eight are the server's [events](#events); `timer` is for
[timers](#timers), and `scan.entry` and `scan.done` for
[scans](#stored-entries). A plugin handles the events it has handlers for,
and only those are delivered to it. One that needs some of them only for
some configs can implement `Plugin::wants` to leave the others out, since
Kiki calls into a plugin for every event it handles. An unknown event, two
handlers for one event, or a handler whose signature doesn't fit its event
fails to compile. A handler that panics traps, which is handled as described
under [Resource limits](#resource-limits).

The calls a plugin makes to the server are in `kiki_plugin::host`, and
`kiki_plugin::log` writes to Kiki's log.

## The interface

The interface between Kiki and a plugin is defined in
[WIT](https://component-model.bytecodealliance.org/design/wit.html), in
[`sdk/rust/kiki-plugin/wit/kiki-plugin.wit`](https://github.com/kernelmethod/kiki-rss/blob/main/sdk/rust/kiki-plugin/wit/kiki-plugin.wit)
in Kiki's source. A plugin is a component targeting its `plugin` world (or a
core module carrying that world's type information, as `wit-bindgen` builds
for `wasm32-unknown-unknown`, which Kiki turns into one), which:

- imports `kiki:plugin/host`, the plugin's calls to the server: `log`,
  `store-get` and `store-set`, `tag-entry` and `untag-entry`, `start-scan`,
  `delete-entries`, `get-feed`, and `every`;
- imports `kiki:plugin/regex`: see [Regular expressions](#regular-expressions);
- exports `init`, which Kiki calls with the plugin's config, as a JSON
  object, when the plugin loads. It returns the events the plugin handles,
  and only those are delivered to it; an error fails the load;
- exports a function for each event: `on-entry-ingest`, `on-fetch-schedule`,
  `on-plugin-load` and so on, and `on-timer`, `on-scan-entry` and
  `on-scan-done` for timers and scans.

`every(secs)` returns a timer's id, which is passed to `on-timer` each time
the timer is due, and `start-scan(options)` returns a scan's id, which is
passed with each of its entries to `on-scan-entry`, and with its summary to
`on-scan-done`. Values in the plugin's store are JSON text.

The Rust crate wraps all of this; other languages can use the WIT file with
their own bindings generator, such as `wit-bindgen`'s for C.

## Plugin config

Every plugin has a config, handed to it when it loads as a JSON object: the
`config` argument of `Plugin::new`, which `kiki_plugin::parse_config`
deserializes. The config is the `[config]` table in the plugin's manifest
(its defaults), with the keys of its config overrides (a JSON object)
applied over it. Each plugin sees only its own config. A plugin with no
config gets an empty object.

Overrides are kept in Kiki's database, keyed by plugin name, and set through
the API under `/v1/plugins/name/{name}/config`: `GET` shows the plugin's
defaults, its overrides, and the config they add up to; `PUT` replaces every
override; `PATCH` sets some overrides and keeps the rest; `DELETE` removes
every override; and `DELETE /v1/plugins/name/{name}/config/{key}` removes one.
From the command line, `kiki plugin config get <name>` prints a plugin's
config as TOML (`--defaults` or `--overrides` for just one part), and
`kiki plugin config set <name> [FILE]` sets overrides from a TOML document,
read from `FILE` or standard input, keeping the others unless `--replace` is
given. `kiki plugin ls` lists the installed plugins.
In the web UI (`kiki web`), each plugin on the Plugins page links to a page
that shows its config and has a form to set, reset or add each setting. Each
setting gets fields that fit it (see [Describing settings](#describing-settings));
any setting can also be edited as JSON.
Since overrides live outside the plugin directory, a new version of a plugin
can be dropped in without losing them. Changing a config reloads the plugins,
so it takes effect at once. If the plugins fail to load with it, it is still
saved, but the plugins keep running with the config they had; the API's
response says why in `reload_error`, and `kiki plugin config set` and the web
UI report it.

TOML tables become JSON objects, and arrays become arrays. TOML dates and
times become strings in their RFC 3339 form, and `inf` and `nan` are not
allowed.

Check the config, and compile any regexes it holds, in `Plugin::new`, as
above, so that a bad pattern fails when the plugin loads, not on every
entry. Note that if any plugin fails to load when the server starts, no
plugins run until it is fixed (a plugin whose *manifest* is invalid is
merely skipped); on a reload, the plugins that were running keep running.

### Describing settings

A manifest can describe the keys of its `[config]` table with a
`[[settings]]` array. The web UI then shows a form field that fits each one,
such as a checkbox, a drop-down or a fieldset per rule, instead of asking for
JSON, and the config API and `kiki plugin config set` refuse values of the
wrong type (a described key may still be set to `null`). Keys that are not
described get fields guessed from their defaults, and are not checked.

```toml
[config]
limit = 10
rules = []

[[settings]]
name = "limit"
type = "integer"
label = "Limit"
description = "How many entries to look at."
min = 1

[[settings]]
name = "rules"
type = "list"
label = "Rules"

[settings.items]
type = "object"

[[settings.items.fields]]
name = "pattern"
type = "string"
required = true

[[settings.items.fields]]
name = "fields"
type = "list"
items = { type = "choice", choices = ["title", "content"] }
```

| `type`    | Holds                          | Options |
|-----------|--------------------------------|---------|
| `string`  | A string                       | `multiline = true` for text over several lines |
| `integer` | A whole number                 | `min`, `max` |
| `number`  | Any number                     | `min`, `max` |
| `boolean` | `true` or `false`              | |
| `choice`  | One of a fixed set of strings  | `choices` (required) |
| `feed`    | A feed: its id, or the URL it is fetched from | |
| `list`    | A list of values of one type   | `items` (required): a table with a `type` and its options |
| `object`  | A table with named fields      | `fields` (required): an array of settings, written like the top-level ones |
| `json`    | Anything; edited as JSON       | |

Every setting has a `name` (the config key, or the object field) and may have
a `label` and a `description`. A field of an object may be `required`; the
others may be left out. Top-level settings may not be `required`: they take
their default from `[config]`, and can be reset to it. Options Kiki does not
know are ignored, so a misspelt option (`mni = 1`) silently has no effect. Objects may not hold fields they do not describe, and
field names are made of ASCII letters, digits, `_` and `-`. A manifest whose
settings are malformed, or whose `[config]` defaults do not match them, is
invalid.

Values are checked when they are saved, not when plugins load. Overrides
saved before a plugin described or tightened a setting keep running as they
are; the web UI shows ones that no longer match as JSON, and saving that key
again must give a value that matches.

## Events

| Event            | Payload                                    | Purpose |
|------------------|--------------------------------------------|---------|
| `entry.parsed`   | `Entry` (see below)                        | Observe a newly-parsed entry, before any transformations. |
| `entry.ingest`   | `Entry` (see below)                        | **Transform or filter** a parsed entry. Return the (possibly modified) entry to keep it, or `None` to drop it. |
| `fetch.success`  | `FetchSuccess`: `feed_id`, `status`, `url`, `content_length` | Fires after a successful (2xx) feed fetch, once the response body has been read. |
| `fetch.error`    | `FetchError`: `feed_id`, `kind`, `status`, `message`, `retry_after` | Fires when a feed fetch fails. `kind` is one of `"http"`, `"timeout"`, `"network"`, `"too_many_redirects"`, `"body_too_large"`, `"parse"`, or `"fetcher"` (the isolated feed fetcher itself failed, e.g. its worker crashed while handling this feed). `status` and `retry_after` are set only when available. |
| `feed.added`     | `FeedEvent`: `id`, `url`, `title`          | Fires after a feed is created via the HTTP API. |
| `feed.removed`   | `FeedEvent`: `id`, `url`, `title`          | Fires after a feed is deleted via the HTTP API, or merged into another feed because it permanently redirected to that feed's URL (its entries then belong to the other feed). `id`, `url`, `title` reflect the feed's state immediately before deletion. |
| `plugin.load`    | none                                       | Fires once plugins have loaded: when the server starts, and after every reload. Where to start a [scan](#stored-entries) of stored entries. |
| `fetch.schedule` | `FetchSchedule` (see below)                | **Lengthen the wait** before a feed's next fetch. Return a number of seconds, or `None` to leave it. See [Scheduling fetches](#scheduling-fetches). |

`entry.ingest` and `fetch.schedule` are **transform** events: their handlers
can change what happens. All other events are observe-only.

### The entry

The payload of `entry.parsed` and `entry.ingest` is an entry with the
following fields:

| Field               | Type              | Mutable |
|---------------------|-------------------|---------|
| `id`                | integer, or none for entries being ingested | No |
| `feed_id`           | integer           | No      |
| `syndication_format`| string (`"rss"` or `"atom"`) | No |
| `guid`              | string            | No      |
| `published_at`      | integer (Unix timestamp), or none | Yes |
| `title`             | string            | Yes     |
| `url`               | string, or none   | Yes     |
| `content`           | string (HTML), or none | Yes |
| `authors`           | list of strings   | No      |
| `categories`        | list of strings   | No      |
| `tags`              | list of strings   | Yes     |
| `cache_assets`      | boolean           | Yes     |

`authors` holds an RSS item's `<author>`, or the names of an Atom entry's
`<author>`s; `categories` holds an RSS item's `<category>` values, or the terms
of an Atom entry's `<category>`s.

`cache_assets` starts `true`. A handler that sets it to `false` keeps Kiki
from downloading the images in the entry's content, and its enclosure, into
the asset cache, each time the entry is fetched. The entry itself is stored
all the same. (It has no effect on entries handed to a
[scan](#stored-entries).)

`tags` starts empty. A `tags` holding any user tag replaces the entry's user
tags; one holding only system tags leaves them alone.

Plugins may also add the system tags `system:read`, `system:saved`, and
`system:hidden` to `tags`. These are applied only when the entry is first
stored: after that an entry's system tags record what the user has done with
it (read it, saved it, hidden or unhidden it), and later refreshes leave them
alone. Handlers never remove system tags through `tags` (though a plugin can
with `untag-entry`). Any other name starting with `system:` is reserved, and
is ignored with a warning.

`id`, `feed_id`, `syndication_format`, and `guid` are identity fields, and
`authors` and `categories` describe the entry as its feed published it.
Handlers may read them, but any modifications are discarded.

### Handler chaining

Handlers for the same event run in the order of their plugins' directory
names. For `entry.ingest`, the output of one handler becomes the input to
the next — if any handler returns `None`, the entry is dropped immediately
and subsequent handlers do not run.

## Scheduling fetches

After a fetch that found a feed working, Kiki plans the next one: as soon as
the freshness hint from the server's `Cache-Control` or `Expires` (or the
feed's own `<ttl>` or `sy:updatePeriod`) runs out, but no sooner than
`min_polling_cadence_seconds` and no later than the feed's own interval.
When that hint is shorter than the interval, `fetch.schedule` fires, and its
handlers may have Kiki wait longer. The bundled `adaptive-fetch` plugin
(`plugins/adaptive-fetch/src/lib.rs`) uses it to back off from feeds that keep
turning out unchanged.

The payload has:

| Field              | Meaning |
|--------------------|---------|
| `feed_id`          | The feed that was fetched. |
| `status`           | `200`, or `304` for `Not Modified`. |
| `change`           | `Changed` if the feed's content differs from the last fetch, `Unchanged` if not (a `304`, or the same body again), or `Unknown` if there is nothing to compare against, as on a feed's first fetch. |
| `hint_secs`        | The freshness hint, in seconds. |
| `interval_secs`    | The feed's fetch interval, in seconds. |
| `min_cadence_secs` | The server's `min_polling_cadence_seconds`. |
| `wait_secs`        | The wait before the next fetch, in seconds. |

A handler returns the number of seconds to wait instead, or `None` to leave
the wait as it is. Handlers run in order, each seeing the wait the one
before chose in `wait_secs`. Kiki then holds the wait between the one it
planned and the feed's interval: a plugin can have a feed fetched less often
than its server asks, but never more often, and never less often than its
interval.

```rust,ignore
// Fetch no feed more than once every ten minutes.
#[on(fetch.schedule)]
fn at_most_every_ten_minutes(&mut self, schedule: FetchSchedule) -> Option<u64> {
    Some(schedule.wait_secs.max(600))
}
```

The event fires on every such fetch, so a handler should be quick, and keep
what it needs from the server, such as values in its store, in its own
fields rather than asking for them every time.

## Storing data

A plugin has a key-value store of its own in Kiki's database, so what it
keeps there outlives reloads and restarts.

- `store-get(key)` returns the value stored under the string `key`, as JSON
  text, or none.
- `store-set(key, value)` stores `value`, JSON text, under `key`, or removes
  the key when `value` is none.

In Rust, `kiki_plugin::host::get` and `set` deserialize and serialize the
values with `serde`. Keys are 1 to 256 bytes long, a value may take up
64 KiB as JSON, and a plugin may keep 1024 keys.

## Stored entries

`entry.ingest` sees each entry once, as it arrives. These calls reach the
entries already stored, such as those fetched before a plugin was installed
or its config changed:

- `tag-entry(id, name)` adds the tag `name` to the stored entry with id
  `id`, creating the tag if it is a new user tag, and returns whether the
  entry did not already have it. `name` may be a system tag, such as
  `system:hidden`. Plugins cannot create new user tags once there are 10,000
  user tags; they can still use the ones that exist.
- `untag-entry(id, name)` removes the tag, returning whether the entry had
  it. System tags can be removed too, so a plugin can mark as unread,
  unsave or unhide entries the user marked; use this with care.
- `start-scan(options)` starts a scan, and returns its id: in the
  background, every stored entry that `options` selects is passed to the
  plugin's `scan.entry` handler, with the scan's id, oldest first, with its
  `id` set and `tags` empty. As with a new entry, the system tags the handler
  adds to `tags` are applied to the entry; nothing else it changes is kept,
  and returning `None` leaves the entry as it is. To change anything else,
  the handler calls `tag-entry` or `untag-entry`. Once the scan has gone
  through every entry, the plugin's `scan.done` handler is called with the
  scan's id and a summary holding `scanned`, how many entries the handler
  saw, and `updated`, how many gained a system tag. It is not called for a
  scan that ends early.

User tags added with `tag-entry` last only until an `entry.ingest`
handler next sets the entry's user tags: when an entry is fetched again and a
handler returns any user tag for it, its user tags are replaced with the ones
returned, as they would be for tags added by hand. Tags that must last are best
set by the same plugin, on ingest as well as on stored entries.

`options` has:

| Option           | Meaning |
|------------------|---------|
| `feed_id`        | Only scan the entries of this feed, if set. |
| `since`          | Only scan entries published at or after this Unix timestamp, if set. |
| `include_hidden` | Also scan entries tagged `system:hidden`, which are skipped otherwise. |

Each call of the handler has the usual [time budget](#resource-limits), so a
scan can visit any number of entries. Scans and feed refreshes take turns
with the plugins: entries are handed to a scan a few at a time, and after
about 50 ms of handling, any events a refresh has queued up go first. A scan
therefore slows refreshes down by a little, rather than holding them up for
long. A plugin runs one scan at a time: starting another cancels the first.
Reloading the plugins ends every scan, and so does stopping the server; a scan
is not resumed afterwards, and its `scan.done` is not called.

Scans cannot start while plugins are loading, from `Plugin::new`: start them
from a `plugin.load` handler. Since `plugin.load` fires on every reload,
including those for other plugins' changes, keep track in the plugin's
store of what a scan has already applied, and record it from `scan.done`,
once the scan has finished: a scan that was cut short then runs again on the
next load. This plugin hides stored entries matching a configured pattern, but
only when the pattern changes, so that entries the user has since unhidden
stay unhidden:

```rust,ignore
use kiki_plugin::regex::Regex;
use kiki_plugin::{host, parse_config, plugin, Entry, Plugin, ScanOptions, ScanSummary};
use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    pattern: String,
}

struct Hide {
    pattern: String,
    re: Regex,
}

impl Plugin for Hide {
    fn new(config: &str) -> Result<Self, String> {
        let Config { pattern } = parse_config(config)?;
        let re = Regex::compile(&pattern, "i")?;
        Ok(Hide { pattern, re })
    }
}

impl Hide {
    fn hide(&self, mut entry: Entry) -> Option<Entry> {
        if self.re.is_match(&entry.title) {
            entry.tags.push("system:hidden".to_string());
        }
        Some(entry)
    }
}

#[plugin]
impl Hide {
    #[on(entry.ingest)]
    fn ingest(&mut self, entry: Entry) -> Option<Entry> {
        self.hide(entry)
    }

    #[on(plugin.load)]
    fn load(&mut self) {
        if host::get::<String>("pattern").ok().flatten() != Some(self.pattern.clone()) {
            let options = ScanOptions { feed_id: None, since: None, include_hidden: false };
            let _ = host::start_scan(options);
        }
    }

    #[on(scan.entry)]
    fn scan(&mut self, _scan: u64, entry: Entry) -> Option<Entry> {
        self.hide(entry)
    }

    #[on(scan.done)]
    fn done(&mut self, _scan: u64, _summary: ScanSummary) {
        let _ = host::set("pattern", &self.pattern);
    }
}
```

## Deleting entries

`delete-entries(filter)` deletes stored entries, and returns how many it
deleted. Deleting cannot be undone, so it needs the `entries.delete`
[permission](#permissions).

It only ever deletes entries their feed has stopped listing: an entry still
in its feed would be fetched again on the feed's next refresh, and stored as
a new, unread entry. Kiki notes when a refresh finds that a feed no longer
lists an entry; `filter` says which of those entries to delete:

| Filter             | Meaning |
|--------------------|---------|
| `dropped_before`   | Delete entries their feed stopped listing before this Unix timestamp. |
| `feed_id`          | Only delete the entries of this feed, if set. |
| `published_before` | Only delete entries published before this Unix timestamp, if set. Entries with no publication date are kept. |
| `keep_tagged`      | Keep entries tagged with any of these tags. Defaults to `["system:saved"]` when not set; an empty list deletes entries however they are tagged. |

A `system:` name in `keep_tagged` that is not a system tag fails the call. To
keep saved entries and entries with the user tag `keep` too, pass
`["system:saved", "keep"]`: setting `keep_tagged` replaces the default,
rather than adding to it. Kiki deletes the entries a few hundred at a time,
so that a large deletion does not hold up feed refreshes for long, but the
call returns only once they are all deleted; a plugin that may delete many
entries at once may need a longer [time budget](#resource-limits). The
bundled `retention` plugin deletes entries some days after their feed stops
listing them, when it loads and then once an hour; see
`plugins/retention/src/lib.rs`.

## Timers

`every(secs)` starts a timer, and returns its id: the plugin's `timer`
handler is called with the id every `secs` seconds, the first time `secs`
seconds after `every` is called. Kiki checks for timers that are due once a
minute, so `secs` must be at least `60`, and at most a year, and a timer may
run up to a minute late; one that runs late does not make the next one late
too. Each call has the plugin's usual [time budget](#resource-limits), and a
failure in one is logged, and does not stop the timer.

A timer can be started from `Plugin::new` or from a handler. Timers are not
kept across reloads: when plugins reload, every timer stops, and the plugin,
made afresh, starts its timers again. Since a timer's first call is a whole
interval away, a plugin that wants to do its work as soon as it loads should
do it from a `plugin.load` handler as well.

## Permissions

Some calls do what cannot be undone, so a plugin may only make them if its
manifest asks to, in its `permissions` array:

```toml
permissions = ["entries.delete"]
```

| Permission       | Allows |
|------------------|--------|
| `entries.delete` | Deleting stored entries, with [`delete-entries`](#deleting-entries). |

A call a plugin has not asked for permission to make fails. A manifest naming
a permission Kiki does not know is invalid. The permissions each plugin asks
for are shown by `kiki plugin ls`, the web UI and `GET /v1/plugins`, so that
you can see what a plugin may do before you install it.

## Feeds

An entry names its feed only by `feed_id`. `get-feed(id)` looks the feed up,
returning its `id`, `url` (the URL it is fetched from, if any) and `title`,
or none if there is no feed with that id. Feed ids depend on the order feeds
were added in, so this is how a plugin can apply settings to feeds named by
URL.

Each lookup is a call to the server, so a plugin that looks feeds up for
every entry is best off remembering the answers, in a map keyed by feed id. A
feed's URL changes when fetching it is permanently redirected (or, if another
feed has the new URL already, the feed is merged into that one and removed),
and a removed feed's id may be given to a feed added later (handle
`feed.removed` to forget it).

## Calls to the server

Calls to the plugin's store, to stored entries and to feeds go to the
server, which answers them from the database, and checks that the plugin has
the [permission](#permissions) a call needs. A call that fails returns an
error message. Time a handler spends waiting on them does not count against
its [time budget](#resource-limits), up to a second per handler call.

## Regular expressions

`kiki_plugin::regex`, the `regex` interface, compiles and matches regular
expressions in Kiki itself, as native code. That is faster than a regex
library built into the plugin, and leaves the plugin much smaller: the
`regex` crate alone adds about a megabyte to a plugin.

- `Regex::compile(pattern, flags)` compiles a pattern; `is_match` and `find`
  match it. `find(haystack, start)` returns the byte offsets of the start
  and end of the first match at or after `start`.
- `RegexSet::compile(patterns)` compiles a list of patterns and flags, whose
  `matches` returns which of them match a string, in one call to Kiki rather
  than one per pattern.

The pattern syntax is the [`regex`](https://docs.rs/regex) crate's: it
supports character classes, alternation, repetition, named groups and inline
flags such as `(?i)`, but not look-around or backreferences. In exchange,
matching always runs in time linear in the input, so a hostile feed cannot
make a pattern run for ever.

`flags` is a string of single-letter flags:

| Flag | Meaning |
|------|---------|
| `i`  | Case-insensitive matching. |
| `m`  | `^` and `$` match at the start and end of each line. |
| `s`  | `.` matches `\n` as well. |
| `x`  | Ignore whitespace and allow `#` comments in the pattern. |
| `U`  | Swap the meaning of greedy and lazy repetition. |

An invalid pattern or flag fails the compile with a message. Compile regexes
in `Plugin::new`, so that a mistake fails when the plugin loads rather than
on every entry.

A plugin may have 128 distinct patterns alive at once. Compiling a pattern
it has alive already, with the same flags, shares it, so a pattern in a
`Regex` and a `RegexSet` counts once; dropping the last one holding a
pattern frees its place. Each compiled pattern is limited to 256 KiB of
compiled program, plus a matching cache of the same size.

Plugins can bundle a regex library instead, if they need something the
interface lacks, such as captures or replacement. Prefer matching each
pattern alone to the `regex` crate's `RegexSet`, though: a set of patterns,
one of which has no literal text to look for, can be a hundred times slower
on text that is not ASCII.

## What a plugin can reach

A plugin runs in a sandbox of its own: it shares no memory with Kiki or with
any other plugin, and can reach nothing but what the `host` and `regex`
interfaces offer. Of [WASI](https://wasi.dev/), the system interface
WebAssembly toolchains build on, Kiki provides only randomness
(`wasi:random`) and the clocks (`wasi:clocks`' `now` and `resolution`), which
libraries use without being asked: Rust's `HashMap` seeds its hasher from
it, for instance. There are no files, no network, no environment and no
standard streams. A plugin built for `wasm32-wasip2` still loads, but calling
any of them, as `std::fs`, `std::env` or `println!` do, traps. Write to
Kiki's log with `log` instead.

Plugins run in the script host, a separate process from the server that
holds no database handle, no filesystem access and no sockets but the one it
talks to the server over, so a plugin that broke out of its sandbox would
find nothing worth having. See the [threat model](threat-model.md#a-misbehaving-plugin).

## Resource limits

Every call into a plugin runs under hard limits:

- **Time**: 100 ms per call, unless the plugin's manifest sets
  `time_budget_ms` to another number of milliseconds or to `"unlimited"`.
  Time spent waiting on the server, in [calls to it](#calls-to-the-server),
  is not counted, up to one second per call; past that, waiting counts like
  anything else. Loading a plugin, which runs its `init`, may take up to 5
  seconds.

  An unlimited handler still has a backstop: plugins run one handler at a
  time in a separate process, and if that process doesn't answer the server
  for 10 seconds, the server stops it, and with it every plugin, until Kiki
  restarts. Keep `"unlimited"` for plugins you trust to finish, such as the
  bundled `sanitize`, which every new entry passes through.
- **Memory**: 16 MiB for each plugin, and 512 KiB of stack.
- **Regexes**: compiled regexes live outside the plugin's memory, and have
  limits of their own; see [Regular expressions](#regular-expressions).

Kiki compiles each plugin to native code when it loads, once for each
distinct `plugin.wasm`, so changing only a plugin's config reloads it without
compiling it again.

When a handler traps, whether it ran out of time or memory, panicked, or
called into WASI that Kiki doesn't provide, it fails:

- For `entry.ingest`, the entry passes through that handler **unmodified** —
  a broken plugin will never silently drop entries.
- For `fetch.schedule`, the wait is left as it was, and the next handler runs.
- For other events, the failure is logged and dropped.

A trap can leave the plugin's memory in any state, so the plugin is then
started afresh the next time it is needed: Kiki loads it again and calls
`init`, which starts its timers again, while the scans it had started end.
Timers that were due when the plugin trapped aren't called. Kiki restarts at
most one plugin per event, so a plugin may miss an event or two if several
trap at once. A plugin that traps more than five times in ten minutes is
disabled until plugins next reload.

## Security

Compiling plugins to native code means the process plugins run in, the
script host, has to be able to write code to memory and then run it, which
Kiki's other processes are not allowed to do (nor is the server, unless it
runs plugins itself, with `kiki serve --no-script-isolation`). Systemd's
`MemoryDenyWriteExecute=` would stop it, and is inherited by every process of
a service, so the units Kiki ships don't set it; if you wrote your own,
remove it, or plugins will fail to compile. See the
[threat model](threat-model.md#a-misbehaving-plugin) for what this means.

## Upgrading from Lua plugins

Earlier versions of Kiki ran plugins written in Lua, as well as WebAssembly
ones. Kiki no longer runs Lua: a plugin whose manifest says `engine = "lua"`
is listed under `errors` in `GET /v1/plugins`, and skipped. To keep using
one, rewrite it as a WebAssembly plugin. The plugins Kiki bundles were all
rewritten in Rust, taking the same configs, and `kiki init --check` replaces
their Lua versions, unless you edited them.

Earlier still, Kiki stored scripts in the database and managed them through
the `/v1/scripts/*` API. `kiki migrate` drops those scripts along with the
tables they were kept in.
