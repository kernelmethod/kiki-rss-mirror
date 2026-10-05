# Getting started

This walks through running Kiki as your own user, adding a feed, and reading
its entries. To run Kiki as a system service instead, see
[Running as a service](deployment.md).

## Start the server

Set up Kiki's database, config and default plugins, then start the server:

```bash
kiki init
kiki serve
```

The server prints the socket it is listening on, by default
`$XDG_RUNTIME_DIR/kiki/kiki.sock`. Everything else talks to Kiki through that
socket. The examples below keep its path in a variable:

```bash
sock="$XDG_RUNTIME_DIR/kiki/kiki.sock"
```

## Add a feed

```bash
curl \
    --header 'Content-Type: application/json' \
    --data '{"title": "my feed", "url": "https://kernelmethod.org/notes/index.xml"}' \
    --unix-socket "$sock" \
    http://localhost/v1/feeds/create
```

Kiki starts fetching the feed straight away, and from then on fetches it on
its own schedule. List your feeds, or look at one of them, with

```bash
curl --unix-socket "$sock" http://localhost/v1/feeds
curl --unix-socket "$sock" http://localhost/v1/feeds/id/$id
```

To fetch a feed again now, rather than waiting for its next turn:

```bash
curl --unix-socket "$sock" --request POST http://localhost/v1/feeds/refresh/$id
```

## Read entries

```bash
curl --unix-socket "$sock" http://localhost/v1/entries
```

or, for a friendlier view, start [the web UI](web-ui.md) instead of
`kiki serve`, and open <http://localhost:8080>:

```bash
kiki web
```

## Bring your feeds from another reader

Most feed readers can export their subscriptions as OPML. Import them with

```bash
kiki opml import subscriptions.opml   # or `-` to read from stdin
```

Folders in the OPML file become tags. Feeds that are already present are
skipped, though they gain the tags of the folders they're in. To go the
other way:

```bash
kiki opml export -o subscriptions.opml
```

Both commands work on the same database `kiki serve` uses, and a running
server starts fetching imported feeds within a few seconds.

## Next steps

- Hide the entries you don't want with the [filter plugin](plugins/filter.md),
  and organise the rest with [automatic tags](plugins/auto-tag.md).
- Change how often feeds are fetched, how long entries are kept, and more in
  [Settings](configuration/settings.md).
- See every endpoint in the [interactive API reference](../api/).
