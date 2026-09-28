//! Integration tests that exercise `kiki serve` under its real sandbox.
//!
//! The library-level unit tests construct a `Server` in-process, which
//! bypasses the sandbox entirely (it is installed in `ServeArgs::run`,
//! not in `Server::run_async`). These tests close that gap by spawning
//! the actual `kiki` binary in a subprocess — with Landlock + seccomp
//! applied just as in production — and driving it over its Unix socket.
//!
//! Each test provisions its own temporary data directory, starts a
//! kiki subprocess pointed at it, and tears the process down on drop.
//! A regression that adds a newly-denied syscall to a hot code path
//! (DB open, UDS bind, JSON parse, SQLite write, reqwest fetch) will
//! show up as the subprocess dying with SIGSYS before a request can
//! complete, surfaced via [`Kiki::assert_still_running`].

#![cfg(all(target_os = "linux", feature = "cli"))]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempfile::TempDir;

const KIKI_BIN: &str = env!("CARGO_BIN_EXE_kiki");

// --------------------------------------------------------------------
// Kiki subprocess harness
// --------------------------------------------------------------------

/// A running `kiki serve` subprocess with its own temp data directory
/// and Unix socket. Dropped instances are killed and reaped.
struct Kiki {
    _dir: TempDir,
    socket: PathBuf,
    child: Option<Child>,
}

impl Kiki {
    /// Spawn `kiki serve` with the given extra flags in a fresh temp
    /// data directory and wait for it to start listening.
    fn spawn(extra_args: &[&str]) -> Self {
        let dir = TempDir::with_prefix("kiki-sandbox-test").expect("create tempdir");

        let init_status = Command::new(KIKI_BIN)
            .arg("init")
            .env("KIKI_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn kiki init");
        assert!(init_status.success(), "kiki init failed: {init_status:?}");

        let socket = dir.path().join("kiki.sock");
        let mut cmd = Command::new(KIKI_BIN);
        cmd.current_dir(dir.path())
            .arg("serve")
            .arg("--uds")
            .arg(&socket)
            .args(extra_args)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let child = cmd.spawn().expect("spawn kiki serve");

        let mut kiki = Kiki {
            _dir: dir,
            socket,
            child: Some(child),
        };
        kiki.wait_until_ready(Duration::from_secs(10));
        kiki
    }

    /// Poll for the Unix socket to appear and accept a connection.
    fn wait_until_ready(&mut self, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("child")
                .try_wait()
                .expect("try_wait")
            {
                panic!(
                    "kiki exited before the socket appeared: {}",
                    describe_exit(status)
                );
            }
            if self.socket.exists() && UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        panic!("kiki did not start listening within {timeout:?}");
    }

    /// Panic if the child has exited. Called between requests to catch
    /// a SIGSYS that arrived after the server was already up.
    fn assert_still_running(&mut self) {
        if let Some(status) = self
            .child
            .as_mut()
            .expect("child")
            .try_wait()
            .expect("try_wait")
        {
            panic!(
                "kiki exited unexpectedly mid-test: {}",
                describe_exit(status)
            );
        }
    }

    fn get(&self, path: &str) -> HttpResponse {
        http_request(&self.socket, "GET", path, None)
    }

    fn post_json(&self, path: &str, body: &str) -> HttpResponse {
        http_request(&self.socket, "POST", path, Some(body))
    }

    /// Send SIGTERM, wait for the process, and return its exit status.
    /// Panics if the process was killed by a signal other than SIGTERM.
    fn shutdown(mut self) {
        let child = self.child.as_mut().expect("child");
        let pid = child.id() as libc::pid_t;
        // SAFETY: sending SIGTERM to our own child is always safe; the
        // worst case is ESRCH if the child has already exited.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let status = child.wait().expect("wait for child");
        self.child = None;
        if let Some(sig) = status.signal() {
            assert!(
                sig == libc::SIGTERM,
                "kiki died from signal {sig} (expected clean exit or SIGTERM): {}",
                describe_exit(status)
            );
        }
    }
}

