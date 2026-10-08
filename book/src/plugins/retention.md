# Deleting old entries

Kiki keeps an entry for as long as its feed lists it. Once a refresh finds
that the feed no longer does, the entry stays where it is, so that you can
still read it, and Kiki notes when it dropped out of the feed.

The `retention` plugin deletes entries once their feed has stopped listing
them for a number of days. It is [installed by default](index.md), but
keeps every entry until you set how long to keep them:

```bash
echo 'max_age_days = 30' | kiki plugin config set retention
```

or set **Days to keep entries** on the plugin's page in the web UI, which
also lists the **Tags to keep**. It deletes entries when plugins load, and
then every hour.

| Setting | Default | Meaning |
| ------- | ------- | ------- |
| `max_age_days` | `0` | Delete an entry once its feed has stopped listing it for this many days, from 1 to 36500. `0` keeps entries forever. |
| `keep_tags` | `["system:saved"]` | Never delete entries tagged with any of these tags. An entry that loses its last such tag, such as one you unsave, is deleted by the next cleanup, if its feed stopped listing it long enough ago. `[]` keeps none. |

To keep entries you have tagged `keep` as well as saved ones:

```bash
echo 'keep_tags = ["system:saved", "keep"]' | kiki plugin config set retention
```

Entries still in their feed are never deleted, however old they are: they
would come right back, as new and unread entries, on the feed's next
refresh.

Deleting entries cannot be undone, so the plugin's manifest asks for the
`entries.delete` [permission](../writing-plugins.md#permissions), which
`kiki plugin ls` and the web UI show.

## Upgrading

Earlier releases of Kiki deleted old entries themselves, as the config
file's `[retention]` section set. The first time the server starts after an
upgrade, it moves `max_age_days` from `kiki.toml` into this plugin's config,
unless the plugin has a `max_age_days` of its own already, and removes it
from the file. `/v1/settings/retention` and `POST /v1/entries/cleanup` are
gone: set the plugin's config with `/v1/plugins/name/retention/config`
instead.

The plugin uses [timers](../writing-plugins.md#timers) and
[deletes entries](../writing-plugins.md#deleting-entries). It is written in
Rust, as a [WebAssembly plugin](../writing-plugins.md); see
[`plugins/retention/src/lib.rs`](https://github.com/kernelmethod/kiki-rss/blob/main/plugins/retention/src/lib.rs)
for the details. Versions before 2.0.0 were written in Lua, and took the same
configuration.
