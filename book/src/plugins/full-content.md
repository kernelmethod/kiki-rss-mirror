# Fetching full articles

Many feeds give only the first paragraph of each article, or nothing but
its title, and leave the rest to their website. The `full-content` plugin
fills such entries in: when an entry's content is short, it fetches the
page the entry links to, finds the article on it, and stores that as the
entry's content instead.

It is [installed by default](index.md), but does nothing until you let
plugins use the network, which Kiki doesn't by default. To turn it on, add
to `kiki.toml` in Kiki's [data directory](../configuration/environment.md):

```toml
[plugins]
allow_network = true
```

A running server picks the change up within a minute. From then on, each
entry with less than `min_length` characters of text (500 by default, not
counting spaces) is taken for a summary. Kiki fetches the page it links to
through the feed fetcher, as it fetches feeds, and through the same
[proxy](../configuration/settings.md#proxy), if you set one. The plugin looks
for the article in the first element on the page that one of its
`selectors` matches, and replaces the entry's content with it, with links
and images made absolute. The [`sanitize`](sanitize.md) and
[`privacy`](privacy.md) plugins then clean it as they clean any other
content.

An article is fetched once: on later refreshes of its feed, the plugin
keeps the article it stored, rather than fetching it again. If a page can't
be fetched, or holds nothing that looks like the article, the entry keeps
its summary, and the plugin tries again after six hours. Entries stored
before you turned it on are filled in the next time their feed is fetched
and has changed, if it still lists them.

Fetching an article takes a moment, while the entry is being stored, so a
feed whose entries all need fetching takes longer to refresh the first
time.

## Choosing feeds

To fetch articles for only some feeds, list them, by id or by URL, in
`feeds`; to never fetch them for some, list those in `exclude`:

```bash
echo 'feeds = [3, "https://example.com/feed.xml"]' | kiki plugin config set full-content
```

## Finding the article

The plugin tries `selectors`, CSS selectors such as `article` or
`.entry-content`, in order, and takes the first element that holds at
least `min_length` characters of text. If none does, it takes the one
holding the most, but only if that is at least twice as much as the
summary, and at least half of `min_length`: on a page that needs a script
to show its article, or keeps it for subscribers, what the selectors find
is often just the page's menus or a notice asking you to subscribe, which
would be no better than the summary. The defaults find the article on most
blogs and news sites. For a site where they find the wrong thing,
put a selector for its article first:

```bash
kiki plugin config get full-content --defaults > full-content.toml
# Edit `selectors` in full-content.toml, then:
kiki plugin config set full-content full-content.toml
```

| Setting | Default | Meaning |
| ------- | ------- | ------- |
| `feeds` | `[]` | Only fill in these feeds' entries, each given by its id or the URL it is fetched from. Empty means every feed. |
| `exclude` | `[]` | Never fill in these feeds' entries. |
| `min_length` | `500` | An entry with fewer characters of text than this, not counting whitespace, is taken for a summary. |
| `selectors` | see `manifest.toml` | Where to look for the article on its page, in order; at most 64. |

## What to know before turning it on

Turning on `allow_network` lets any plugin whose manifest asks for the
[`network` permission](../writing-plugins.md#permissions) fetch web pages,
not only this one; `kiki plugin ls` shows which plugins ask for it. The
pages are fetched from whatever URL an entry links to, which the feed
chose, so a feed can have Kiki fetch a page from any server Kiki can
reach, your local network included, as it already can by linking images.
See the [threat model](../threat-model.md#a-misbehaving-plugin).

To turn the plugin off but leave network access on for other plugins,
delete `plugins/system/full-content` from Kiki's data directory, or set
`enabled = false` in its `manifest.toml`. Either way, `kiki init --check`
leaves it as you left it.

The plugin is written in Rust, as a [WebAssembly plugin](../writing-plugins.md),
using [`fetch-page`](../writing-plugins.md#fetching-web-pages); see
[`plugins/full-content/src/lib.rs`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/full-content/src/lib.rs)
for the details.
