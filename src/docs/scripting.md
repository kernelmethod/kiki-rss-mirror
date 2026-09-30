# Writing plugins for Kiki

Kiki has a Lua scripting engine that plugins use to hook into server events —
transforming incoming entries, reacting to feed lifecycle changes, or logging
when something interesting happens.

## Plugins

A plugin is a directory inside the `plugins/` directory in Kiki's home (next
to `kiki.db`; `kiki init` creates it). Every plugin has a manifest,
`manifest.toml`, at its root, and its code alongside:

```text
plugins/
└── hide-sponsored/
    ├── manifest.toml
    ├── main.lua
    └── lib/
        └── rules.lua
```

The manifest is a [TOML](https://toml.io) file declaring the plugin's name,
its version, and the engine its code is written for:

```toml
name = "hide-sponsored"
version = "1.0.0"
engine = "lua"
description = "Hide sponsored posts"
authors = ["Jane Doe <jane@example.com>"]
license = "MIT"
homepage = "https://example.com/hide-sponsored"
entrypoint = "main.lua"
enabled = true

[config]
[[config.rules]]
field = "title"
pattern = '\bsponsored\b'
flags = "i"
```

| Field         | Required | Meaning |
|---------------|----------|---------|
| `name`        | Yes      | The plugin's name: lowercase letters, digits, `-` and `_`, starting with a letter or digit, at most 64 characters. Must be unique among installed plugins. |
| `version`     | Yes      | The plugin's version, as `MAJOR.MINOR.PATCH` with an optional pre-release or build suffix ([Semantic Versioning](https://semver.org)). |
| `engine`      | Yes      | The engine the plugin's code is written for. Currently only `"lua"`. |
| `entrypoint`  | No       | The file that runs when the plugin loads, relative to the plugin directory. Defaults to `main.lua`. |
| `description`, `authors`, `license`, `homepage` | No | Informational; shown by the API. |
| `enabled`     | No       | Set to `false` to keep a plugin installed without running it. Defaults to `true`. |
| `config`      | No       | A table holding the plugin's default config; see [Plugin config](#plugin-config). |
| `settings`    | No       | An array describing the keys of `config`: their types, labels and descriptions; see [Describing settings](#describing-settings). |

Other fields are ignored, so plugins may carry metadata of their own. Kiki
may give meaning to new fields in later versions, as it did to `settings`,
so a plugin's own metadata is best kept under a name unlikely to clash, such
as a table named after the plugin.

To install a plugin, copy its directory into `plugins/`; to remove one, delete
its directory. A running server watches the plugins directory and reloads its
plugins whenever a file in it changes (hidden files, such as editors' swap
files, are ignored), and whenever a plugin's config is changed. A reload
rebuilds every plugin: each entrypoint runs again, and then the
[`plugin.load`](#events) handlers run. If the plugins fail to load, say
because of a syntax error or a bad regex in a config, the plugins that were
running keep running, as they were, and the error is logged.
Plugins load in the order of their directory names, so
prefixing directory names with numbers (`10-filter`, `20-tag`) controls the
order their handlers run in.

A plugin whose manifest is missing or invalid is skipped with a warning in the
server log, and listed with the reason under `errors` in `GET /v1/plugins`;
the other plugins still load. `GET /v1/plugins/name/{name}` shows one plugin's
manifest and config. Both show the plugins as they were last loaded.

When the server runs sandboxed (the default), it can only read files inside
Kiki's home, so a plugin directory that is a symbolic link to somewhere else
cannot be loaded. Symbolic links *inside* a plugin directory are ignored.

## The `kiki` global

Every script runs inside a sandboxed Lua 5.4 VM with a restricted standard
library (`string`, `table`, `math`, and the safe subset of `os`). On top of
that, kiki exposes a single additional global — the `kiki` table:

- `kiki.on(event_name, handler)` — register `handler` to fire every time
  `event_name` is emitted.
- `kiki.log(level, message)` — write `message` to the server log at the given
  level. `level` must be one of `"debug"`, `"info"`, `"warn"`, or `"error"`.
- `kiki.regex(pattern [, flags])` — compile a regular expression. See
  [Regular expressions](#regular-expressions) below.
- `kiki.store` — the plugin's own key-value store, kept in the database. See
  [Storing data](#storing-data).
- `kiki.entries` — tag the entries already stored, and scan through them.
  See [Stored entries](#stored-entries).
- `kiki.feeds` — look up the feeds entries come from. See [Feeds](#feeds).

The sandbox removes `dofile`, `loadfile`, `debug`, `io`, `package`, and the
destructive `os.*` calls (`execute`, `exit`, `getenv`, `remove`, `rename`,
`tmpname`). Scripts cannot read from disk, make network requests, or spawn
processes. `require` loads only the plugin's own modules; see
[Modules](#modules).

Each plugin runs in an environment of its own: globals a plugin defines are
not visible to other plugins.

## Script structure

The recommended shape for a plugin's entrypoint is to register one or more
event handlers via `kiki.on` at the top level. The entrypoint's top-level chunk
runs when plugins load: when the server starts, and again on every reload.
Handlers fire later, each time their event is emitted.

```lua
kiki.on("entry.ingest", function(entry)
    entry.title = "[kiki] " .. entry.title
    return entry
end)

kiki.on("fetch.error", function(err)
    kiki.log("warn", "feed " .. err.feed_id .. " failed: " .. err.message)
end)
```

A script's top-level chunk must not return a value — scripts register
handlers through `kiki.on` side effects only. Returning anything from the
chunk is rejected at load time.

## Modules

A plugin can split its code across several files. Every `.lua` file in the
plugin directory other than the entrypoint is a module, named after its path
with `/` replaced by `.` and the extension dropped: `lib/rules.lua` is
`lib.rules`, and `lib/init.lua` is `lib`. `require(name)` runs a module the
first time it is called and returns what the module returned (or `true` if it
returned nothing); later calls return the same value.

```lua
-- lib/rules.lua
local M = {}
function M.compile(rules) --[[ ... ]] end
return M
```

```lua
-- main.lua
local rules = require("lib.rules")
```

A plugin can only `require` its own modules. A plugin's source files may total
at most 1 MiB.

## Plugin config

Every plugin has a config, handed to the plugin's entrypoint as its argument,
converted to a Lua table, so a plugin reads it with `local config = ...`. The
config is the `[config]` table in the plugin's manifest (its defaults), with
the keys of its config overrides (a JSON object) applied over it. Each plugin
sees only its own config. A plugin with no config gets an empty table.

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

Tables (and JSON objects) become Lua tables keyed by string, and arrays become
sequences indexed from 1. TOML dates and times become strings in their
RFC 3339 form, and `inf` and `nan` are not allowed. A `null` override
becomes `nil`, so a key set to `null` is absent from its table.

For example, this script hides entries whose fields match a configured
regular expression, or whose fields do *not* match one when `invert` is set:

```lua
local config = ...

local rules = {}
for _, rule in ipairs(config.rules or {}) do
    table.insert(rules, {
        field = rule.field,
        re = kiki.regex(rule.pattern, rule.flags),
        invert = rule.invert == true,
    })
end

kiki.on("entry.ingest", function(entry)
    for _, rule in ipairs(rules) do
        local value = entry[rule.field] or ""
        if rule.re:is_match(value) ~= rule.invert then
            table.insert(entry.tags, "system:hidden")
            break
        end
    end
    return entry
end)
```

configured with, say, these overrides (sent with
`PUT /v1/plugins/name/{name}/config`):

```json
{
  "rules": [
    { "field": "title", "pattern": "\\b(sponsored|webinar)\\b", "flags": "i" },
    { "field": "content", "pattern": "rust", "flags": "i", "invert": true }
  ]
}
```

Compiling the regexes in the top-level chunk means a bad pattern in the
config fails when the plugin loads, not on every entry. Note that if any
plugin's code fails to load when the server starts, no plugins run until it is
fixed (a plugin whose *manifest* is invalid is merely skipped); on a reload,
the plugins that were running keep running.

Kiki ships a complete version of this plugin as `plugins/filter` in its
source, and `kiki init` installs it by default (unless Kiki was built without
the `default-plugins` feature, or `--no-default-plugins` is passed); see its
`main.lua` for the settings it takes. `kiki init --check` installs it into an
existing home directory too, and updates it when a new release of Kiki bundles
a new version, unless its files have been edited. It also installs
`plugins/strip-tracking`, which uses `entry.ingest` to remove tracking
parameters such as `utm_source` from entries' URLs and the links in their
content, and tracking pixels from their content, and can keep images from
being downloaded for chosen feeds with `cache_assets`, and `plugins/auto-tag`, which tags entries that match regular
expressions or come from given feeds.

## Events

| Event            | Payload                                    | Purpose |
|------------------|--------------------------------------------|---------|
| `entry.parsed`   | entry table (see below)                    | Observe a newly-parsed entry, before any transformations. Return value is ignored. |
| `entry.ingest`   | entry table (see below)                    | **Transform or filter** a parsed entry. Return the (possibly modified) entry to keep it, or `nil` to drop it. |
| `fetch.success`  | `{ feed_id, status, url, content_length }` | Fires after a successful (2xx) feed fetch, once the response body has been read. |
| `fetch.error`    | `{ feed_id, kind, status, message, retry_after }` | Fires when a feed fetch fails. `kind` is one of `"http"`, `"timeout"`, `"network"`, `"too_many_redirects"`, `"body_too_large"`, `"parse"`, or `"fetcher"` (the isolated feed fetcher itself failed, e.g. its worker crashed while handling this feed). `status` and `retry_after` are populated only when available. |
| `feed.added`     | `{ id, url, title }`                       | Fires after a feed is created via the HTTP API. |
| `feed.removed`   | `{ id, url, title }`                       | Fires after a feed is deleted via the HTTP API, or merged into another feed because it permanently redirected to that feed's URL (its entries then belong to the other feed). `id`, `url`, `title` reflect the feed's state immediately before deletion. |
| `plugin.load`    | none                                       | Fires once plugins have loaded: when the server starts, and after every reload. Where to start a [scan](#stored-entries) of stored entries. |

Only `entry.ingest` is a **transform** event — its handlers can modify or
filter the payload. All other events are observe-only; their return values are
discarded.

### The entry table

The payload for `entry.parsed` and `entry.ingest` is a Lua table with the
following fields:

| Field               | Lua type          | Mutable |
|---------------------|-------------------|---------|
| `id`                | integer, or `nil` for entries being ingested | No |
| `feed_id`           | integer           | No      |
| `syndication_format`| string (`"rss"` or `"atom"`) | No |
| `guid`              | string            | No      |
| `published_at`      | integer (Unix timestamp) or `nil` | Yes |
| `title`             | string            | Yes     |
| `url`               | string or `nil`   | Yes     |
| `content`           | string (HTML) or `nil` | Yes |
| `authors`           | array of strings  | No      |
| `categories`        | array of strings  | No      |
| `tags`              | array of strings  | Yes     |
| `cache_assets`      | boolean           | Yes     |

`authors` holds an RSS item's `<author>`, or the names of an Atom entry's
`<author>`s; `categories` holds an RSS item's `<category>` values, or the terms
of an Atom entry's `<category>`s.

`cache_assets` starts `true`. A handler that sets it to `false` keeps Kiki
from downloading the images in the entry's content, and its enclosure, into
the asset cache, each time the entry is fetched; setting it to `nil` leaves it
`true`. The entry itself is stored all the same. (It has no effect on
entries handed to a [scan](#stored-entries).)

`tags` starts empty. A `tags` holding any user tag replaces the entry's user
tags; one holding only system tags leaves them alone.

Scripts may also add the system tags `system:read`, `system:saved`, and
`system:hidden` to `tags`. These are applied only when the entry is first
stored: after that an entry's system tags record what the user has done with
it (read it, saved it, hidden or unhidden it), and later refreshes leave them
alone. Handlers never remove system tags through `tags` (though a plugin can
with `kiki.entries.untag`). Any other name starting with `system:` is
reserved, and is ignored with a warning.

`id`, `feed_id`, `syndication_format`, and `guid` are identity fields, and
`authors` and `categories` describe the entry as its feed published it.
Handlers may read them, but any modifications are discarded when the entry is
converted back out of Lua.

### Handler chaining

Multiple handlers may be registered for the same event; they run in
registration order (which matches the order of their plugins' directory
names). For `entry.ingest`, the output of one
handler becomes the input to the next — if any handler returns `nil`, the
entry is dropped immediately and subsequent handlers do not run.

## Storing data

`kiki.store` keeps data for the plugin in Kiki's database, so it outlives
reloads and restarts. Each plugin has a store of its own.

- `kiki.store.get(key)` returns the value stored under the string `key`, or
  `nil`.
- `kiki.store.set(key, value)` stores `value` under `key`, or removes the key
  when `value` is `nil`.

Values are kept as JSON: `nil`, booleans, numbers, strings, and tables of
them. A table whose keys are exactly `1..n` is kept as a list, and any other
as an object, whose keys must be strings; tables may nest 32 deep. Keys are 1
to 256 bytes long, a value may take up 64 KiB as JSON, and a plugin may keep
1024 keys.

## Stored entries

`entry.ingest` sees each entry once, as it arrives. `kiki.entries` reaches the
entries already stored, such as those fetched before a plugin was installed or
its config changed:

- `kiki.entries.tag(id, name)` adds the tag `name` to the stored entry with id
  `id`, creating the tag if it is a new user tag, and returns whether the
  entry did not already have it. `name` may be a system tag, such as
  `system:hidden`. Plugins cannot create new user tags once there are 10,000
  user tags; they can still use the ones that exist.
- `kiki.entries.untag(id, name)` removes the tag, returning whether the entry
  had it. System tags can be removed too, so a plugin can mark as unread,
  unsave or unhide entries the user marked; use this with care.
- `kiki.entries.scan([options,] handler [, on_done])` starts a scan: in the
  background, every stored entry that `options` selects is passed to
  `handler`, oldest first, as an entry table with its `id` set and `tags`
  empty. As with a new entry, the system tags the handler adds to `tags` are
  applied to the entry; nothing else it changes is kept, and returning `nil`
  leaves the entry as it is. To change anything else, the handler calls
  `kiki.entries.tag` or `untag`. Once the scan has gone through every entry,
  `on_done`, if given, is called with a table holding `scanned`, how many
  entries the handler saw, and `updated`, how many gained a system tag. It is
  not called for a scan that ends early. Returns the scan's id.

User tags added with `kiki.entries.tag` last only until an `entry.ingest`
handler next sets the entry's user tags: when an entry is fetched again and a
handler returns any user tag for it, its user tags are replaced with the ones
returned, as they would be for tags added by hand. Tags that must last are best
set by the same plugin, on ingest as well as on stored entries.

`options` is a table with any of:

| Option           | Meaning |
|------------------|---------|
| `feed_id`        | Only scan the entries of this feed. |
| `since`          | Only scan entries published at or after this Unix timestamp. |
| `include_hidden` | Also scan entries tagged `system:hidden`, which are skipped by default. |

Each call of the handler has the usual [time budget](#resource-limits), so a
scan can visit any number of entries. Scans and feed refreshes take turns
with the plugins: entries are handed to a scan a few at a time, and after
about 50 ms of handling, any events a refresh has queued up go first. A scan
therefore slows refreshes down by a little, rather than holding them up for
long. A plugin runs one scan at a time: starting another cancels the first.
Reloading the plugins ends every scan, and so does stopping the server; a scan
is not resumed afterwards, and its `on_done` is not called.

Scans cannot start while plugins are loading, from the top-level chunk:
start them from a `plugin.load` handler. Since `plugin.load` fires on every
reload, including those for other plugins' changes, keep track in
`kiki.store` of what a scan has already applied, and record it from `on_done`,
once the scan has finished: a scan that was cut short then runs again on the
next load. This plugin hides stored entries matching a configured pattern, but
only when the pattern changes, so that entries the user has since unhidden
stay unhidden:

```lua
local config = ...
local re = kiki.regex(config.pattern, "i")

local function hide(entry)
    if re:is_match(entry.title) then
        table.insert(entry.tags, "system:hidden")
    end
    return entry
end

kiki.on("entry.ingest", hide)

kiki.on("plugin.load", function()
    if kiki.store.get("pattern") ~= config.pattern then
        kiki.entries.scan(hide, function()
            kiki.store.set("pattern", config.pattern)
        end)
    end
end)
```

## Feeds

An entry names its feed only by `feed_id`. `kiki.feeds.get(id)` looks the
feed up, returning a table with its `id`, `url` (the URL it is fetched from,
or `nil`) and `title`, or `nil` if there is no feed with that id. Feed ids
depend on the order feeds were added in, so this is how a plugin can apply
settings to feeds named by URL:

```lua
local config = ...
local urls = {}
for _, url in ipairs(config.feeds or {}) do
    urls[url] = true
end

local seen = {}
kiki.on("entry.ingest", function(entry)
    if seen[entry.feed_id] == nil then
        local feed = kiki.feeds.get(entry.feed_id)
        seen[entry.feed_id] = feed ~= nil and urls[feed.url] == true
    end
    if seen[entry.feed_id] then
        table.insert(entry.tags, "watched")
    end
    return entry
end)
```

Each lookup is a call to the server, so a plugin that looks feeds up for
every entry is best off remembering the answers, as above. A feed's URL
changes when fetching it is permanently redirected (or, if another feed has
the new URL already, the feed is merged into that one and removed), and a
removed feed's id
may be given to a feed added later (listen for `feed.removed` to forget it).

## Calls to the server

Calls to `kiki.store`, `kiki.entries` and `kiki.feeds` go to the server,
which answers them from the database. Time a handler spends waiting on them
does not count against its [time budget](#resource-limits), up to a second
per handler call.

## Regular expressions

Lua's built-in patterns have no alternation (`a|b`) and no case-insensitive
matching. For anything beyond simple matching, `kiki.regex` compiles a
pattern with Rust's [`regex`](https://docs.rs/regex) crate:

```lua
local promo = kiki.regex([[\b(sponsored|giveaway|webinar)\b]], "i")

kiki.on("entry.ingest", function(entry)
    if promo:is_match(entry.title) then
        table.insert(entry.tags, "promo")
    end
    return entry
end)
```

The pattern syntax is the `regex` crate's: it supports character classes,
alternation, repetition, named groups and inline flags such as `(?i)`, but not
look-around or backreferences. In exchange, matching always runs in time
linear in the input, so a hostile feed cannot make a pattern run for ever.
Long Lua bracket strings (`[[...]]`) avoid having to double every backslash.

`flags` is an optional string of single-letter flags:

| Flag | Meaning |
|------|---------|
| `i`  | Case-insensitive matching. |
| `m`  | `^` and `$` match at the start and end of each line. |
| `s`  | `.` matches `\n` as well. |
| `x`  | Ignore whitespace and allow `#` comments in the pattern. |
| `U`  | Swap the meaning of greedy and lazy repetition. |

An invalid pattern or flag raises an error. Compile regexes at the top of a
script, as above, so that a mistake fails when the script loads rather than on
every entry.

A compiled regex has these methods. Positions are 1-based byte offsets, as with
Lua's `string` functions; `init`, where accepted, is where to start searching
and may be negative to count from the end.

| Method | Returns |
|--------|---------|
| `re:is_match(s)` | `true` if the regex matches anywhere in `s`. |
| `re:find(s [, init])` | The start and end of the first match, or `nil`. |
| `re:match(s [, init])` | The text of the first match, or `nil`. |
| `re:captures(s [, init])` | A table of the first match's groups, or `nil`. `[0]` is the whole match, `[1]`, `[2]`, … are the numbered groups and named groups are also available by name. A group that took no part in the match is `false`. |
| `re:match_all(s)` | An array of the text of every non-overlapping match. |
| `re:replace(s, replacement [, limit])` | `s` with matches replaced by `replacement`, in which `$1` or `${name}` expand to a group (write `$$` for a literal `$`). Replaces every match, or only the first `limit`. |
| `re:split(s [, limit])` | An array of the parts of `s` between matches, splitting into at most `limit` parts if given. |

`re.pattern` and `re.flags` hold the pattern and flags the regex was compiled
with, and `kiki.regex.escape(s)` returns `s` with every metacharacter escaped,
for matching it literally. `kiki.regex.new(pattern [, flags])` is the same as
`kiki.regex(pattern [, flags])`.

Compiling the same pattern with the same flags again returns the regex that is
already compiled, so building a regex inside a handler is cheap, if less tidy.

## Resource limits

Every handler call runs under two hard limits:

- **Time**: 100 ms per invocation. Enforced by a Lua debug hook that fires
  every 1000 VM instructions. Scripts stuck in long-running C-level calls
  (e.g. pathological `string.gsub` patterns) can exceed this slightly before
  control returns to the VM. Time spent waiting on the server in calls to
  `kiki.store`, `kiki.entries` and `kiki.feeds` is not counted, up to one
  second per invocation; past that, waiting counts like anything else.
- **Memory**: 16 MiB across the entire VM. Allocations that would exceed this
  cap fail the handler.
- **Regexes**: compiled regexes live outside the VM, so the memory cap does not
  count them. Each is instead limited to 256 KiB of compiled program (plus a
  matching cache of the same size), and at most 128 distinct regexes may be
  alive at once; exceeding either raises an error. Regexes a script no longer refers to are freed by Lua's garbage
  collector.

When a handler errors, times out, or exceeds the memory cap:

- For `entry.ingest`, the entry passes through that handler **unmodified** —
  a broken script will never silently drop entries.
- For observe events, the failure is logged and dropped.

## Upgrading from database scripts

Earlier versions of Kiki stored scripts in the database and managed them
through the `/v1/scripts/*` API. `kiki migrate` drops those scripts along with
the tables they were kept in; to keep using a script, install it as a plugin.
