# Plugins

Plugins are small Lua programs that Kiki runs as entries arrive: to hide
them, tag them, clean them up, or anything else you can write. Each lives in
its own directory under `plugins/` in Kiki's [data directory](../files.md).

## Plugins installed by default

`kiki init` installs three plugins, bundled into the `kiki` binary:

| Plugin | What it does |
| ------ | ------------ |
| [`filter`](filter.md) | Hides entries matching rules you set. |
| [`auto-tag`](auto-tag.md) | Tags entries matching rules you set. |
| [`strip-tracking`](strip-tracking.md) | Removes tracking parameters from links, and tracking pixels from content. |

`filter` and `auto-tag` do nothing until you give them rules.
`strip-tracking` works out of the box.

Pass `kiki init --no-default-plugins` to skip them. `kiki init --check`,
which the packaged services run before every start, installs default plugins
that are new in this release and updates the ones an earlier release
installed, unless you've edited their files. A default plugin you delete
isn't reinstalled; `plugins/.default-plugins.toml` records which were
installed.

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
installing a plugin is a matter of copying its directory into place.

To write your own, see [Writing plugins](../writing-plugins.md).
