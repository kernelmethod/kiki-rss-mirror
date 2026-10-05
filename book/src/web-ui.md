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

## Logging in

By default the web UI has no login: anyone who can reach it can read and
change everything in Kiki. To require a login, start it with
`--require-login`, or set it in `kiki.toml`:

```toml
[web_ui]
require_login = true
```

The web UI then asks for an [API token](tokens.md) before showing anything,
and acts with that token's scopes: someone who logged in with a `reader`
token can read entries and mark them read or saved, but sees no plugin
pages and can't delete tags. Create a token for each person or device:

```bash
kiki token create laptop --scopes reader
```

Sessions last 30 days, or until you log out, the token is revoked or
expires, or the web UI restarts. The session cookie is `HttpOnly` and
`SameSite=Strict`, and is marked `Secure` when a reverse proxy in front of
the web UI reports HTTPS in `X-Forwarded-Proto`.

The login form sends the token over whatever connection the browser has to
the web UI, so beyond your own machine, serve the web UI over HTTPS through
a reverse proxy.

## Reaching it by another name

The web UI only answers requests whose `Host` header names `localhost`,
`127.0.0.1` or `::1`. That way a website can't reach it by pointing its own
domain at your machine (DNS rebinding).

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

Unless it [requires a login](#logging-in), anyone who can reach the web UI
can read and change everything in Kiki. If you make it reachable beyond
your own machine, require a login, or put it behind a reverse proxy that
handles authentication.
