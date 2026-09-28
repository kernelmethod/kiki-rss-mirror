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

Other fields are ignored, so plugins may carry metadata of their own.

To install a plugin, copy its directory into `plugins/`; to remove one, delete
its directory. Kiki discovers plugins only when the server starts, so restart
the server after installing, removing, or editing a plugin (including its
`config.json`). Plugins load in the order of their directory names, so
prefixing directory names with numbers (`10-filter`, `20-tag`) controls the
order their handlers run in.

A plugin whose manifest is missing or invalid is skipped with a warning in the
server log, and listed with the reason under `errors` in `GET /v1/plugins`;
the other plugins still load. `GET /v1/plugins/name/{name}` shows one plugin's
manifest and config. Both show the plugins as they were when the server
started.

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
runs exactly once, when the server starts; handlers fire later, each
time their event is emitted.

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
config is the `[config]` table in the plugin's manifest, with the keys of an
optional `config.json` file (a JSON object) in the plugin directory applied
over it. Keeping your settings in `config.json` leaves the
manifest's defaults untouched, so a new version of a plugin can be dropped in
without losing them. Each plugin sees only its own config. A plugin with no
config gets an empty table.

Tables (and JSON objects) become Lua tables keyed by string, and arrays become
sequences indexed from 1. TOML dates and times become strings in their
RFC 3339 form, and `inf` and `nan` are not allowed. A `null` in `config.json`
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

configured with, say, this `config.json`:

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
plugin's code fails to load, no plugins run until it is fixed (a plugin whose
*manifest* is invalid is merely skipped).

## Events

| Event            | Payload                                    | Purpose |
|------------------|--------------------------------------------|---------|
| `entry.parsed`   | entry table (see below)                    | Observe a newly-parsed entry, before any transformations. Return value is ignored. |
| `entry.ingest`   | entry table (see below)                    | **Transform or filter** a parsed entry. Return the (possibly modified) entry to keep it, or `nil` to drop it. |
| `fetch.success`  | `{ feed_id, status, url, content_length }` | Fires after a successful (2xx) feed fetch, once the response body has been read. |
| `fetch.error`    | `{ feed_id, kind, status, message, retry_after }` | Fires when a feed fetch fails. `kind` is one of `"http"`, `"timeout"`, `"network"`, `"too_many_redirects"`, `"body_too_large"`, `"parse"`, or `"fetcher"` (the isolated feed fetcher itself failed, e.g. its worker crashed while handling this feed). `status` and `retry_after` are populated only when available. |
| `feed.added`     | `{ id, url, title }`                       | Fires after a feed is created via the HTTP API. |
| `feed.removed`   | `{ id, url, title }`                       | Fires after a feed is deleted via the HTTP API. `id`, `url`, `title` reflect the feed's state immediately before deletion. |

Only `entry.ingest` is a **transform** event — its handlers can modify or
filter the payload. All other events are observe-only; their return values are
discarded.

### The entry table

The payload for `entry.parsed` and `entry.ingest` is a Lua table with the
following fields:

| Field               | Lua type          | Mutable |
|---------------------|-------------------|---------|
| `feed_id`           | integer           | No      |
| `syndication_format`| string (`"rss"` or `"atom"`) | No |
| `guid`              | string            | No      |
| `published_at`      | integer (Unix timestamp) or `nil` | Yes |
| `title`             | string            | Yes     |
| `url`               | string or `nil`   | Yes     |
| `content`           | string (HTML) or `nil` | Yes |
| `tags`              | array of strings  | Yes     |

`tags` starts empty. A non-empty `tags` replaces the entry's user tags.

Scripts may also add the system tags `system:read`, `system:saved`, and
`system:hidden` to `tags`. These are applied only when the entry is first
stored: after that an entry's system tags record what the user has done with
it (read it, saved it, hidden or unhidden it), and later refreshes leave them
alone. Scripts never remove system tags. Any other name starting with
`system:` is reserved, and is ignored with a warning.

`feed_id`, `syndication_format`, and `guid` are identity fields. Handlers may
read them, but any modifications are discarded when the entry is converted back
out of Lua.

### Handler chaining

Multiple handlers may be registered for the same event; they run in
registration order (which matches the order of their plugins' directory
names). For `entry.ingest`, the output of one
handler becomes the input to the next — if any handler returns `nil`, the
entry is dropped immediately and subsequent handlers do not run.

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
  control returns to the VM.
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
