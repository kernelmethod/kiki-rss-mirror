# Environment

`kiki init` sets up a per-user data directory, so one server runs per user
with no configuration:

| Path                              | Holds                                                          |
| --------------------------------- | -------------------------------------------------------------- |
| `$XDG_DATA_HOME/kiki` (`~/.local/share/kiki`) | The data directory, below                          |
| `$XDG_RUNTIME_DIR/kiki/kiki.sock` | The Unix socket (on macOS, in the data directory)              |

The data directory holds `kiki.db` (the database), `kiki.toml` (the
[settings](settings.md) that differ from the defaults), `plugins/` (the
installed [plugins](../plugins/index.md): in `system/` if bundled with Kiki,
in `user/` if you installed them) and `assets/` (cached images and
enclosures).

## Environment variables

| Variable           | Effect                                                                 |
| ------------------ | ---------------------------------------------------------------------- |
| `KIKI_HOME`        | The data directory. The socket moves with it unless `KIKI_RUNTIME_DIR` is set. |
| `KIKI_RUNTIME_DIR` | The directory holding the socket.                                      |
| `KIKI_SOCKET`      | The socket's exact path, as `--uds` does. Beats `KIKI_RUNTIME_DIR`.    |
| `KIKI_PROXY`       | Overrides [`proxy.url`](settings.md#proxy).                            |
| `KIKI_NO_PROXY`    | Overrides [`proxy.no_proxy`](settings.md#proxy).                       |
| `RUST_LOG`         | What is logged, such as `warn` or `kiki_rss::process=debug,info`. Defaults to `info`. |

`KIKI_HOME` and `KIKI_RUNTIME_DIR` name the directories themselves: no
`kiki/` subdirectory is appended. Running `kiki serve` in a directory that
already holds a `kiki.db` serves that directory, as if `KIKI_HOME` named it.

```bash
export KIKI_HOME=/srv/kiki
kiki init && kiki serve   # /srv/kiki/kiki.db, /srv/kiki/kiki.sock
```
