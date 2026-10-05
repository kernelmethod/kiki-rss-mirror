#!/usr/bin/env python3
"""Measure what Kiki's inter-process traffic costs during feed ingestion.

Starts a local feed server and a fresh `kiki serve`, then runs refreshes.
With the default `--workload new`, every item of every feed is new, so each
round ingests FEEDS * ITEMS entries through the feed fetcher and the script
host (with the plugins `kiki init` installs). With `--workload unchanged`,
no feed ever changes: each round is FEEDS fetches answered `304 Not
Modified`, with a `max-age=0` freshness hint, which measures what one fetch
and its scheduling cost apart from storing entries. For each round it
records the wall time, and the CPU time and context switches of every Kiki
process; with --strace, it also counts each process's system calls for the
first round.

    ipcbench.py run KIKI [--label NAME]           one binary, JSON lines out
    ipcbench.py compare BEFORE AFTER [--runs N]   interleaved runs, then a table
    ipcbench.py summarize RESULTS.jsonl           the table for saved results

See README.md for what the numbers mean. Needs Python 3.8+, Linux, and
`strace` for --strace; nothing outside the standard library.
"""
import argparse
import http.client
import http.server
import json
import os
import socket
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time

CLK_TCK = os.sysconf("SC_CLK_TCK")
LOREM = "Lorem ipsum dolor sit amet, consectetur adipiscing. "
# Processes are named by their hidden subcommand, e.g. `kiki __script-host`.
ROLES = ["server", "script-host", "feed-fetcher", "feed-worker", "feed-parser", "feed-resolver"]
TRACED = ["server", "script-host", "feed-fetcher", "feed-worker", "feed-parser"]
# A round has settled once Kiki's processes have used less than this much CPU
# between samples, for SETTLE_QUIET samples in a row, SETTLE_STEP apart.
SETTLE_CPU = 0.02
SETTLE_QUIET = 3
SETTLE_STEP = 0.2


# --- The feed server ------------------------------------------------------


class FeedServer:
    """Serves /feed/<n>.xml: `items` items of about `content_bytes` of HTML
    each, whose guids change every round.

    With `unchanged`, the feeds keep the items of round 0 instead, and are
    served with an `ETag` and `Cache-Control: max-age=0`, so every request
    after the first is answered `304 Not Modified`. `requests` counts the
    requests answered, and `not_modified` the 304s among them."""

    def __init__(self, items, content_bytes, unchanged=False):
        self.items = items
        self.content = "<p>" + LOREM * max(1, content_bytes // len(LOREM)) + "</p>"
        self.round = 0
        self.unchanged = unchanged
        self.requests = 0
        self.not_modified = 0
        self.lock = threading.Lock()
        bench = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                try:
                    n = int(self.path.rsplit("/", 1)[1].split(".")[0])
                except (IndexError, ValueError):
                    self.send_response(404)
                    self.end_headers()
                    return
                r = 0 if bench.unchanged else bench.round
                etag = f'"r{r}-f{n}"'
                fresh = bench.unchanged and self.headers.get("If-None-Match") == etag
                with bench.lock:
                    bench.requests += 1
                    bench.not_modified += fresh
                if fresh:
                    self.send_response(304)
                    self.send_header("ETag", etag)
                    self.send_header("Cache-Control", "max-age=0")
                    self.end_headers()
                    return
                body = bench.feed(n, r)
                self.send_response(200)
                if bench.unchanged:
                    self.send_header("ETag", etag)
                    self.send_header("Cache-Control", "max-age=0")
                self.send_header("Content-Type", "application/rss+xml")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()
        self.base = f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def feed(self, n, r):
        items = "".join(
            f"<item><title>Item {r}-{n}-{i}</title><guid>r{r}-f{n}-i{i}</guid>"
            f"<link>http://example.com/{r}/{n}/{i}?utm_source=bench</link>"
            f"<pubDate>Mon, 01 Jan 2024 00:00:00 GMT</pubDate>"
            f"<description><![CDATA[{self.content}]]></description></item>"
            for i in range(self.items)
        )
        return (
            f'<?xml version="1.0"?><rss version="2.0"><channel><title>Feed {n}</title>'
            f"<link>http://example.com/</link><description>bench</description>"
            f"{items}</channel></rss>"
        ).encode()


# --- Talking to Kiki ------------------------------------------------------


class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("localhost", timeout=120)
        self.unix_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.unix_path)


