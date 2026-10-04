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
systemd's own hardening on top. The [threat model](threat-model.md) describes
each process and what its sandbox allows.

If the sandbox is demonstrably what breaks Kiki on your system,
`kiki serve --seccomp-log-only` logs system call violations instead of
killing the process, and `--no-sandbox` turns the sandbox off altogether.
Please report the problem if you need either.

## Exposing Kiki over the network

Kiki serves over a Unix socket and nothing else; it does not listen on TCP.
A socket is reachable only by processes that can reach its path, which is
access control Kiki does not have to implement.

To reach Kiki from other machines, put a reverse proxy in front of the
socket and let it handle TLS and authentication. With nginx:

```nginx
location / {
    proxy_pass http://unix:/run/kiki/kiki.sock:;
}
```

or with Caddy:

```caddy
reverse_proxy unix//run/kiki/kiki.sock
```

Remember that the API has no authentication of its own: anyone who can
reach it can read and change everything. See [Security](api.md#security).

## Monitoring

The server answers `GET /v1/health`, and serves Prometheus metrics at
`/metrics`. Under systemd, it also pings the service watchdog while it can
still fetch feeds, so a server that hangs is restarted.
