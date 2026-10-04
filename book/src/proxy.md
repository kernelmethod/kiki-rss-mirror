# Proxies

Kiki can send feed fetches and asset downloads through an HTTP(S) or SOCKS5
proxy. Set it in `kiki.toml`:

```toml
[proxy]
url = "http://user:password@proxy.example:3128"
no_proxy = "localhost, .internal.example, 10.0.0.0/8"   # optional
```

or with environment variables, which take precedence over the file:

| Variable         | Overrides        |
| ---------------- | ---------------- |
| `$KIKI_PROXY`    | `proxy.url`      |
| `$KIKI_NO_PROXY` | `proxy.no_proxy` |

`no_proxy` is a comma-separated list of hosts to reach directly: domains
(which include their subdomains), IP addresses, CIDR ranges, or `*`.

When no proxy URL is set either way, Kiki honours the conventional
`$HTTPS_PROXY`, `$HTTP_PROXY`, `$ALL_PROXY` and `$NO_PROXY` instead.

Changes to the file take effect without a restart. An invalid `$KIKI_PROXY`
stops `kiki serve` at startup.

## Using Tor

A `socks5h://` URL sends everything through a SOCKS5 proxy, host name
lookups included, so it is the one to use with Tor:

```toml
[proxy]
url = "socks5h://127.0.0.1:9050"
```

With `socks5://`, Kiki looks host names up itself, and those DNS queries go
out directly, telling whoever can see them which sites Kiki fetches from.

To keep Kiki from downloading images and enclosures at all for some feeds,
see `skip_assets` in the [strip-tracking plugin](plugins/strip-tracking.md).