def api(sock, method, path, body=None):
    conn = UnixConnection(sock)
    data = json.dumps(body).encode() if body is not None else None
    conn.request(method, path, body=data, headers={"Content-Type": "application/json"})
    resp = conn.getresponse()
    raw = resp.read()
    if resp.status >= 300:
        raise RuntimeError(f"{method} {path}: {resp.status} {raw[:200]!r}")
    return json.loads(raw) if raw else None


def descendants(pid):
    try:
        kids = open(f"/proc/{pid}/task/{pid}/children").read().split()
    except OSError:
        return []
    out = []
    for kid in map(int, kids):
        out += [kid] + descendants(kid)
    return out


def role(pid, server):
    if pid == server:
        return "server"
    try:
        argv = open(f"/proc/{pid}/cmdline").read().split("\0")
    except OSError:
        return None
    return next((a.strip("_") for a in argv if a.startswith("__")), None)


def sample(server):
    """CPU seconds and context switches per role, over all their threads."""
    totals = {}
    for pid in [server] + descendants(server):
        name = role(pid, server)
        if not name:
            continue
        try:
            stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
            cpu = (int(stat[11]) + int(stat[12])) / CLK_TCK
            switches = 0
            for tid in os.listdir(f"/proc/{pid}/task"):
                for line in open(f"/proc/{pid}/task/{tid}/status"):
                    if "ctxt_switches:" in line:
                        switches += int(line.split()[-1])
        except OSError:
            continue
        t = totals.setdefault(name, [0.0, 0])
        t[0] += cpu
        t[1] += switches
    return totals


def settle(server, start):
    """Wait for the work a round set off, entries' asset caching among it,
    to finish: until Kiki's processes go quiet. Returns the last sample and
    the seconds from `start` to the last one in which they were busy."""
    last = sample(server)
    busy_at = time.time()
    quiet = 0
    deadline = time.time() + 300
    while quiet < SETTLE_QUIET and time.time() < deadline:
        time.sleep(SETTLE_STEP)
        now = sample(server)
        used = sum(now[k][0] - last.get(k, [0, 0])[0] for k in now)
        last = now
        if used < SETTLE_CPU:
            quiet += 1
        else:
            quiet = 0
            busy_at = time.time()
    return last, busy_at - start


def parse_strace(path):
    calls = {}
    for line in open(path):
        f = line.split()
        if len(f) >= 5 and f[0][0].isdigit() and f[-1] != "total":
            try:
                calls[f[-1]] = int(f[3])
            except ValueError:
                pass
    return dict(sorted(calls.items(), key=lambda kv: -kv[1]))


# --- A run ----------------------------------------------------------------


