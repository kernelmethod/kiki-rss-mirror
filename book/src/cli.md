# Command line

Every command prints its full set of options with `--help`, e.g.
`kiki serve --help`.

| Command | What it does |
| ------- | ------------ |
| `kiki init` | Sets up the data directory: database, config and [default plugins](plugins/index.md). `--check` only does what's missing, and syncs the default plugins; `--force` starts over, deleting the database; `--no-default-plugins` skips the plugins. |
| `kiki serve` | Starts the server. `--uds PATH` picks the socket; `--api-listen ADDR` also serves the API over TCP, [with tokens](tokens.md#serving-the-api-over-tcp); see [Sandboxing](deployment.md#sandboxing) for `--no-sandbox` and `--seccomp-log-only`. |
| `kiki web` | Starts the server along with [the web UI](web-ui.md). `--listen ADDR` (default `127.0.0.1:8080`), `--allowed-host HOST`, `--require-login`, and `kiki serve`'s options. |
| `kiki migrate` | Migrates the database to the current schema; see [Upgrading](deployment.md#upgrading). `--dry-run`, `--no-backup`. |
| `kiki opml import FILE` | Imports feeds from OPML; `-` reads from stdin. |
| `kiki opml export` | Exports feeds as OPML; `-o FILE` writes to a file. |
| `kiki plugin ls` | Lists installed plugins. |
| `kiki plugin config get NAME` | Prints a plugin's config as TOML. `--defaults` or `--overrides` for just one part. |
| `kiki plugin config set NAME [FILE]` | Overrides a plugin's config from TOML, read from `FILE` or stdin. `--replace` drops overrides not given. |
| `kiki token create NAME --scopes SCOPES` | Creates an [API token](tokens.md) and prints it. `--expires 90d` makes it expire. |
| `kiki token ls` | Lists API tokens. |
| `kiki token revoke TOKEN` | Revokes an API token, by id or name. |
| `kiki systemd install` | Installs a systemd user service; see [Running as a service](deployment.md). `--enable` also enables it. |
| `kiki systemd status` | Shows the user service's status. |
| `kiki systemd uninstall` | Removes it. `--keep-data` or `--remove-data`. |
| `kiki docs` | Writes the API reference as a standalone HTML page; `-o FILE` writes to a file. |
| `kiki version` | Prints Kiki's version. |
