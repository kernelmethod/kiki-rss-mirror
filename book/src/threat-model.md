# Threat model

Kiki spends its day fetching documents from servers it has no reason to
trust, parsing them, and running plugins over what it finds. This page sets
out what Kiki tries to protect, who it trusts and who it doesn't, and how it
splits its work across sandboxed processes so that a bug in the riskiest
code doesn't hand an attacker everything else.

The sandboxing described here is Linux-only. Elsewhere, Kiki still fetches
feeds and runs plugins in separate processes, but nothing stops those
processes from doing anything your user account can do; see
[Limits](#limits).

## What Kiki protects

- **Your data**: the feeds, entries, tags and plugin state in `kiki.db`,
  your settings in `kiki.toml`, and the cached assets, all in the
  [data directory](configuration/environment.md).
- **Secrets**: the credentials of feeds that need a login, a proxy's
  password, if it has one, and [API tokens](tokens.md). Kiki stores only a
  hash of each token; the web UI holds the tokens of the people logged in
  to it in memory, for as long as their sessions last.
- **The rest of your account**: your other files, the other processes you
  run, and the machines on your network.
- **Kiki itself**: a hostile feed or plugin shouldn't be able to stop Kiki
  from fetching every other feed.

## Who Kiki trusts

| Trusted | Partly trusted | Untrusted |
| --- | --- | --- |
| You, and anyone else who can reach the API socket, within what [anonymous access](tokens.md#anonymous-access) or their token allows | Plugins | Feed servers, and every byte they send: feeds, web pages, images, SVG, redirects, headers |
| The kiki executable, and the kernel | | DNS answers |
| | | The network between Kiki and a feed server |
| | | Websites open in your browser while `kiki web` runs |

**By default, the API trusts anyone who can reach it.** A request that
carries an [API token](tokens.md) may do only what the token's scopes
allow, but a request without one may do anything, unless
[`anonymous_access`](tokens.md#anonymous-access) under `[api]` limits it to
reading (`"read-only"`) or to nothing (`"token-required"`). Who can reach
the socket at all is left to the filesystem: only those who can reach its
path, and a directory Kiki creates for it is readable by your user alone. See [Security](api.md#security) and
[Exposing Kiki over the network](deployment.md#exposing-kiki-over-the-network).

**Plugins** are installed by you, so Kiki doesn't treat them as attackers.
They still run in a process of their own with nothing to reach but the
server, because a plugin is code that runs over text a feed wrote, inside a
Lua VM written in C or as WebAssembly compiled to native code. A buggy plugin, or a feed that subverts one, shouldn't
put the database within reach.

## Processes

A single process would need the union of everything Kiki does: write the
database, listen for API requests, connect to any server on the internet,
and parse whatever it sends back. Kiki instead runs several processes, each
with a sandbox fitted to its own job:

<figure class="diagram-figure">
<svg class="diagram" viewBox="0 0 800 800" role="img" aria-labelledby="pm-title pm-desc" xmlns="http://www.w3.org/2000/svg">
  <title id="pm-title">Kiki's process model</title>
  <desc id="pm-desc">The server owns the data directory and listens on the API's Unix socket, which the web UI and other API clients connect to. The server starts the script host and the feed fetcher's supervisor, and talks to each over a socket pair. The supervisor starts the worker, the parser and the resolver, and relays messages between them and the server. Only the worker and the resolver reach the internet.</desc>
  <defs>
    <marker id="pm-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
      <path class="arrowhead" d="M0,0 L10,5 L0,10 z"/>
    </marker>
  </defs>
  <!-- Clients -->
  <rect class="external" x="20" y="20" width="110" height="90" rx="6"/>
  <text class="title" x="36" y="44">Browser</text>
  <rect class="proc" x="180" y="20" width="230" height="90" rx="6"/>
  <text class="title" x="196" y="44">web UI (kiki web)</text>
  <text x="196" y="64">files: none</text>
  <text x="196" y="80">accepts on its TCP listener</text>
  <text x="196" y="96">connects: Unix sockets only</text>
  <rect class="external" x="450" y="20" width="160" height="90" rx="6"/>
  <text class="title" x="466" y="44">API clients</text>
  <text x="466" y="64">readers, scripts,</text>
  <text x="466" y="80">a reverse proxy</text>
  <line class="link" x1="130" y1="65" x2="180" y2="65" marker-end="url(#pm-arrow)"/>
  <text x="155" y="58" text-anchor="middle">HTTP</text>
  <line class="link" x1="295" y1="110" x2="295" y2="180" marker-end="url(#pm-arrow)"/>
  <text x="303" y="142">HTTP over</text>
  <text x="303" y="156">Unix socket</text>
  <line class="link" x1="530" y1="110" x2="530" y2="180" marker-end="url(#pm-arrow)"/>
  <text x="538" y="142">HTTP over</text>
  <text x="538" y="156">Unix socket</text>
  <!-- Server and its data -->
  <rect class="proc" x="180" y="180" width="430" height="110" rx="6"/>
  <text class="title" x="196" y="204">server (kiki serve)</text>
  <text x="196" y="224">rw: data directory, socket directory, SQLite temp directory</text>
  <text x="196" y="240">ro: libraries, time zones, /proc, /sys</text>
  <text x="196" y="256">listens on the API's Unix socket; connects to nothing</text>
  <text x="196" y="272">no execve, and no new sockets, once its children are up</text>
  <rect class="external" x="650" y="180" width="130" height="110" rx="6"/>
  <text class="title" x="664" y="204">data directory</text>
  <text x="664" y="224">kiki.db</text>
  <text x="664" y="240">kiki.toml</text>
  <text x="664" y="256">plugins/</text>
  <text x="664" y="272">assets/</text>
  <line class="link" x1="610" y1="235" x2="650" y2="235" marker-end="url(#pm-arrow)"/>
  <text x="630" y="228" text-anchor="middle">rw</text>
  <!-- Script host -->
  <path class="link" d="M230 290 V330 H115 V370" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <text x="121" y="354">socket pair</text>
  <rect class="proc" x="20" y="370" width="190" height="90" rx="6"/>
  <text class="title" x="36" y="394">script host</text>
  <text x="36" y="414">files: none</text>
  <text x="36" y="430">sockets: none</text>
  <text x="36" y="446">runs plugins</text>
  <!-- Feed fetcher -->
  <rect class="group" x="235" y="335" width="550" height="280" rx="10"/>
  <text class="note" x="249" y="353">feed fetcher</text>
  <line class="link" x1="520" y1="290" x2="520" y2="370" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <text x="528" y="318">socket pair</text>
  <rect class="proc" x="360" y="370" width="320" height="90" rx="6"/>
  <text class="title" x="376" y="394">supervisor</text>
  <text x="376" y="414">ro: TLS trust stores, resolver config</text>
  <text x="376" y="430">may execve only the kiki executable</text>
  <text x="376" y="446">relays frames; parses nothing</text>
  <line class="link" x1="420" y1="460" x2="335" y2="510" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <line class="link" x1="517" y1="460" x2="517" y2="510" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <line class="link" x1="620" y1="460" x2="692" y2="510" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <text x="525" y="489">socket pairs</text>
  <rect class="exposed" x="250" y="510" width="170" height="90" rx="6"/>
  <text class="title" x="266" y="534">worker</text>
  <text x="266" y="554">ro: TLS trust stores</text>
  <text x="266" y="570">outbound TCP only</text>
  <text x="266" y="586">holds feed credentials</text>
  <rect class="exposed" x="435" y="510" width="160" height="90" rx="6"/>
  <text class="title" x="451" y="534">parser</text>
  <text x="451" y="554">files: none</text>
  <text x="451" y="570">sockets: none</text>
  <text x="451" y="586">parses XML, HTML, SVG</text>
  <rect class="exposed" x="610" y="510" width="165" height="90" rx="6"/>
  <text class="title" x="626" y="534">resolver</text>
  <text x="626" y="554">ro: resolver config</text>
  <text x="626" y="570">DNS only (port 53)</text>
  <text x="626" y="586">sees only hostnames</text>
  <!-- Internet -->
  <line class="link" x1="335" y1="600" x2="335" y2="660" marker-end="url(#pm-arrow)"/>
  <text x="343" y="636">HTTP(S)</text>
  <line class="link" x1="692" y1="600" x2="692" y2="660" marker-end="url(#pm-arrow)"/>
  <text x="700" y="636">DNS</text>
  <rect class="external" x="250" y="660" width="525" height="60" rx="6"/>
  <text class="title" x="266" y="684">Internet</text>
  <text x="266" y="702">feed and asset servers, proxies, name servers, and your LAN</text>
  <!-- Legend -->
  <rect class="proc" x="20" y="745" width="24" height="14" rx="3"/>
  <text x="52" y="757">Kiki process</text>
  <rect class="exposed" x="160" y="745" width="24" height="14" rx="3"/>
  <text x="192" y="757">handles bytes from the internet</text>
  <rect class="external" x="420" y="745" width="24" height="14" rx="3"/>
  <text x="452" y="757">outside Kiki</text>
  <line class="link" x1="20" y1="782" x2="44" y2="782" marker-end="url(#pm-arrow)"/>
  <text x="52" y="786">requests go one way</text>
  <line class="link" x1="200" y1="782" x2="224" y2="782" marker-start="url(#pm-arrow)" marker-end="url(#pm-arrow)"/>
  <text x="232" y="786">requests go both ways</text>
</svg>
<figcaption>Kiki's processes, what each may reach, and how they talk to
one another. An arrow points away from the side that starts a request.
<code>kiki web</code> is optional; <code>kiki serve</code> runs the rest on
its own.</figcaption>
</figure>

| Process | Started by | Files | Network | Its job |
| --- | --- | --- | --- | --- |
| server | `kiki serve`, or `kiki web` | read-write: the data directory, the socket's directory, SQLite's temp directory | listens on the API's Unix socket; connects to nothing | Owns the database and the asset cache, serves the API, schedules fetches and dispatches plugin events |
| script host | the server | none | none | Runs plugins |
| supervisor | the server | read-only: the TLS trust stores and the resolver's configuration | none of its own | Starts the three processes below, relays messages between them and the server, and replaces any that die |
| worker | the supervisor | read-only: the TLS trust stores | outbound TCP; may not bind or listen | Downloads feeds, images, enclosures and favicons |
| parser | the supervisor | none | none | Parses feeds, web pages, SVG and entries' HTML |
| resolver | the supervisor | read-only: the resolver's configuration | DNS only: UDP, and TCP to port 53 | Looks hostnames up for the worker |
| web UI | `kiki web` | none | accepts connections on the listener it bound at startup; connects only to Unix sockets | Serves the [web UI](web-ui.md), as a client of the API |

`kiki web` starts its server before installing its own sandbox, so the
server runs under its own sandbox alone, not inside the web UI's. Every
other child starts out inside its parent's sandbox and adds its own on top,
so it can never have more than its parent.

### How they talk

Each child is the kiki executable run again with a hidden subcommand. It
inherits one end of a Unix socket pair, which its parent created for it, and
no other file descriptor but its standard streams: no database handle and no
open files. Inside the feed fetcher, only the worker is given the
environment, which may hold a proxy's password; the parser and the resolver
get just the few variables they read.

None of the children can create a Unix socket, so its parent is the only
process on the machine it can talk to. In particular, nothing in the feed
fetcher can reach the API socket, whose only access control is who can
reach it.

Messages are length-prefixed binary frames, with a cap on their size, so a
compromised child can't make its parent allocate more than it can afford.

- **Server and script host**: the server sends plugins' source and events;
  the script host answers each with its result. While handling an event, a
  plugin may ask the server for something (`kiki.store`, `kiki.entries`,
  `kiki.feeds`), and the server decides whether and how to answer. The
  script host never touches the database itself.
- **Server and feed fetcher**: the server sends jobs, such as fetching a
  feed with its credentials, parsing a file, or downloading an image. The
  fetcher sends back plain data, which the server checks before writing
  anything.
- **Inside the feed fetcher**: the worker, parser and resolver each talk
  only to the supervisor, which passes frames between them and to the
  server. The worker hands bytes to the parser, and hostnames to the
  resolver; neither helper's answers ever reach the server directly, and
  the parser never sees a URL's credentials.

### The sandbox

On Linux each process puts up three layers of sandbox before it reads any
untrusted input. Every thread it starts later inherits them, and so does
every process it starts:

- **Landlock** restricts the files each process can reach, as in the table
  above, and the network ports the fetcher's processes and the web UI may
  bind or connect to. It also bars each process from signalling, or
  reaching abstract Unix sockets of, any process outside its own sandbox.
- **seccomp** kills the process if it makes a system call none of Kiki's
  processes need, such as `ptrace`, `mount`, `bpf`, `io_uring` or
  namespace creation, and refuses the socket calls that process has no
  use for. Only the supervisor may `execve`, and then only the kiki
  executable.
- **`PR_SET_MDWE`** refuses memory that is both writable and executable,
  so injected code can't be written into memory and then run. Every
  process gets it but the script host, which compiles WebAssembly plugins
  to native code, and so must write code and then run it. (A build
  without the `wasm-plugins` feature refuses it in the script host too.)
  For the same reason, the systemd units Kiki ships don't set
  `MemoryDenyWriteExecute=`, which every process would inherit.

Each of these depends on the kernel:

| Feature | Linux |
| --- | --- |
| Landlock's file rules | 5.13 |
| Refusing writable and executable memory | 6.3 |
| Landlock's network rules | 6.7 |
| Landlock's signal and abstract socket scoping | 6.12 |

On an older kernel, the parts it doesn't support are skipped, and a warning
in the log says which. The packaged systemd units add systemd's own
hardening on top; see [Running as a service](deployment.md#sandboxing).

## Threats

### A hostile feed

A feed, or a page or image it links to, may be built to exploit a bug in
Kiki's TLS, HTTP, decompression or parsing code.

- **Parsing** happens in the parser, the process with the least in reach:
  no files, no sockets, no credentials, and only the bytes it was handed. A
  parser taken over by a feed can lie about what the feed said, and that is
  all. The server checks what it is told as it would any input: it records
  entries against the feed it asked about, not the feed the reply names, and
  sanitizes an SVG image again before storing it.
- **Downloading** happens in the worker, which has to reach the network. A
  worker taken over by a server it fetched from can connect wherever it
  likes and read the TLS trust stores, and sees the credentials of the
  feeds it fetches, and the proxy's, from then until it is replaced. It
  can't reach the database, any other file, or the API socket.
- **Crashes** are contained. The supervisor replaces a worker, parser or
  resolver that dies or hangs, and Kiki works out which feed was to blame
  by retrying the requests that were in flight one at a time. That feed is
  recorded as having crashed the fetcher; the others carry on.
- **Exhaustion** is bounded: every fetch has a timeout and a cap on the
  size of the body (see `timeout_seconds` and `max_feed_bytes` under
  [`[feed_fetch]`](configuration/settings.md#feed_fetch)), and the supervisor kills a parse or lookup that
  runs too long.

Content that is merely unpleasant, rather than an exploit, reaches you
through clients. The [`sanitize`](plugins/sanitize.md) plugin, installed by
default, removes scripts, styles, embedded content, event handlers and
unsafe links from new entries' HTML before Kiki stores it, and the web UI
sanitizes it again before display. Without that plugin, or for entries
stored before it was installed, Kiki stores entries' HTML as the feed wrote
it, so **other API clients should sanitize entry content themselves**
before showing it in a browser, rather than rely on the plugin's config.
The [`privacy`](plugins/privacy.md) plugin removes tracking
pixels and tracking parameters from entries' content, but it isn't a
sanitizer: it leaves scripts, event handlers and the like where they are.

### A hostile name server

DNS answers come from whichever name server a feed's domain points to. The
C library's resolver parses them in the resolver process, which sees
nothing but hostnames: no feed bytes and no credentials.

### Requests to your network

A feed's URL, or one it redirects to, may name `localhost` or an address on
your LAN, and Kiki will fetch it. Kiki doesn't block private addresses,
because feeds on the local network are a reasonable thing to subscribe to.
If Kiki must not reach some services, use a [proxy](configuration/settings.md#proxy) or a firewall
to keep it from them.

Credentials for a feed are only sent to that feed's own origin; they are
dropped when a redirect leads to another.

### A network attacker

HTTPS feeds are verified against the system's TLS trust stores. Plain HTTP
feeds have no protection from anyone who can see or change their traffic,
and nor do their credentials.

### A misbehaving plugin

A plugin, or a feed that subverts one, runs in the script host, which has
no files and no sockets. What it can do is what the plugin API allows:

- change or drop the entries passed to its `entry.ingest` handlers;
- delay a feed's next fetch, through `fetch.schedule`, but never past the
  feed's own interval, and never to fetch it sooner than its server asks;
- tag and untag any stored entry;
- read the URL and title of any feed;
- read and write plugin state. The server keeps each plugin's state
  apart, but it takes the script host's word for which plugin is asking, so
  code that breaks out of the Lua VM, or out of a WebAssembly plugin's
  sandbox, can reach every plugin's state.

Plugins that ask for different permissions run in separate Lua VMs, so a
plugin without `entries.delete` can't change the code of one that has it,
such as by replacing `string.format`. Each WebAssembly plugin runs in an
instance of its own, sharing no memory with any other plugin. All of them
share the script host process, though, so code that breaks out of a VM or
an instance can make any call, with any plugin's permissions.

The script host compiles WebAssembly plugins to native code with Wasmtime's
Cranelift compiler, so it may write code to memory and then run it, which no
other Kiki process may. A bug in Cranelift, or in Wasmtime's sandboxing, that
lets a plugin run code of its own lands in the script host, with no files,
no sockets, and only the plugin API to reach the server through: the same
place a Lua VM escape lands. What the host loses is a defence against
memory-corruption exploits, which can no longer be stopped from writing new
code and running it.

Each handler has a time and a memory budget (see
[Resource limits](writing-plugins.md#resource-limits)). A plugin's manifest
may lift its time budget, as the bundled `sanitize` plugin does so that no
entry is stored unsanitized; the server still stops a script host that
doesn't answer for 10 seconds. A script host that
crashes or stops answering is not replaced; plugins stay disabled until
Kiki is restarted, rather than handing a fresh Lua VM to whatever broke the
last one. Entries keep flowing in either way. A WebAssembly plugin that
traps, such as by running out of time or memory, is started afresh in the
script host, and disabled until plugins reload if it keeps trapping.

`kiki serve --no-script-isolation` runs plugins inside the server instead,
with the server's access to the database. Avoid it unless the script host
won't start.

### The web UI

By default the web UI has no login, and may do whatever the API allows
requests without a token. With [`--require-login`](web-ui.md#logging-in),
it asks for an API token first and acts with that token's scopes. Its
session cookie is `HttpOnly` and `SameSite=Strict`, and the login form
sends the token over whatever connection the browser has, so beyond your
own machine, serve it over HTTPS. It listens on `127.0.0.1` by default,
and answers only requests whose `Host` header names an allowed host, so
that a website can't reach it by pointing its own domain at your machine
(DNS rebinding). Requests that change anything must come from the web UI's
own pages, judging by the browser's `Sec-Fetch-Site` or `Origin` headers.

A web UI taken over by a request can reach the API, but no files and no
other network service. Through the API, it can do what anonymous access
allows, and what the tokens of the sessions it holds allow.

## Limits

Kiki doesn't defend against:

- **Anyone who can reach the API without a token**, unless you set
  `anonymous_access`. Before exposing the API beyond your user, set it to
  `"token-required"` or put a reverse proxy with authentication in front.
- **Anyone holding a token**, within its scopes. A token is a bearer
  credential: whoever has it can use it until it expires or is revoked.
- **Other processes running as your user.** They can read the data
  directory directly; the sandbox limits what Kiki can do, not what others
  can do to Kiki.
- **A compromised server process.** It holds the database, the settings
  and every feed's credentials. Everything else on this page exists to keep
  untrusted input away from it.
- **Kernel bugs**, or a kernel too old for parts of the sandbox (see
  [The sandbox](#the-sandbox)).
- **Systems other than Linux**, where Kiki runs without a sandbox, and
  `--no-sandbox`, which turns it off on Linux. seccomp is also a denylist
  rather than an allowlist: it removes the most dangerous system calls
  without risking killing Kiki over a harmless one nobody thought of.