def run(kiki, label, workload, feeds, items, content_bytes, rounds, trace, serve_args):
    unchanged = workload == "unchanged"
    feeds_srv = FeedServer(items, content_bytes, unchanged)
    home = tempfile.mkdtemp(prefix="kiki-ipcbench-")
    env = {k: v for k, v in os.environ.items() if "proxy" not in k.lower()}
    env.update(KIKI_HOME=home, RUST_LOG=os.environ.get("KIKI_LOG", "warn"))
    subprocess.run([kiki, "init"], env=env, check=True, capture_output=True)
    sock = os.path.join(home, "kiki.sock")
    log = open(os.path.join(home, "kiki.log"), "w")
    proc = subprocess.Popen(
        [kiki, "serve", "--uds", sock, *serve_args], env=env, stdout=log, stderr=log
    )
    try:
        start = time.time()
        while not os.path.exists(sock):
            if time.time() - start > 20 or proc.poll() is not None:
                raise RuntimeError(f"kiki did not start; see {log.name}")
            time.sleep(0.05)
        time.sleep(0.5)

        # Only the benchmark's refreshes, not the scheduler's.
        settings = api(sock, "GET", "/v1/settings/feed-fetch")
        settings["default_fetch_interval_seconds"] = 86400
        settings["min_polling_cadence_seconds"] = 3600
        api(sock, "PUT", "/v1/settings/feed-fetch", settings)
        for n in range(feeds):
            api(sock, "POST", "/v1/feeds/create",
                {"title": f"Feed {n}", "url": f"{feeds_srv.base}/feed/{n}.xml"})

        db = sqlite3.connect(f"file:{home}/kiki.db?mode=ro", uri=True, timeout=30)

        def wait_for(r):
            want = feeds * items
            deadline = time.time() + 600
            have = 0
            while time.time() < deadline:
                (have,) = db.execute(
                    "SELECT count(*) FROM entries WHERE guid LIKE ?", (f"r{r}-%",)
                ).fetchone()
                if have >= want:
                    return
                time.sleep(0.05)
            raise RuntimeError(f"round {r}: only {have} of {want} entries arrived")

        def wait_for_checks(since):
            """Wait until every feed has been checked at or after `since`."""
            deadline = time.time() + 600
            have = 0
            while time.time() < deadline:
                (have,) = db.execute(
                    "SELECT count(*) FROM feeds WHERE last_checked >= ?", (since,)
                ).fetchone()
                if have >= feeds:
                    return
                time.sleep(0.05)
            raise RuntimeError(f"only {have} of {feeds} feeds were checked")

        # Round 0, queued by creating the feeds, is the warm-up.
        wait_for(0)
        time.sleep(1)
        results = []
        for r in range(1, rounds + 1):
            feeds_srv.round = r
            tracers = []
            if trace and r == 1:
                for pid in [proc.pid] + descendants(proc.pid):
                    name = role(pid, proc.pid)
                    if name in TRACED:
                        out = os.path.join(home, f"strace-{name}.txt")
                        cmd = ["strace", "-c", "-f", "-o", out, "-p", str(pid)]
                        tracers.append((name, out, subprocess.Popen(cmd, stderr=subprocess.DEVNULL)))
                time.sleep(1)
            # `last_checked` is in whole seconds, so a round starts on a new
            # one to tell its checks from the last round's.
            time.sleep(1 - time.time() % 1)
            before = sample(proc.pid)
            requests, not_modified = feeds_srv.requests, feeds_srv.not_modified
            start = time.time()
            api(sock, "POST", "/v1/feeds/refresh")
            if unchanged:
                wait_for_checks(int(start))
            else:
                wait_for(r)
            wall = time.time() - start
            after, settled = settle(proc.pid, start)
            result = {
                "requests": feeds_srv.requests - requests,
                "not_modified": feeds_srv.not_modified - not_modified,
                "wall": round(wall, 3),
                "settle": round(max(wall, settled), 3),
                "procs": {
                    k: [round(after[k][i] - before.get(k, [0, 0])[i], 3) for i in range(2)]
                    for k in after
                },
            }
            if tracers:
                result["syscalls"] = {}
                for name, out, tracer in tracers:
                    tracer.send_signal(2)
                    tracer.wait(30)
                    result["syscalls"][name] = parse_strace(out)
            results.append(result)
        return {"label": label, "workload": workload, "feeds": feeds, "items": items,
                "rounds": results}
    finally:
        proc.terminate()
        try:
            proc.wait(10)
        except subprocess.TimeoutExpired:
            proc.kill()


# --- Reporting ------------------------------------------------------------


