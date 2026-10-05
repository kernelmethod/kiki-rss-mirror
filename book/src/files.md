# Where Kiki keeps its files

| What                          | Default                                                         |
| ----------------------------- | --------------------------------------------------------------- |
| Database, settings, plugins and cached assets | `$XDG_DATA_HOME/kiki` (`~/.local/share/kiki`) |
| Unix domain socket            | `$XDG_RUNTIME_DIR/kiki/kiki.sock` (macOS: beside the database)  |

Both are per-user, so one server runs per user with no configuration needed.

Inside the data directory:

| Path         | Holds                                                                 |
| ------------ | --------------------------------------------------------------------- |
| `kiki.db`    | The SQLite database: feeds, entries, tags and plugin state.            |
| `kiki.toml`  | [Settings](configuration.md) that differ from the defaults.            |
| `plugins/`   | Installed [plugins](plugins/index.md), one directory each: in `system/` if bundled with Kiki, in `user/` if you installed them. |
| `assets/`    | Cached images and enclosures; see `asset_cache` in [Settings](settings.md). |

## Moving things around

Two environment variables name these locations directly. Each _is_ the
directory: neither gets a `kiki/` subdirectory appended, unlike the shared
platform locations they replace.

| Variable            | Names                                                  |
| ------------------- | ------------------------------------------------------ |
| `$KIKI_HOME`        | Kiki's home: the database, settings, and cached assets |
| `$KIKI_RUNTIME_DIR` | Kiki's runtime directory: the socket                   |

`$KIKI_HOME` alone moves the whole instance, socket included: a directory
Kiki was pointed at keeps the socket too.

```bash
export KIKI_HOME=/srv/kiki
kiki init && kiki serve   # /srv/kiki/kiki.db, /srv/kiki/kiki.sock
```

`$KIKI_RUNTIME_DIR` alone splits the socket back out, wherever the database
happens to live:

```bash
export KIKI_RUNTIME_DIR=/run/kiki
kiki serve   # ~/.local/share/kiki/kiki.db, /run/kiki/kiki.sock
```

Set both and each goes where it was told. `kiki init` takes no arguments: it
always sets up `$KIKI_HOME` when that is set, and the platform data
directory otherwise.

Serving from a directory that already holds a `kiki.db` uses that directory
the same way, so `cd`-ing into one and running `kiki serve` keeps its socket
at `./kiki.sock` unless `$KIKI_RUNTIME_DIR` says otherwise.

To pin the socket to an exact path rather than a directory, pass
`--uds PATH` or set `$KIKI_SOCKET`; both beat `$KIKI_RUNTIME_DIR`.