impl Drop for Kiki {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Render an ExitStatus in a way that flags SIGSYS (the likely symptom
/// of a seccomp regression) so a failing test message is obvious.
fn describe_exit(status: std::process::ExitStatus) -> String {
    match status.signal() {
        Some(sig) if sig == libc::SIGSYS => "SIGSYS (seccomp filter killed the process — a denied \
             syscall was attempted)"
            .to_string(),
        Some(sig) => format!("killed by signal {sig}"),
        None => format!("exit code {:?}", status.code()),
    }
}

// --------------------------------------------------------------------
// Minimal HTTP-over-UDS client (keeps the test hermetic — no reqwest,
// no curl dependency).
// --------------------------------------------------------------------

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

impl HttpResponse {
    fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).expect("non-utf8 response body")
    }

    /// Panic unless the status code is 2xx. Returns self so the caller
    /// can chain `.json()`.
    #[track_caller]
    fn assert_success(self) -> Self {
        assert!(
            (200..300).contains(&self.status),
            "request failed with HTTP {}: {}",
            self.status,
            self.body_str()
        );
        self
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("response body is not valid JSON")
    }
}

fn http_request(socket: &Path, method: &str, path: &str, body: Option<&str>) -> HttpResponse {
    let mut stream = UnixStream::connect(socket).expect("connect to kiki socket");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut req = format!(
        "{method} {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Connection: close\r\n"
    );
    if let Some(b) = body {
        req.push_str(&format!(
            "Content-Type: application/json\r\n\
             Content-Length: {}\r\n",
            b.len()
        ));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    if let Some(b) = body {
        stream.write_all(b.as_bytes()).expect("write body");
    }

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("malformed response: no header/body separator");
    let headers = &raw[..split];
    let body = raw[split + 4..].to_vec();

    let first_line = headers
        .split(|&b| b == b'\n')
        .next()
        .expect("empty response");
    let line = std::str::from_utf8(first_line).expect("non-utf8 status line");
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .expect("status line missing code")
        .parse()
        .expect("status code parse");

    HttpResponse { status, body }
}

// --------------------------------------------------------------------
// A throwaway HTTP server serving a single RSS document, used by the
// outbound-fetch test.
// --------------------------------------------------------------------

const RSS_BODY: &str = r#"<?xml version="1.0"?>
<rss version="2.0">
  <channel>
    <title>sandbox-test feed</title>
    <link>http://127.0.0.1/</link>
    <description>test</description>
    <item>
      <title>hello from a sandboxed fetch</title>
      <link>http://127.0.0.1/item-1</link>
      <guid>urn:kiki:sandbox-test:item-1</guid>
      <description>ok</description>
    </item>
  </channel>
</rss>
"#;

/// Spawn a tiny single-shot HTTP server on 127.0.0.1 that serves
/// [`RSS_BODY`] once and exits. Returns the bound address and the
/// thread handle.
fn spawn_local_rss_server() -> (SocketAddr, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local rss server");
        tx.send(listener.local_addr().expect("local_addr")).ok();
        listener
            .set_nonblocking(false)
            .expect("set_nonblocking(false)");
        // Accept up to a handful of connections so that kiki's probe
        // requests (HEAD, conditional GET on refetch) don't hang the
        // test if the fetcher issues more than one.
        for _ in 0..4 {
            let (mut stream, _) = match listener.accept() {
                Ok(x) => x,
                Err(_) => return,
            };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/rss+xml; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{}",
                RSS_BODY.len(),
                RSS_BODY
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    let addr = rx.recv().expect("rss server failed to bind");
    (addr, handle)
}

// --------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------

#[test]
fn sandboxed_server_serves_the_api() {
    let mut kiki = Kiki::spawn(&[]);
    let feeds = kiki.get("/v1/feeds").assert_success();
    kiki.assert_still_running();
    assert_eq!(feeds.json()["count"], 0);
    kiki.shutdown();
}

#[test]
fn sandboxed_server_persists_db_writes() {
    let mut kiki = Kiki::spawn(&[]);

    kiki.post_json(
        "/v1/feeds/create",
        r#"{"title":"sandbox test","url":"http://127.0.0.1:1/never-fetched"}"#,
    )
    .assert_success();
    kiki.assert_still_running();

    let list = kiki.get("/v1/feeds").assert_success();
    assert_eq!(
        list.json()["count"],
        1,
        "expected one feed to be persisted, got {}",
        list.body_str()
    );
    kiki.assert_still_running();
    kiki.shutdown();
}

#[test]
fn sandboxed_server_fetches_outbound_http() {
    let (addr, _server) = spawn_local_rss_server();
    let mut kiki = Kiki::spawn(&[]);

    let created = kiki
        .post_json(
            "/v1/feeds/create",
            &format!(r#"{{"title":"sandbox fetch","url":"http://{addr}/feed.xml"}}"#),
        )
        .assert_success();
    let feed_id = created.json()["id"].as_i64().expect("id");

    kiki.post_json(&format!("/v1/feeds/refresh/{feed_id}"), "")
        .assert_success();

    // Entry ingestion is asynchronous — poll until the entry shows up
    // or the process dies.
    let deadline = Instant::now() + Duration::from_secs(10);
    let last = loop {
        kiki.assert_still_running();
        let entries = kiki.get("/v1/entries?limit=5").assert_success();
        if entries.json()["count"].as_i64().unwrap_or(0) >= 1 {
            break entries;
        }
        if Instant::now() >= deadline {
            panic!(
                "entry never appeared after sandboxed fetch; last response: {}",
                entries.body_str()
            );
        }
        thread::sleep(Duration::from_millis(200));
    };
    let _ = last;
    kiki.shutdown();
}

#[test]
fn seccomp_log_only_mode_still_runs() {
    let mut kiki = Kiki::spawn(&["--seccomp-log-only"]);
    kiki.get("/v1/feeds").assert_success();
    kiki.assert_still_running();
    kiki.shutdown();
}

#[test]
fn no_sandbox_flag_still_runs() {
    let mut kiki = Kiki::spawn(&["--no-sandbox"]);
    kiki.get("/v1/feeds").assert_success();
    kiki.assert_still_running();
    kiki.shutdown();
}

// --------------------------------------------------------------------
// Process-tree helpers
// --------------------------------------------------------------------

/// Scan /proc for children of `parent` whose command line contains
/// `needle`.
fn child_pids_matching(parent: u32, needle: &str) -> Vec<u32> {
    let mut found = Vec::new();
    let entries = match std::fs::read_dir("/proc") {
        Ok(e) => e,
        Err(_) => return found,
    };
    for entry in entries.flatten() {
        let pid: u32 = match entry.file_name().to_string_lossy().parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let ppid = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok());
        if ppid != Some(parent) {
            continue;
        }
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if String::from_utf8_lossy(&cmdline).contains(needle) {
            found.push(pid);
        }
    }
    found
}

/// Read the `Seccomp` mode out of a process's /proc status. `2` is
/// `SECCOMP_MODE_FILTER`.
fn seccomp_mode(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp:"))
        .and_then(|v| v.trim().parse().ok())
}

/// Poll until `pid` is gone, or the deadline passes.
fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    !Path::new(&format!("/proc/{pid}")).exists()
}

/// Refresh `feed_id` and poll until at least `n` entries exist, or the
/// server dies.
fn refresh_until_entries(kiki: &mut Kiki, feed_id: i64, n: i64) {
    kiki.post_json(&format!("/v1/feeds/refresh/{feed_id}"), "")
        .assert_success();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        kiki.assert_still_running();
        let entries = kiki.get("/v1/entries?limit=5").assert_success();
        if entries.json()["count"].as_i64().unwrap_or(0) >= n {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "entry never appeared after refresh; last response: {}",
                entries.body_str()
            );
        }
        thread::sleep(Duration::from_millis(200));
    }
}