def summarize(results, out=sys.stdout):
    labels, rounds, traces = [], {}, {}
    for res in results:
        label = res["label"]
        if label not in labels:
            labels.append(label)
        for r in res["rounds"]:
            if "syscalls" in r:
                traces.setdefault(label, r["syscalls"])
            # A traced round is slowed by strace, so it is not timed.
            else:
                rounds.setdefault(label, []).append(r)
    workload = results[0].get("workload", "new") if results else "new"
    if workload == "unchanged":
        size = results[0]["feeds"]
        what = "304 Not Modified fetches"
    else:
        size = results[0]["feeds"] * results[0]["items"] if results else 0
        what = "new entries"

    def med(label, f):
        return statistics.median(f(r) for r in rounds[label]) if rounds.get(label) else 0

    print(f"Medians over rounds of {size:,} {what} "
          f"({', '.join(f'{l}: {len(rounds.get(l, []))}' for l in labels)} rounds)\n",
          file=out)
    rows = [
        ("feed requests", lambda r: r.get("requests", 0), "{:,.0f}"),
        ("304 responses", lambda r: r.get("not_modified", 0), "{:,.0f}"),
        ("wall time (s)", lambda r: r["wall"], "{:.2f}"),
        ("settle time (s)", lambda r: r.get("settle", 0), "{:.2f}"),
        ("all processes CPU (s)", lambda r: sum(v[0] for v in r["procs"].values()), "{:.2f}"),
        ("all ctx switches", lambda r: sum(v[1] for v in r["procs"].values()), "{:,.0f}"),
    ]
    for role_ in ROLES:
        rows.append((f"{role_} CPU (s)", lambda r, k=role_: r["procs"].get(k, [0, 0])[0], "{:.2f}"))
        rows.append((f"{role_} ctx switches", lambda r, k=role_: r["procs"].get(k, [0, 0])[1], "{:,.0f}"))
    if traces:
        for role_ in TRACED:
            rows.append((f"{role_} syscalls (strace)", None, role_))

    width = max(len(name) for name, _, _ in rows)
    print(f"{'':{width}}  " + "  ".join(f"{l:>12}" for l in labels)
          + ("  {:>8}".format("change") if len(labels) == 2 else ""), file=out)
    for name, f, fmt in rows:
        if f is None:
            vals = [sum(traces.get(l, {}).get(fmt, {}).values()) for l in labels]
            cells = [f"{v:,}" for v in vals]
        else:
            vals = [med(l, f) for l in labels]
            cells = [fmt.format(v) for v in vals]
        line = f"{name:{width}}  " + "  ".join(f"{c:>12}" for c in cells)
        if len(vals) == 2 and vals[0]:
            line += f"  {100 * (vals[1] - vals[0]) / vals[0]:+7.0f}%"
        print(line, file=out)

    for label in labels:
        if label in traces:
            print(f"\nBusiest system calls, {label}:", file=out)
            for role_, calls in traces[label].items():
                top = ", ".join(f"{k} {v:,}" for k, v in list(calls.items())[:5])
                print(f"  {role_:13} {top}", file=out)


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="cmd", required=True)

    def sizes(sp):
        sp.add_argument("--workload", choices=["new", "unchanged"], default="new",
                        help="new: every item is new each round; "
                             "unchanged: every fetch is a 304 (default: new)")
        sp.add_argument("--feeds", type=int,
                        help="feeds to create (default: 40, or 1000 for --workload unchanged)")
        sp.add_argument("--items", type=int,
                        help="items per feed (default: 250, or 5 for --workload unchanged)")
        sp.add_argument("--content-bytes", type=int, default=2048,
                        help="approximate size of each item's HTML")
        sp.add_argument("--rounds", type=int, default=3, help="measured refreshes per run")
        sp.add_argument("--strace", action="store_true",
                        help="count system calls in the first round (slows it down)")
        sp.add_argument("--serve-arg", action="append", default=[],
                        help="extra argument for `kiki serve`, e.g. --serve-arg=--no-sandbox")

    sp = sub.add_parser("run", help="benchmark one binary, printing JSON")
    sp.add_argument("kiki")
    sp.add_argument("--label", default="kiki")
    sizes(sp)

    sp = sub.add_parser("compare", help="benchmark two binaries, interleaved")
    sp.add_argument("before")
    sp.add_argument("after")
    sp.add_argument("--labels", default="before,after")
    sp.add_argument("--runs", type=int, default=3, help="runs of each binary")
    sp.add_argument("--out", help="also append each run's JSON to this file")
    sizes(sp)

    sp = sub.add_parser("summarize", help="print the table for saved results")
    sp.add_argument("results")

    args = p.parse_args()
    if args.cmd == "summarize":
        summarize([json.loads(l) for l in open(args.results) if l.strip()])
        return

    if args.feeds is None:
        args.feeds = 1000 if args.workload == "unchanged" else 40
    if args.items is None:
        args.items = 5 if args.workload == "unchanged" else 250
    common = (args.workload, args.feeds, args.items, args.content_bytes, args.rounds)
    if args.cmd == "run":
        res = run(args.kiki, args.label, *common, args.strace, args.serve_arg)
        print(json.dumps(res))
        return

    labels = args.labels.split(",")
    bins = [args.before, args.after]
    results = []
    out = open(args.out, "a") if args.out else None
    for i in range(args.runs):
        for kiki, label in zip(bins, labels):
            # System-call counting slows the round it covers, so it gets runs
            # of its own rather than skewing the timed ones.
            print(f"run {i + 1}/{args.runs}: {label}", file=sys.stderr)
            res = run(kiki, label, *common, False, args.serve_arg)
            results.append(res)
            if out:
                out.write(json.dumps(res) + "\n")
                out.flush()
    if args.strace:
        for kiki, label in zip(bins, labels):
            print(f"strace run: {label}", file=sys.stderr)
            res = run(kiki, label, args.workload, args.feeds, args.items, args.content_bytes,
                      1, True, args.serve_arg)
            results.append(res)
            if out:
                out.write(json.dumps(res) + "\n")
                out.flush()
    summarize(results)


if __name__ == "__main__":
    main()
