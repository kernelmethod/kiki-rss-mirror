# Stripping tracking parameters

The `privacy` plugin cleans up new entries as they arrive:

- It removes tracking parameters, such as `utm_source`, `fbclid` and
  `gclid`, from the query strings of entries' URLs and of the links in their
  content. It also cleans query-like fragments, such as `#xtor=RSS-1`.
- It removes tracking pixels from entries' content: images declared 1×1 or
  smaller, and images from known trackers, such as WordPress.com's stats and
  FeedBurner, so that Kiki never downloads them.

It is [installed by default](index.md) and works without any
configuration. It only cleans entries as they are downloaded, not the ones
already stored.

## Changing what it strips

To strip other parameters or trackers, or to leave entries' content alone,
start from its defaults:

```bash
kiki plugin config get privacy --defaults > privacy.toml
# edit privacy.toml: add names to `params` ("prefix_*" matches a prefix)
# or image sources to `trackers` ("*.example.com", "example.com/pixel"),
# or set `content = false` or `pixels = false`
kiki plugin config set privacy privacy.toml
```

## Not downloading assets at all

To keep Kiki from downloading any images or enclosures for some feeds, so
that the sites serving them never hear from it, list the feeds, by id or by
URL, in `skip_assets`:

```bash
echo 'skip_assets = [3, "https://example.com/feed.xml"]' | kiki plugin config set privacy
```

See [`plugins/privacy/main.lua`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/privacy/main.lua)
for the details.
