# kiki-rss

`kiki` is an RSS/Atom feed _engine_. It is not a complete feed reader in and of
itself, but rather a reusable component that can be run behind the scenes to
power a reader.

## Example usage

Start a Kiki server with

```bash
cargo run --release -- init --auto
cargo run --release -- serve
```

The server prints the socket it is listening on, by default
`$XDG_RUNTIME_DIR/kiki/kiki.sock`.

You can add feeds to the server with e.g.

```bash
curl \
    --header 'Content-Type: application/json' \
    --data '{"title": "my feed", "url": "https://kernelmethod.org/notes/index.xml"}' \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds/create
```

You should then be able to see the server listed with

```bash
curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds

curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    http://localhost/v1/feeds/id/$id
```

The server will automatically populate its database with feed entries once
you've added some endpoints. However, you can manually trigger a feed fetch
with

```bash
curl \
    --unix-socket "$XDG_RUNTIME_DIR/kiki/kiki.sock" \
    --request POST \
    http://localhost/v1/feeds/fetch/$id
```

## Where Kiki keeps its files

| | Default |
| --- | --- |
| Database and cached assets | `$XDG_DATA_HOME/kiki` (`~/.local/share/kiki`) |
| Unix domain socket | `$XDG_RUNTIME_DIR/kiki/kiki.sock` (macOS: beside the database) |

Both are per-user, so one server runs per user with no configuration needed.

`$KIKI_HOME` moves all of it. Every subcommand honours it, and the socket
lives in the directory alongside the database:

```bash
export KIKI_HOME=/srv/kiki
kiki init && kiki serve   # /srv/kiki/kiki.db, /srv/kiki/kiki.sock
```

Serving from a directory that already holds a `kiki.db` uses that directory
the same way, so `kiki init . && kiki serve` keeps its socket at `./kiki.sock`.

To place the socket on its own, pass `--uds PATH` or set `$KIKI_SOCKET`;
`--port PORT` and `--bind ADDR` listen on TCP instead.
