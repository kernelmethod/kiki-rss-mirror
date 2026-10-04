# ipcbench

A benchmark for what Kiki's inter-process traffic costs while it ingests
feeds: between the server and the script host, and between the server and
the feed fetcher's processes.

It serves feeds from a local HTTP server, starts a fresh `kiki serve` with
the plugins `kiki init` installs, and runs refreshes in which every item
of every feed is new. Each round therefore ingests `--feeds` × `--items`
entries (10,000 by default), each of which crosses the fetcher's channels
and the script host's. For every round it records:

- the wall time from asking for the refresh to the last entry being stored;
- the CPU time and context switches of each Kiki process, read from
  `/proc`, summed over its threads.

With `--strace`, it also counts every system call each of the server, the
script host, the fetcher's supervisor and its worker makes over one round,
in runs of its own, since tracing slows the processes it traces.

## Running

It needs Linux, Python 3.8 or later, and `strace` for `--strace`; nothing
outside Python's standard library. Build the binaries to compare with the
`profiling` profile (or `release`), not a debug build:

```bash
git worktree add ../kiki-before main
(cd ../kiki-before && cargo build --profile profiling)
cargo build --profile profiling

tools/ipcbench/ipcbench.py compare \
    ../kiki-before/target/profiling/kiki target/profiling/kiki \
    --runs 5 --strace --out /tmp/ipcbench.jsonl
```

`compare` alternates between the two binaries, so that drift in the
machine's load is spread over both, and prints the median of every
measure with the change from one to the other:

```
Medians over rounds of 10,000 new entries (before: 15, after: 15 rounds)

                                      before         after    change
wall time (s)                           4.51          3.89      -14%
server CPU (s)                          4.01          3.49      -13%
...
script-host syscalls (strace)         80,160        20,080      -75%
```

`run` benchmarks a single binary and prints its results as JSON, and
`summarize` prints the table for results saved with `--out`. Each
`kiki serve` runs with a Kiki home of its own under the system's temporary
directory, which is left there so its `kiki.log` can be read afterwards.

| Option         | Default | Meaning                                                    |
|----------------|---------|------------------------------------------------------------|
| `--feeds`      | 40      | Feeds to create                                            |
| `--items`      | 250     | Items per feed, each about 2 KiB of HTML                   |
| `--rounds`     | 3       | Refreshes measured per run, after one warm-up refresh      |
| `--runs`       | 3       | Runs of each binary (`compare` only)                       |
| `--strace`     | off     | Also count system calls, in one extra run of each binary   |
| `--serve-arg`  | —       | Pass an argument to `kiki serve`; repeatable               |

## Reading the results

Wall time is the noisiest measure: a round is short, and its length depends
on how the feed fetches happen to overlap. CPU time and context switches
are steadier, and the system call counts barely move between runs, so a
change that removes messages or system calls shows up most clearly there.
Compare medians over several runs rather than single rounds.

The server's numbers include its SQLite work, which dominates them; the
script host does nothing but serve the server's requests, so its numbers
are the most direct measure of the script host channel.