// --------------------------------------------------------------------
// Feed fetcher isolation
// --------------------------------------------------------------------

mod fetch_isolation {
    use super::*;
    use kiki_rss::process::feed_fetcher;

    impl Kiki {
        /// PID of this server's feed fetcher supervisor, if it has one.
        fn fetcher_supervisor_pids(&self) -> Vec<u32> {
            let server_pid = self.child.as_ref().expect("child").id();
            child_pids_matching(server_pid, feed_fetcher::SUBCOMMAND)
        }

        /// Wait for the supervisor's worker to exist and return its PID.
        fn wait_for_fetcher_worker(&mut self, not: Option<u32>) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.assert_still_running();
                let supervisor = *self
                    .fetcher_supervisor_pids()
                    .first()
                    .expect("a feed fetcher supervisor");
                let workers = child_pids_matching(supervisor, feed_fetcher::SUBCOMMAND);
                if let Some(&w) = workers.iter().find(|&&w| Some(w) != not) {
                    return w;
                }
                if Instant::now() >= deadline {
                    panic!("no feed fetcher worker appeared (excluding {not:?})");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    fn create_feed(kiki: &mut Kiki, addr: SocketAddr) -> i64 {
        let created = kiki
            .post_json(
                "/v1/feeds/create",
                &format!(r#"{{"title":"isolated fetch","url":"http://{addr}/feed.xml"}}"#),
            )
            .assert_success();
        created.json()["id"].as_i64().expect("id")
    }

    /// By default feeds are fetched by a sandboxed supervisor/worker pair,
    /// not by the server.
    #[test]
    fn feeds_are_fetched_in_a_separate_sandboxed_process_by_default() {
        let mut kiki = Kiki::spawn(&[]);
        let supervisors = kiki.fetcher_supervisor_pids();
        assert_eq!(
            supervisors.len(),
            1,
            "expected exactly one `{}` child of the server, found {supervisors:?}",
            feed_fetcher::SUBCOMMAND
        );
        let worker = kiki.wait_for_fetcher_worker(None);
        for pid in [supervisors[0], worker] {
            assert_eq!(
                seccomp_mode(pid),
                Some(2),
                "fetcher pid {pid} is not running under a seccomp filter"
            );
        }
        kiki.shutdown();
    }

    /// A worker that dies is replaced inside the sandbox, and fetching
    /// carries on — the server itself can no longer spawn anything, so
    /// this is the only way fetching survives a crash.
    #[test]
    fn a_killed_worker_is_replaced_and_fetching_continues() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);
        let supervisor = kiki.fetcher_supervisor_pids()[0];
        let first = kiki.wait_for_fetcher_worker(None);

        // SAFETY: SIGKILL to a process of our own; at worst ESRCH.
        unsafe {
            libc::kill(first as libc::pid_t, libc::SIGKILL);
        }
        assert!(wait_for_exit(first, Duration::from_secs(5)));
        let second = kiki.wait_for_fetcher_worker(Some(first));
        assert_ne!(first, second);
        assert_eq!(
            kiki.fetcher_supervisor_pids(),
            vec![supervisor],
            "the supervisor must survive its worker"
        );

        let feed_id = create_feed(&mut kiki, addr);
        refresh_until_entries(&mut kiki, feed_id, 1);
        kiki.shutdown();
    }

