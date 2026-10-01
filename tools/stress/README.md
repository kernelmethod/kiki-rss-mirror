# kiki-stress

An out-of-process stress harness for `kiki serve`, for finding slowdowns and
deadlocks under concurrent load.

It starts a local feed server and a Kiki server, then drives Kiki's API with a
weighted mix of reads, writes, searches, tagging, refreshes, and feed churn
while the fetch scheduler ingests continuously. Along the way it:

- serves feeds that change every ten seconds; one in twenty is slow (2–8s),
  one in twenty answers `500`, and one in twenty has 1,500 items. Items embed
  images, so the asset cache is exercised too;
- runs two independent canaries: `GET /v1/health` (read pool) and toggling a
  system tag on one entry (the writer connection);
- watches in-flight requests, and when one exceeds `STALL_MS` or any request
  gets a `408`, attaches `gdb` to Kiki and saves every thread's backtrace.

## Running

```bash
# A build with symbols, so the backtraces name Kiki's functions.
cargo build --profile profiling

# A fresh Kiki home for the run.
export STRESS_HOME=$(mktemp -d)
KIKI_HOME=$STRESS_HOME target/profiling/kiki init

cd tools/stress
KIKI_BIN=../../target/profiling/kiki STRESS_OUT=/tmp/kiki-stress \
    FEEDS=300 WORKERS=64 SECS=90 cargo run --release
```

The harness strips proxy variables from Kiki's environment so it can reach
the local feed server.

### Web UI mode

With `MODE=web`, the harness runs `kiki web` instead of `kiki serve`, and its
clients browse the web UI the way a person reading in a browser does:
opening the index (mostly its first page), feed and tag pages, entries and
search results; loading the images and favicons on each page, revalidating
the ones it has seen with `If-None-Match`; swiping entries read, saving
them, and now and then marking a feed read. Every listing page is one
`/v1/entries/search` call that leaves out read and hidden entries, plus a
count of them, so this is the mode to use for the server's work under
normal use.

Before starting Kiki it sets the `filter` and `auto-tag` plugins' rules, as
a deployment of the web UI does, so that one entry in ten is hidden and the
tag pages have entries: see `STRESS_FILTER` and `STRESS_AUTOTAG` in
`src/main.rs`. `FILTER_TOML` and `AUTOTAG_TOML` add rules of your own, such
as a deployment's real ones, ahead of those.

```bash
KIKI_BIN=../../target/profiling/kiki STRESS_OUT=/tmp/kiki-web \
    MODE=web FEEDS=300 WORKERS=8 SECS=60 cargo run --release
```

The setup, sampling and canaries still use the API over the Unix socket.

### Profiling

`STRESS_OUT/pids.txt` names the processes started (`web=` and `serve=` in
web mode), for attaching a profiler once the log says the warm-up is done.
Most of the server's work happens on tokio's blocking threads, so sample
the whole system with DWARF call graphs (Kiki is built without frame
pointers):

```bash
perf record -e cpu-clock -F 99 -a --call-graph dwarf,8192 -o perf.data -- sleep 50
perf report -i perf.data --no-children --no-inline -g none --pid <serve pid> --sort sym
```

`--no-inline` keeps `perf report` from spending minutes resolving inlined
frames. `-e cpu-clock` works in VMs without hardware counters.

| Variable      | Default | Meaning                                                  |
|---------------|---------|----------------------------------------------------------|
| `KIKI_BIN`    | `kiki`  | The `kiki` binary to run                                 |
| `STRESS_HOME` | —       | `KIKI_HOME` for the server; must already be initialized  |
| `STRESS_OUT`  | —       | Directory for the run's output                           |
| `FEEDS`       | `300`   | Feeds to create before the load starts                   |
| `WORKERS`     | `64`    | Concurrent clients (closed loop)                         |
| `SECS`        | `60`    | Length of the load phase                                 |
| `SLOW_EVERY`  | `20`    | Every Nth feed responds slowly; `0` for none             |
| `STALL_MS`    | `5000`  | In-flight age that triggers a thread dump                |
| `KIKI_ARGS`   | —       | Extra arguments for `kiki serve`                         |
| `KIKI_LOG`    | `warn`  | `RUST_LOG` for the server                                |
| `MODE`        | `api`   | `api` to drive the API, `web` to drive the web UI        |
| `FILTER_TOML` | —       | In web mode, extra `filter` plugin rules                 |
| `AUTOTAG_TOML`| —       | In web mode, extra `auto-tag` plugin rules               |

## Output

`STRESS_OUT` receives:

- `report.txt`: per-operation latency percentiles and status counts, a
  per-second timeline (completions, errors, `408`s, worst latency, and each
  canary's worst latency), and the events seen during the run;
- `kiki.log`: the server's log;
- `metrics.txt`: the server's `/metrics` after the load, including pool
  acquire times, task-queue depth, and per-statement timings;
- `stack-N.txt`: `gdb` backtraces taken on stalls (at most four), of
  every process started;
- `pids.txt`: the processes started;
- in web mode, `filter.toml` and `auto-tag.toml`: the plugin rules set.

Clients run a closed loop, so when every client is blocked on one kind of
request, nothing else is sent. A gap in the timeline is a sign of that, not
necessarily of a server-wide stall; the canaries tell the two apart.
