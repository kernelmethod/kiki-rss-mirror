# kiki-rss

`kiki` is an RSS/Atom feed _engine_. It is not a complete feed reader in and of
itself, but rather a reusable component that can be run behind the scenes to
power a reader.

## Example usage

Start a Kiki server with

```bash
cargo run --release -- init
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

Two environment variables name these directly. Each *is* the directory —
neither gets a `kiki/` subdirectory appended, unlike the shared platform
locations they replace:

| | Names |
| --- | --- |
| `$KIKI_HOME` | Kiki's home: the database and cached assets |
| `$KIKI_RUNTIME_DIR` | Kiki's runtime directory: the socket |

`$KIKI_HOME` alone moves the whole instance, socket included — a directory
Kiki was pointed at keeps the socket too:

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

Set both and each goes where it was told. `kiki init` takes no arguments —
it always sets up `$KIKI_HOME` when that is set, and the platform data
directory otherwise.

Serving from a directory that already holds a `kiki.db` uses that directory
the same way, so `cd`-ing into one and running `kiki serve` keeps its socket
at `./kiki.sock` unless `$KIKI_RUNTIME_DIR` says otherwise.

To pin the socket to an exact path rather than a directory, pass
`--uds PATH` or set `$KIKI_SOCKET`; both beat `$KIKI_RUNTIME_DIR`.

Kiki serves over a Unix socket and nothing else — it does not listen on
TCP. A socket is reachable only by processes that can reach its path, which
is access control Kiki does not have to implement or authenticate. To expose
it over the network, put a reverse proxy in front of the socket and let that
own the TLS and authentication the job needs; nginx spells it
`proxy_pass http://unix:/run/user/1000/kiki/kiki.sock:;`.