    /// A feed named by hostname rather than IP is fetched even though the
    /// fetcher cannot read /etc/hosts or any resolver configuration: the
    /// lookup is done by the server, on the fetcher's behalf.
    #[test]
    fn hostnames_are_resolved_by_the_server() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);
        let created = kiki
            .post_json(
                "/v1/feeds/create",
                &format!(
                    r#"{{"title":"by name","url":"http://localhost:{}/feed.xml"}}"#,
                    addr.port()
                ),
            )
            .assert_success();
        let feed_id = created.json()["id"].as_i64().expect("id");
        refresh_until_entries(&mut kiki, feed_id, 1);
        kiki.shutdown();
    }

    /// Neither fetcher process may outlive the server.
    #[test]
    fn the_fetcher_exits_with_the_server() {
        let mut kiki = Kiki::spawn(&[]);
        let supervisor = kiki.fetcher_supervisor_pids()[0];
        let worker = kiki.wait_for_fetcher_worker(None);
        kiki.shutdown();

        for pid in [supervisor, worker] {
            assert!(
                wait_for_exit(pid, Duration::from_secs(5)),
                "fetcher pid {pid} outlived the server"
            );
        }
    }
}

// --------------------------------------------------------------------
// Script host isolation
// --------------------------------------------------------------------

#[cfg(all(feature = "lua", feature = "metrics"))]
mod script_isolation {
    use super::*;
    use kiki_rss::process::script_host;

    /// A script that stamps every ingested entry's title, so a test can tell
    /// from the API alone whether the handler actually ran.
    const TITLE_STAMPING_SCRIPT: &str = r#"kiki.on("entry.ingest", function(entry) entry.title = "[scripted] " .. entry.title; return entry end)"#;

