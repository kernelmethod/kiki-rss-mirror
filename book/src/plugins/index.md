# Plugins

Plugins are small Lua programs that Kiki runs as entries arrive: to hide
them, tag them, clean them up, or anything else you can write. Each lives in
its own directory under `plugins/` in Kiki's [data directory](../files.md):
in `plugins/system/` if it came with Kiki, and in `plugins/user/` if you
installed it.

## Plugins installed by default

`kiki init` installs five plugins, bundled into the `kiki` binary, into
`plugins/system/`:

| Plugin | What it does |
| ------ | ------------ |
| [`filter`](filter.md) | Hides entries matching rules you set. |
| [`auto-tag`](auto-tag.md) | Tags entries matching rules you set. |
| [`sanitize`](sanitize.md) | Removes scripts, styles, embedded content and unsafe links from entries' HTML. |
| [`strip-tracking`](strip-tracking.md) | Removes tracking parameters from links, and tracking pixels from content. |
| [`adaptive-fetch`](adaptive-fetch.md) | Fetches feeds less often while they keep turning out unchanged. |

`filter` and `auto-tag` do nothing until you give them rules.
`sanitize`, `strip-tracking` and `adaptive-fetch` work out of the box.

Pass `kiki init --no-default-plugins` to skip them. `kiki init --check`,
which the packaged services run before every start, installs default plugins
that are new in this release and updates the ones an earlier release
installed, unless you've edited their files. A default plugin you delete
isn't reinstalled; `plugins/system/.default-plugins.toml` records which
were installed.

The plugins in `plugins/system/` are *system* plugins, and the ones in
`plugins/user/` are *user* plugins. Both kinds load, run and take config the
same way; the only difference is that Kiki looks after system plugins, while
user plugins are yours to install, update and remove. `kiki plugin ls`, the
web UI and the API (the `source` field of a plugin, `"system"` or `"user"`)
all show which kind each plugin is.

## Managing plugins

To see the installed plugins, and to read or change a plugin's config as
TOML:

```bash
kiki plugin ls
kiki plugin config get filter              # --defaults or --overrides for just one part
echo 'patterns = ["sponsored", "webinar"]' | kiki plugin config set my-plugin
kiki plugin config set filter overrides.toml --replace
```

`config set` overrides the top-level keys it is given and keeps the plugin's
other overrides, unless `--replace` is passed. A plugin's config overrides
are kept in the database, not in its files, so changing them doesn't count
as editing the plugin.

A running server reloads its plugins with the new config straight away. It
also reloads them whenever a file in the plugins directory changes, so
installing a plugin is a matter of copying its directory into
`plugins/user/`.

To write your own, see [Writing plugins](../writing-plugins.md).
