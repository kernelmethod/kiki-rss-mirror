# Installation

Kiki is a single binary, `kiki`. It runs on Linux and macOS; on Linux it
also sandboxes itself with Landlock and seccomp.

## Prebuilt binary

Each [GitHub release](https://github.com/kernelmethod/kiki-rss/releases)
includes a statically linked binary for x86-64 Linux, which runs on any
distribution:

```bash
tar --zstd -xf kiki-x86_64-linux-musl.tar.zst
sudo install -m 0755 kiki /usr/local/bin/kiki
kiki version
```

## Distribution packages

The repository holds packaging for Debian and Ubuntu (`cargo deb`), Fedora
and other RPM-based distributions (`cargo generate-rpm`), and Arch Linux
(`dist/PKGBUILD`). Each package installs the binary to `/usr/bin/kiki` and
a hardened `kiki.service` system unit, creates a `kiki` user, and sets up
its data directory in `/var/lib/kiki`. See
[Running as a service](deployment.md) for what happens next.

```bash
cargo build --release
cargo deb --no-build                # target/debian/kiki-rss_*.deb
cargo generate-rpm                  # target/generate-rpm/kiki-rss-*.rpm
(cd dist && makepkg)                # kiki-*.pkg.tar.zst
```

## NixOS

The flake provides a NixOS module that runs Kiki as a system service:

```nix
{
  inputs.kiki.url = "github:kernelmethod/kiki-rss";

  outputs = { nixpkgs, kiki, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        kiki.nixosModules.default
        {
          services.kiki.enable = true;
          # Optional; these are the defaults.
          services.kiki.dataDir = "/var/lib/kiki";
          services.kiki.unixSocket = "/run/kiki/kiki.sock";
        }
      ];
    };
  };
}
```

To run Kiki without installing it, use `nix run github:kernelmethod/kiki-rss
-- serve`.

## From source

Kiki builds with a recent stable Rust toolchain, with the
`wasm32-unknown-unknown` target for the [filter](plugins/filter.md) plugin,
which the build compiles to WebAssembly:

```bash
rustup target add wasm32-unknown-unknown
cargo install --locked --git https://github.com/kernelmethod/kiki-rss kiki-rss
```

### Cargo features

All of these are on by default. Turn them off with
`--no-default-features` and pick the ones you want with `--features`.

| Feature           | What it adds                                                         |
| ----------------- | -------------------------------------------------------------------- |
| `cli`             | The `kiki` command-line program itself.                              |
| `systemd`         | `kiki systemd`, to install a systemd user service.                   |
| `api-docs`        | The interactive API reference at `/docs`, and `kiki docs`.           |
| `metrics`         | Prometheus metrics at `/metrics`.                                    |
| `web-ui`          | `kiki web`.                                                          |
| `default-plugins` | The plugins `kiki init` installs (`filter`, `auto-tag`, `sanitize`, `privacy`, `adaptive-fetch`). |