    impl Kiki {
        /// PIDs of this server's script host children, read out of /proc.
        ///
        /// The isolation is only real if the Lua VM is somewhere else, so
        /// the tests check the process tree rather than taking the log's
        /// word for it.
        fn script_host_pids(&mut self) -> Vec<u32> {
            let server_pid = self.child.as_ref().expect("child").id();
            child_pids_matching(server_pid, script_host::SUBCOMMAND)
        }

        /// Block until `kiki_scripts_loaded` reaches `n`, so a test never
        /// races a reload that is still in flight.
        fn wait_for_scripts_loaded(&mut self, n: u64, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            loop {
                self.assert_still_running();
                let metrics = self.get("/metrics");
                let last = String::from_utf8_lossy(&metrics.body).into_owned();
                if metric_value(&last, "kiki_scripts_loaded") == Some(n as f64) {
                    return;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "kiki_scripts_loaded never reached {n} within {timeout:?}; \
                         last /metrics response:\n{last}"
                    );
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    /// Read a bare (unlabelled) gauge out of a Prometheus exposition body.
    fn metric_value(body: &str, name: &str) -> Option<f64> {
        body.lines()
            .filter(|l| !l.starts_with('#'))
            .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
    }

    /// Add a script and wait for the reloaded runner to report it loaded.
    fn install_script(kiki: &mut Kiki, source: &str) {
        install_script_with_config(kiki, source, serde_json::json!({}));
    }

    /// Add a script with a config and wait for the reloaded runner to
    /// report it loaded.
    fn install_script_with_config(kiki: &mut Kiki, source: &str, config: serde_json::Value) {
        let body = serde_json::json!({
            "engine": "lua",
            "text": source,
            "kind": "user",
            "config": config,
        });
        kiki.post_json("/v1/scripts/create", &body.to_string())
            .assert_success();
        kiki.post_json("/v1/scripts/reload", "").assert_success();
        kiki.wait_for_scripts_loaded(1, Duration::from_secs(10));
    }

    /// Refresh `feed_id` and poll until an entry shows up, returning its
    /// title.
    fn refresh_and_read_title(kiki: &mut Kiki, feed_id: i64) -> String {
        kiki.post_json(&format!("/v1/feeds/refresh/{feed_id}"), "")
            .assert_success();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            kiki.assert_still_running();
            let entries = kiki.get("/v1/entries?limit=5").assert_success();
            let json = entries.json();
            if let Some(title) = json["entries"][0]["title"].as_str() {
                return title.to_string();
            }
            if Instant::now() >= deadline {
                panic!(
                    "no entry appeared after refresh; last response: {}",
                    entries.body_str()
                );
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    /// Create a feed pointing at `addr` and return its id.
    fn create_local_feed(kiki: &mut Kiki, addr: SocketAddr) -> i64 {
        let created = kiki
            .post_json(
                "/v1/feeds/create",
                &format!(r#"{{"title":"scripted","url":"http://{addr}/feed.xml"}}"#),
            )
            .assert_success();
        created.json()["id"].as_i64().expect("id")
    }

    /// The headline property: by default the Lua VM lives in a child
    /// process, not in the server.
    #[test]
    fn scripts_run_in_a_separate_process_by_default() {
        let mut kiki = Kiki::spawn(&[]);
        let hosts = kiki.script_host_pids();
        assert_eq!(
            hosts.len(),
            1,
            "expected exactly one `{}` child of the server, found {hosts:?}",
            script_host::SUBCOMMAND
        );
        kiki.assert_still_running();
        kiki.shutdown();
    }

    /// The child must survive installing its own, tighter sandbox — a
    /// regression there would show up as no seccomp filter, or as a host
    /// that died before it could answer anything.
    #[test]
    fn the_script_host_installs_its_own_seccomp_filter() {
        let mut kiki = Kiki::spawn(&[]);
        let host = *kiki
            .script_host_pids()
            .first()
            .expect("a script host child");

        // 2 is SECCOMP_MODE_FILTER. The server's own filter is installed
        // separately; this asserts the child got one of its own.
        assert_eq!(
            seccomp_mode(host),
            Some(2),
            "script host pid {host} is not running under a seccomp filter"
        );
        kiki.assert_still_running();
        kiki.shutdown();
    }

    /// End to end: a script registered through the API transforms a real
    /// entry, with the VM in the sandboxed child and the database in the
    /// server. This is the test that fails if anything in the IPC path —
    /// framing, serialisation, the child's sandbox — is wrong.
    #[test]
    fn an_isolated_script_transforms_an_ingested_entry() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);

        install_script(&mut kiki, TITLE_STAMPING_SCRIPT);
        assert_eq!(kiki.script_host_pids().len(), 1);

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[scripted] "),
            "entry title was not transformed by the isolated script host: {title:?}"
        );
        kiki.shutdown();
    }

    /// A script's config crosses the IPC channel to the script host and
    /// reaches the script's top-level chunk.
    #[test]
    fn an_isolated_script_receives_its_config() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);

        install_script_with_config(
            &mut kiki,
            r#"
            local config = ...
            kiki.on("entry.ingest", function(entry)
                entry.title = config.prefix .. entry.title
                return entry
            end)
            "#,
            serde_json::json!({"prefix": "[configured] "}),
        );

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[configured] "),
            "entry title was not transformed using the script's config: {title:?}"
        );
        kiki.shutdown();
    }

