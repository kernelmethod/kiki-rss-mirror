# Memory usage

What holds memory in a running Kiki, what was done about it, and how to
measure it again. The measurements here were taken in October 2026 against
v0.28.0, prompted by the nixdev dashboard showing 100–200 MiB across Kiki's
processes while it fetched about 45 feeds for a single reader.

## Summary

* **The live heap is small.** Under steady load from `tools/stress`, the
  server's live heap peaked at about 40 MiB; at rest it is far less. Most of
  the resident memory was the allocator holding freed memory, not data.
* **The allocator decides most of the footprint.** mimalloc in secure mode,
  the allocator until now, kept about four times as much memory as musl's
  malloc across Kiki's processes, with no measurable difference in
  throughput or latency. Kiki no longer uses mimalloc: the static musl
  build uses musl's malloc, and glibc builds use glibc's with its arenas
  capped at two (see `src/main.rs`).
* **Histogram samples piled up between scrapes.** The Prometheus recorder
  is built with `build_recorder`, which starts no upkeep task, so every
  sample recorded between `/metrics` renders was kept: about 1.5 MiB/s
  under load, without bound when nothing scrapes. The server now folds
  samples into their buckets every five seconds
  ([`crate::metrics::run_upkeep`]).

## Where the memory went

Per-process breakdowns came from `/proc/<pid>/smaps`, counting PSS so that
the pages Kiki's processes share (the executable, and the feed fetcher
worker's copy-on-write pages) are split between them rather than counted
once per process. Heap profiles came from heaptrack, on a build with the
system allocator, since heaptrack cannot see allocations mimalloc makes.

Under the stress harness's web load (one client, about 450 requests a
second), the server's live heap peaked at about 40 MiB, of which:

| What | Peak |
|---|---|
| Buffered histogram samples, with a scrape every 15 s | ~10 MiB |
| SQLite page cache, across the read pool and writer | ~3.4 MiB |
| Everything else (requests, responses, entries, plugins) | ~26 MiB |

Its resident memory was several times that. The rest was:

* memory the allocator held after it was freed — the largest share by far,
  as the allocator comparison below shows;
* thread stacks: tokio's blocking pool runs every database call, and the
  server had 35–45 threads under load, each with the stack pages it had
  touched still resident;
* the executable's code, about 3 MiB of PSS per process.

Without scrapes, the histogram buffer dominates: the server grew
steadily to 304 MiB over three minutes, and a single `/metrics` render
brought it back to about 70 MiB. With scrapes every 15 seconds it held at
100–118 MiB under the same load.

## Allocator benchmarks

The glibc numbers were taken with `MALLOC_ARENA_MAX` set in the
environment, before `src/main.rs` set it itself; the two are equivalent.

Each run started a fresh Kiki home with 50 feeds from `tools/stress` in web
mode (`MODE=web`), scraped `/metrics` every 30 seconds as nixdev's
Prometheus does, and sampled every process's PSS every 10 seconds for 150
seconds, discarding the first three samples. The figures are the mean of
the total across all of Kiki's processes (`web`, `serve`, the feed
fetcher's supervisor and worker, and the script host), in MiB. Runs were on
a 4-core VM; absolute numbers will differ elsewhere, but every pair of
repeated runs agreed to within a few MiB.

### The static musl build

All five builds used the `profiling` profile with thin LTO, so they differ
only in allocator. "Background" is the harness with no clients
(`WORKERS=0`): only the feed refreshes, as on a quiet day. The 1-client
column is two runs averaged; the 8-client column is a single run.

| Allocator | Background | 1 client | 8 clients | req/s, 8 clients |
|---|---|---|---|---|
| mimalloc, secure (the old default) | 118 | 170 | 290 | 980 |
| mimalloc | 107 | 151 | — | — |
| mimalloc, tuned (below) | 50 | 66 | 103 | 881 |
| jemalloc, tuned (below) | 48 | 75 | 126 | 988 |
| **musl's malloc** | **28** | **40** | **70** | **934** |

With one client every build managed 320–375 requests a second, and the
spread between repeated runs of one build was larger than between builds.
With eight, the p50 and p99 latencies of the index, entry, search, and
asset pages were within about 10% of each other across all four builds:
musl's single allocator lock did not show at this load, which is more than
a hundred times what a person reading feeds generates.

The largest difference was in the feed fetcher, which handles each refresh
as a burst: in the background runs it held about 6 MiB with musl's malloc
and 36–46 MiB with mimalloc's secure mode. The server went from about 60
MiB to about 15.

The tuned variants were:

* jemalloc, through `tikv-jemallocator` with
  `narenas:2,background_thread:true,dirty_decay_ms:1000,muzzy_decay_ms:0`
  set at build time (`JEMALLOC_SYS_WITH_MALLOC_CONF`);
* mimalloc without secure mode, with `MIMALLOC_PURGE_DELAY=100`,
  `MIMALLOC_ARENA_EAGER_COMMIT=0`, `MIMALLOC_PAGE_FULL_RETAIN=0`,
  `MIMALLOC_PAGE_RECLAIM_ON_FREE=1`, and `MIMALLOC_ALLOW_THP=0`.

musl's malloc was chosen as the smallest and the simplest: it needs no
dependency and no tuning, and returns freed memory to the OS as it goes,
so Kiki's runtimes no longer need to hand memory back to mimalloc as their
threads park, as they did before. What it gives up is
mimalloc's secure mode (guard pages, encrypted free lists, randomized
allocation); musl's malloc keeps its metadata out of band and checks it,
which is some, but less, protection against heap corruption.

### The glibc build

The glibc builds (the default and `profiling` flake outputs, and the deb
and rpm packages) use glibc's malloc, with `M_ARENA_MAX` set to 2 at
startup. The same harness with one client, on glibc builds:

| Allocator | 1 client | req/s |
|---|---|---|
| mimalloc, secure | 194–197 | 427–454 |
| mimalloc | 156 | 457 |
| mimalloc, secure, `MIMALLOC_PURGE_DELAY=0` | 138 | 366 |
| glibc's malloc | 133 | 445 |
| glibc's malloc, `MALLOC_ARENA_MAX=2` | 83–89 | 429–436 |
| glibc's malloc, `MALLOC_ARENA_MAX=1` | 79 | 434 |

In the background runs, mimalloc's secure mode held 131 MiB and glibc's
malloc with `MALLOC_ARENA_MAX=2` held 53 MiB. A zero purge delay for
mimalloc cost a fifth of the throughput, so it was ruled out. Two arenas
rather than one keeps two threads from contending for one lock, for the
few MiB the second costs.

## Other findings

* **Backtraces under `RUST_BACKTRACE`.** A failed feed refresh is logged
  with `{:?}` (`src/tasks/worker.rs`), which, when `RUST_BACKTRACE` is
  set, captures and symbolizes a backtrace. Symbolizing parses and caches
  the executable's debug info, about 110 MiB in a build with symbols such
  as `profiling`. Leave `RUST_BACKTRACE` unset on long-running servers.
* **The config watcher sees every database write.** The config file's
  watcher watches its directory, which by default is Kiki's home and holds
  the database, so every SQLite write wakes the watcher thread: about
  435,000 allocations in three minutes of load. This costs CPU rather than
  memory.
* **The `kiki serve` started by `kiki web` outlives it** if `kiki web` is
  killed with `SIGKILL`, since nothing ties the server's lifetime to it.
  Under systemd, the unit's cgroup is killed as a whole, so this only
  affects processes run by hand.

## Measuring again

Build with symbols, start a run of the stress harness, and read each
process's PSS while it runs:

```bash
cargo build --profile profiling
export STRESS_HOME=$(mktemp -d)
KIKI_HOME=$STRESS_HOME target/profiling/kiki init

cd tools/stress && cargo build --release
KIKI_BIN=../../target/profiling/kiki STRESS_OUT=/tmp/kiki-mem \
    MODE=web FEEDS=50 WORKERS=1 SECS=150 ./target/release/kiki-stress &

# In another shell, every few seconds:
for pid in $(pgrep -x kiki); do
    printf '%s %s KiB\n' "$(tr '\0' ' ' </proc/$pid/cmdline | cut -d' ' -f2)" \
        "$(awk '/^Pss:/ {print $2}' /proc/$pid/smaps_rollup)"
done
```

Scrape `/metrics` during the run
(`curl --unix-socket $STRESS_HOME/kiki.sock http://localhost/metrics`), as a
deployment's Prometheus would, or the histogram buffer will be part of what
you measure. Unset `RUST_BACKTRACE`, for the reason above.

For a heap profile of a glibc build, start the server under `heaptrack`,
and pass `--no-sandbox` so that heaptrack can write its output:

```bash
heaptrack -o /tmp/serve target/profiling/kiki serve --no-sandbox
heaptrack_print -f /tmp/serve.*.zst --print-peaks 1 -n 15
```

Attaching heaptrack to a running server with `-p` needs `--no-sandbox` too,
and loses track of frees on threads that were already blocked when it
attached, which can show as large false leaks.
