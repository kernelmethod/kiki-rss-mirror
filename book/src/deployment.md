# Running as a service

## As your own user

On Linux with systemd, `kiki systemd install` installs a user service that
starts `kiki serve` for you:

```bash
kiki systemd install --enable
loginctl enable-linger "$USER"   # keep Kiki running while you're logged out
```

A user service stops when your last session ends, and the socket in
`$XDG_RUNTIME_DIR` goes with it, unless lingering is enabled as above.
`kiki systemd uninstall` removes the service again, and asks whether to
remove Kiki's data too (`--keep-data` or `--remove-data` to skip the
question).

## As a system service

The Debian, RPM and Arch packages install `kiki.service`, which runs Kiki as
the `kiki` user with its data in `/var/lib/kiki` and its socket at
`/run/kiki/kiki.sock`:

```bash
sudo systemctl enable --now kiki.service
```

To talk to it, add yourself to the `kiki` group, or run clients as the
`kiki` user. On NixOS, the [NixOS module](installation.md#nixos) sets up the
same thing.

Before starting the server, the service runs `kiki init --check`, which sets
up the data directory on first run and installs or updates the
[default plugins](plugins/index.md).

## Upgrading

When a new release changes the database schema, `kiki serve` refuses to
start until the database is migrated. Stop the service, then run the
migrations as the user Kiki runs as:

```bash
sudo -u kiki env KIKI_HOME=/var/lib/kiki kiki migrate --dry-run   # list them
sudo -u kiki env KIKI_HOME=/var/lib/kiki kiki migrate
```

`kiki migrate` backs up the database first, so it needs about as much free
space as the database takes; `--no-backup` skips the backup.

## Sandboxing

On Linux, `kiki serve` sandboxes itself as it starts, with Landlock limiting
the files it can reach and seccomp limiting its system calls. It also fetches
and parses feeds, and runs plugins, in separate processes with sandboxes of
their own and no access to the database. The packaged systemd units add
systemd's own hardening on top.

If the sandbox is demonstrably what breaks Kiki on your system,
`kiki serve --seccomp-log-only` logs system call violations instead of
killing the process, and `--no-sandbox` turns the sandbox off altogether.
Please report the problem if you need either.

## Exposing Kiki over the network

Kiki serves the API on a Unix socket, which is reachable only by processes
that can reach its path. Requests on the socket need no token, so never
expose the socket itself, through a reverse proxy or otherwise.

To reach the API from other machines, serve it over TCP with
`--api-listen`, where every request needs an [API token](tokens.md), and
put a reverse proxy in front that terminates TLS, since the listener itself
speaks plain HTTP:

```bash
kiki serve --api-listen 127.0.0.1:8081
```

With nginx:

```nginx
location / {
    proxy_pass http://127.0.0.1:8081;
}
```

or with Caddy:

```caddy
reverse_proxy 127.0.0.1:8081
```

To reach the web UI from other machines, run `kiki web` with
[`--require-login`](web-ui.md#logging-in) behind the same kind of proxy,
pointed at the web UI's `--listen` address.

## Monitoring

The server answers `GET /v1/health`, and serves Prometheus metrics at
`/metrics`. Over the TCP listener, scraping the metrics takes a token with
the `metrics` scope (`kiki token create prometheus --scopes metrics`),
which Prometheus sends with `authorization: { credentials: … }` in its
scrape config. Under systemd, it also pings the service watchdog while it can
still fetch feeds, so a server that hangs is restarted.