    /// `kiki.regex` compiles and matches inside the script host, under the
    /// child's own sandbox, not just in the in-process VM the unit tests use.
    #[test]
    fn an_isolated_script_can_use_regexes() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);

        install_script(
            &mut kiki,
            r#"
            local re = kiki.regex([[^hello from (?P<how>\w+)]], "i")
            kiki.on("entry.ingest", function(entry)
                local caps = re:captures(entry.title)
                if caps then entry.title = "[" .. caps.how .. "] " .. entry.title end
                return entry
            end)
            "#,
        );

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert_eq!(title, "[a] hello from a sandboxed fetch");
        kiki.shutdown();
    }

    /// `--no-script-isolation` is the documented escape hatch: no child, and
    /// scripts still work. If this passes while the test above fails, the
    /// problem is in the IPC layer rather than in the scripting engine.
    #[test]
    fn no_script_isolation_runs_lua_in_the_server_process() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&["--no-script-isolation"]);

        assert!(
            kiki.script_host_pids().is_empty(),
            "--no-script-isolation must not spawn a script host"
        );

        install_script(&mut kiki, TITLE_STAMPING_SCRIPT);
        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[scripted] "),
            "in-process script did not transform the entry: {title:?}"
        );
        kiki.shutdown();
    }

    /// A reload swaps the child's VM in place rather than respawning it —
    /// the server cannot spawn anything once its sandbox is up, so a reload
    /// that needed a new process would silently stop working.
    #[test]
    fn reloading_scripts_reuses_the_same_host_process() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);

        install_script(&mut kiki, TITLE_STAMPING_SCRIPT);
        let before = kiki.script_host_pids();

        kiki.post_json("/v1/scripts/reload", "").assert_success();
        kiki.wait_for_scripts_loaded(1, Duration::from_secs(10));

        let after = kiki.script_host_pids();
        assert_eq!(
            before, after,
            "a script reload must not respawn the host process"
        );

        // And the reloaded runner still works.
        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(title.starts_with("[scripted] "), "title was {title:?}");
        kiki.shutdown();
    }

    /// The host must not outlive the server that spawned it.
    #[test]
    fn the_script_host_exits_with_the_server() {
        let mut kiki = Kiki::spawn(&[]);
        let host = *kiki
            .script_host_pids()
            .first()
            .expect("a script host child");
        kiki.shutdown();

        assert!(
            wait_for_exit(host, Duration::from_secs(5)),
            "script host pid {host} outlived the server"
        );
    }

    /// A broken script must not take the host down with it: the failure is
    /// reported over IPC, the entry passes through unmodified, and the
    /// child keeps serving.
    #[test]
    fn a_failing_script_leaves_the_host_running() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);

        install_script(
            &mut kiki,
            r#"kiki.on("entry.ingest", function(entry) error("boom") end)"#,
        );
        let before = kiki.script_host_pids();

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            !title.is_empty(),
            "a failing handler must not drop the entry"
        );

        assert_eq!(
            before,
            kiki.script_host_pids(),
            "a script error must not kill the host process"
        );
        kiki.assert_still_running();
        kiki.shutdown();
    }
}
