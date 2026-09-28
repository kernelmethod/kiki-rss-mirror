# Writing scripts for Kiki

Kiki has a Lua scripting engine that user-supplied scripts use to hook into
server events — transforming incoming entries, reacting to feed lifecycle
changes, or logging when something interesting happens.

## The `kiki` global

Every script runs inside a sandboxed Lua 5.4 VM with a restricted standard
library (`string`, `table`, `math`, and the safe subset of `os`). On top of
that, kiki exposes a single additional global — the `kiki` table:

- `kiki.on(event_name, handler)` — register `handler` to fire every time
  `event_name` is emitted.
- `kiki.log(level, message)` — write `message` to the server log at the given
  level. `level` must be one of `"debug"`, `"info"`, `"warn"`, or `"error"`.

The sandbox removes `require`, `dofile`, `loadfile`, `debug`, `io`, `package`,
and the destructive `os.*` calls (`execute`, `exit`, `getenv`, `remove`,
`rename`, `tmpname`). Scripts cannot read from disk, make network requests, or
spawn processes.

## Script structure

The recommended shape for a script is to register one or more event handlers
via `kiki.on` at the top level. The script's top-level chunk runs exactly once
when kiki loads its scripts; handlers fire later, each time their event is
emitted.

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

`tags` holds the entry's user tags. Tag names starting with `system:` are
reserved for system tags (such as `system:read`); scripts cannot set them, and
any such names are ignored with a warning. An entry's system tags are never
affected by scripts.

`feed_id`, `syndication_format`, and `guid` are identity fields. Handlers may
read them, but any modifications are discarded when the entry is converted back
out of Lua.

### Handler chaining

Multiple handlers may be registered for the same event; they run in
registration order (which matches the order their owning scripts were
inserted into the `scripts` table). For `entry.ingest`, the output of one
handler becomes the input to the next — if any handler returns `nil`, the
entry is dropped immediately and subsequent handlers do not run.

## Resource limits

Every handler call runs under two hard limits:

- **Time**: 100 ms per invocation. Enforced by a Lua debug hook that fires
  every 1000 VM instructions. Scripts stuck in long-running C-level calls
  (e.g. pathological `string.gsub` patterns) can exceed this slightly before
  control returns to the VM.
- **Memory**: 16 MiB across the entire VM. Allocations that would exceed this
  cap fail the handler.

When a handler errors, times out, or exceeds the memory cap:

- For `entry.ingest`, the entry passes through that handler **unmodified** —
  a broken script will never silently drop entries.
- For observe events, the failure is logged and dropped.

## Managing scripts

Scripts are stored in the `scripts` table and managed via the
`/v1/scripts/*` HTTP API. Changes take effect immediately: the server
rebuilds its scripting VM on every script create / update / delete, replacing
the old runner atomically so in-flight events always see a consistent handler
set.
