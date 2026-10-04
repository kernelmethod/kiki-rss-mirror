# The web UI

`kiki web` runs the Kiki server together with a small web UI for reading
feeds in a browser:

```bash
kiki web                          # http://localhost:8080
kiki web --listen 0.0.0.0:8080    # listen on every interface
```

It starts its own server exactly as `kiki serve` would, on the same socket,
so use it in place of `kiki serve` rather than alongside it. It takes all of
`kiki serve`'s options too.

The browser never talks to the Kiki API directly: the web UI calls the API
over the server's socket and renders what it gets back.

## Reaching it by another name

The web UI has no login, so it only answers requests whose `Host` header
names `localhost`, `127.0.0.1` or `::1`. That way a website can't reach it
by pointing its own domain at your machine (DNS rebinding).

To reach the web UI by another name, such as a LAN host name or address,
allow that name in `kiki.toml`:

```toml
[web_ui]
allowed_hosts = ["kiki.lan", "192.168.1.5", "*.home.example"]
```

or with `--allowed-host`, which can be given more than once and adds to the
file's list. `*.home.example` allows every subdomain of `home.example`, and
`*` allows any host at all, which turns the check off.

`kiki web` reads the file as it starts, so restart it after changing the
list.

Anyone who can reach the web UI can read and change everything in Kiki. If
you make it reachable beyond your own machine, put it behind a reverse proxy
that handles authentication.
