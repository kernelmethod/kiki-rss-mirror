# Backing off from feeds that rarely change

Some servers ask for their feed to be fetched far more often than it
changes: `Cache-Control: max-age=0` is common. Kiki follows such a hint
down to `min_polling_cadence_seconds` (one minute by default), so a feed
that changes once a week can be fetched every minute.

The `adaptive-fetch` plugin backs off from such a feed while it keeps
turning out unchanged. Each fetch that finds it unchanged (a
`304 Not Modified`, or the same body again) doubles the wait, and each one
that finds it changed halves it. The wait settles near how often the feed
really changes, and it always stays between the wait the server asked for
and the feed's own fetch interval, so a feed is never fetched less often
than its interval.

It is [installed by default](index.md) and works without any
configuration. Feeds whose server sends no such hint already wait their full
interval, so the plugin leaves them alone. Each feed's progress is kept in
the plugin's store, so it survives restarts.

## Choosing feeds

To fetch some feeds as often as their server asks, list them, by id or by
URL, in `exclude`:

```bash
echo 'exclude = [3, "https://example.com/feed.xml"]' | kiki plugin config set adaptive-fetch
```

To back off from only some feeds, list them in `feeds` instead; with `feeds`
empty, every feed is backed off from.

To turn adaptive fetching off altogether, delete `plugins/system/adaptive-fetch`
from Kiki's [data directory](../configuration/environment.md), or set `enabled = false` in its
`manifest.toml`. Either way, `kiki init --check` leaves it as you left it.

The plugin uses the [`fetch.schedule`](../writing-plugins.md#scheduling-fetches)
event; see
[`plugins/adaptive-fetch/main.lua`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/adaptive-fetch/main.lua)
for the details.
