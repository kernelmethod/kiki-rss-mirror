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

/// The user and group ID of `nobody`, which [`Kiki::spawn_unprivileged`]
/// runs kiki as when the tests run as root.
const NOBODY: u32 = 65534;

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
        Self::spawn_with(extra_args, |_| {})
    }

    /// As [`Self::spawn`], calling `setup` with the data directory once
    /// it has been initialized and before the server starts.
    fn spawn_with(extra_args: &[&str], setup: impl FnOnce(&Path)) -> Self {
        Self::spawn_as(extra_args, setup, None, &[], Stdio::null())
    }

    /// As [`Self::spawn`], with `envs` added to `kiki serve`'s
    /// environment.
    fn spawn_with_env(extra_args: &[&str], envs: &[(&str, &str)]) -> Self {
        Self::spawn_as(extra_args, |_| {}, None, envs, Stdio::null())
    }

    /// As [`Self::spawn`], with `kiki serve`'s standard error, which its
    /// children share, written to `log`, and `rust_log` deciding what is
    /// logged.
    fn spawn_logging_to(extra_args: &[&str], log: std::fs::File, rust_log: &str) -> Self {
        Self::spawn_as(
            extra_args,
            |_| {},
            None,
            &[("RUST_LOG", rust_log)],
            log.into(),
        )
    }

    /// As [`Self::spawn`], but run kiki as an unprivileged user when the
    /// tests run as root, for checks that root's privileges would get
    /// around.
    fn spawn_unprivileged(extra_args: &[&str]) -> Self {
        // SAFETY: `geteuid` has no preconditions and cannot fail.
        let root = unsafe { libc::geteuid() } == 0;
        Self::spawn_as(
            extra_args,
            |_| {},
            root.then_some(NOBODY),
            &[],
            Stdio::null(),
        )
    }

    /// As [`Self::spawn_with`], running kiki as the user and group `id`
    /// if one is given, with the data directory handed over to it, with
    /// `envs` added to `kiki serve`'s environment, and its standard error
    /// sent to `stderr`.
    ///
    /// `stderr` must not be a pipe nobody drains, which would stop the
    /// server and its children once it filled.
    fn spawn_as(
        extra_args: &[&str],
        setup: impl FnOnce(&Path),
        id: Option<u32>,
        envs: &[(&str, &str)],
        stderr: Stdio,
    ) -> Self {
        use std::os::unix::process::CommandExt;

        let dir = TempDir::with_prefix("kiki-sandbox-test").expect("create tempdir");
        let command = || {
            let mut cmd = Command::new(KIKI_BIN);
            if let Some(id) = id {
                cmd.uid(id).gid(id);
            }
            cmd
        };
        if let Some(id) = id {
            std::os::unix::fs::chown(dir.path(), Some(id), Some(id)).expect("chown tempdir");
        }

        let init_status = command()
            .arg("init")
            // The tests count the plugins they load themselves.
            .arg("--no-default-plugins")
            .env("KIKI_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn kiki init");
        assert!(init_status.success(), "kiki init failed: {init_status:?}");
        setup(dir.path());

        let socket = dir.path().join("kiki.sock");
        let mut cmd = command();
        cmd.current_dir(dir.path())
            .arg("serve")
            .arg("--uds")
            .arg(&socket)
            .args(extra_args)
            .env("RUST_LOG", "warn")
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(stderr);

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

    /// Wait until the log at `path` has every one of `needles` in it.
    fn wait_for_log(&mut self, path: &Path, needles: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.assert_still_running();
            let log = std::fs::read_to_string(path).unwrap_or_default();
            if needles.iter().all(|n| log.contains(n)) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("{needles:?} did not all appear in the log:\n{log}");
            }
            thread::sleep(Duration::from_millis(50));
        }
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

/// The Landlock ABI version the kernel supports, or 0 without Landlock.
fn landlock_abi() -> i64 {
    // SAFETY: with a null attribute pointer and the VERSION flag,
    // landlock_create_ruleset(2) only reports the ABI version.
    let abi = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32, // LANDLOCK_CREATE_RULESET_VERSION
        )
    };
    abi.max(0)
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

/// The server's children log to its standard error, as it is the one
/// standard stream it leaves them: the feed fetcher's supervisor and the
/// worker it starts, and the script host, each say that their sandbox is
/// up.
#[test]
fn the_childrens_logs_reach_the_servers_stderr() {
    let dir = TempDir::with_prefix("kiki-child-logs").expect("create tempdir");
    let path = dir.path().join("stderr.log");
    let log = std::fs::File::create(&path).expect("create the log file");
    let mut kiki = Kiki::spawn_logging_to(&[], log, "info");
    kiki.wait_for_log(&path, &["\"feed-fetcher\"", "\"feed-worker\""]);
    kiki.wait_for_log(&path, &["\"script-host\""]);
    kiki.shutdown();
}

/// `RUST_LOG` decides what every process logs. The supervisor says it has
/// started the worker before the worker installs its sandbox, so once the
/// worker has said so, the supervisor's line would be there if `RUST_LOG`
/// let it through.
#[test]
fn rust_log_decides_what_the_children_log() {
    let dir = TempDir::with_prefix("kiki-child-logs").expect("create tempdir");
    let path = dir.path().join("stderr.log");
    let log = std::fs::File::create(&path).expect("create the log file");
    let mut kiki = Kiki::spawn_logging_to(&[], log, "warn,kiki_rss::sandbox=info");
    kiki.wait_for_log(&path, &["\"feed-worker\""]);
    let logged = std::fs::read_to_string(&path).expect("read the log");
    assert!(
        !logged.contains("worker started"),
        "the supervisor logged at INFO despite RUST_LOG:\n{logged}"
    );
    kiki.shutdown();
}

#[test]
fn no_sandbox_flag_still_runs() {
    let mut kiki = Kiki::spawn(&["--no-sandbox"]);
    kiki.get("/v1/feeds").assert_success();
    kiki.assert_still_running();
    kiki.shutdown();
}

/// Set to anything to make [`seccomp_probe`] run.
const SECCOMP_PROBE_ENV: &str = "KIKI_SECCOMP_PROBE";

/// Printed by [`seccomp_probe`] once every check that should pass has,
/// just before it makes the call that must kill it.
const SECCOMP_PROBE_PASSED: &str = "seccomp probe: allowed calls passed";

/// Not a test of its own: [`the_syscall_denylist_is_enforced`] runs it in
/// a process of its own. It installs the script host's sandbox, the
/// strictest there is, and checks what the seccomp filters' argument
/// rules let through, then tries to create a user namespace, which must
/// kill it.
#[test]
fn seccomp_probe() {
    use kiki_rss::sandbox::{apply, SandboxConfig};
    if std::env::var_os(SECCOMP_PROBE_ENV).is_none() {
        return;
    }
    apply(&SandboxConfig::script_host(false)).expect("install the script host sandbox");

    // `clone3` is refused, and the C library falls back to `clone`.
    thread::spawn(|| 42)
        .join()
        .map(|n| assert_eq!(n, 42))
        .expect("start a thread");

    // A denied ioctl fails, rather than killing.
    let ch: libc::c_char = 0;
    // SAFETY: `TIOCSTI` reads one byte from a valid pointer.
    let rc = unsafe { libc::ioctl(0, libc::TIOCSTI, &ch) };
    assert_eq!(rc, -1, "TIOCSTI was allowed");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOTTY)
    );
    println!("{SECCOMP_PROBE_PASSED}");
    let _ = std::io::Write::flush(&mut std::io::stdout());

    // Seccomp kills the process here.
    let flags = (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong;
    // SAFETY: a `fork`-like `clone` with no stack, so the child, if one
    // is created, runs on a copy of this stack and exits at once.
    let pid = unsafe { libc::syscall(libc::SYS_clone, flags, 0usize, 0usize, 0usize, 0usize) };
    if pid == 0 {
        // SAFETY: `_exit` is async-signal-safe, and skips the parent's
        // destructors.
        unsafe { libc::_exit(0) };
    }
    println!("seccomp probe: a user namespace was created");
}

/// The filters refuse denied ioctls, kill for namespaces, and let
/// threads start: run [`seccomp_probe`] in a fresh process, and
/// check that it got past the allowed calls and was then killed.
#[test]
fn the_syscall_denylist_is_enforced() {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "seccomp_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(SECCOMP_PROBE_ENV, "1")
        .output()
        .expect("run the probe");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains(SECCOMP_PROBE_PASSED),
        "probe failed the allowed calls ({}):\n{stdout}\n{stderr}",
        describe_exit(output.status)
    );
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGSYS),
        "probe was not killed for creating a user namespace ({}):\n{stdout}\n{stderr}",
        describe_exit(output.status)
    );
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
// Process metrics
// --------------------------------------------------------------------

/// The value of the `process`-labelled sample `name{process="<process>"}`
/// in a Prometheus exposition body.
fn process_metric(body: &str, name: &str, process: &str) -> Option<f64> {
    let prefix = format!("{name}{{process=\"{process}\"}}");
    body.lines()
        .find_map(|line| line.strip_prefix(&prefix)?.trim().parse().ok())
}

/// The sandboxed server can read its children's proportional memory.
/// That takes their Landlock domains to be nested inside the server's,
/// which they are only if it installed its Landlock rules before starting
/// them; started first, they would be beyond its reach, and their series
/// would be missing. Root could read them regardless, so the server runs
/// unprivileged.
#[test]
fn the_sandboxed_server_reports_its_childrens_memory() {
    let mut kiki = Kiki::spawn_unprivileged(&[]);
    let children = ["feed_fetcher", "script_host"];
    // The first sample is taken as the server starts.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        kiki.assert_still_running();
        let metrics = kiki.get("/metrics");
        let body = String::from_utf8_lossy(&metrics.body).into_owned();
        let missing: Vec<&str> = children
            .iter()
            .copied()
            .filter(|p| {
                process_metric(&body, "kiki_process_proportional_memory_bytes", p)
                    .is_none_or(|bytes| bytes <= 0.0)
            })
            .collect();
        if missing.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            panic!("no proportional memory reported for {missing:?}; last /metrics:\n{body}");
        }
        thread::sleep(Duration::from_millis(200));
    }
    kiki.shutdown();
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
            self.wait_for_fetcher_child(feed_fetcher::WORKER_SUBCOMMAND, not)
        }

        /// Wait for the supervisor's parser to exist and return its PID.
        /// It is started when there is first something to parse.
        fn wait_for_fetcher_parser(&mut self, not: Option<u32>) -> u32 {
            self.wait_for_fetcher_child(feed_fetcher::PARSER_SUBCOMMAND, not)
        }

        /// Wait for the supervisor's resolver to exist and return its PID.
        /// It is started when there is first something to look up.
        fn wait_for_fetcher_resolver(&mut self, not: Option<u32>) -> u32 {
            self.wait_for_fetcher_child(feed_fetcher::RESOLVER_SUBCOMMAND, not)
        }

        /// Wait for a child of the supervisor started with `subcommand`,
        /// other than `not`, and return its PID.
        fn wait_for_fetcher_child(&mut self, subcommand: &str, not: Option<u32>) -> u32 {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                self.assert_still_running();
                let supervisor = *self
                    .fetcher_supervisor_pids()
                    .first()
                    .expect("a feed fetcher supervisor");
                let children = child_pids_matching(supervisor, subcommand);
                if let Some(&c) = children.iter().find(|&&c| Some(c) != not) {
                    return c;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "no `{subcommand}` child of the feed fetcher appeared (excluding {not:?})"
                    );
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }

    /// How many seccomp filters `pid` runs under, on kernels that say
    /// (5.9+).
    fn seccomp_filters(pid: u32) -> Option<u32> {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .and_then(|v| v.trim().parse().ok())
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

    /// By default feeds are fetched by a sandboxed supervisor and worker,
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
        assert_stricter_than_the_supervisor(&kiki, worker, "worker");
        kiki.shutdown();
    }

    /// Each of the supervisor's children installs a sandbox of its own on
    /// top of the supervisor's, which shows as more seccomp filters, on
    /// kernels that say how many (5.9+).
    fn assert_stricter_than_the_supervisor(kiki: &Kiki, child: u32, what: &str) {
        let supervisor = kiki.fetcher_supervisor_pids()[0];
        if let (Some(s), Some(c)) = (seccomp_filters(supervisor), seccomp_filters(child)) {
            assert!(
                c > s,
                "the {what} runs under {c} seccomp filters, no more than the supervisor's {s}"
            );
        }
    }

    /// Set to anything to make [`worker_sandbox_probe`] run.
    const WORKER_PROBE_ENV: &str = "KIKI_WORKER_SANDBOX_PROBE";

    /// Printed by the worker probe once it has made a UDP socket, just
    /// before it tries to bind it.
    const WORKER_PROBE_SOCKET_MADE: &str = "worker sandbox probe: socket made";

    /// Not a test of its own: [`binding_kills_the_worker_but_not_the_supervisor`]
    /// runs it in a process of its own. It installs the worker's sandbox
    /// as the worker does, on top of the supervisor's, makes a UDP socket,
    /// which the worker may, then binds it, which must kill it.
    #[test]
    fn worker_sandbox_probe() {
        use kiki_rss::sandbox::{apply, SandboxConfig};
        if std::env::var_os(WORKER_PROBE_ENV).is_none() {
            return;
        }
        apply(&SandboxConfig::feed_fetcher(false)).expect("install the fetcher sandbox");
        // The supervisor may bind, for the resolver it starts.
        std::net::UdpSocket::bind("127.0.0.1:0").expect("the supervisor may bind");
        apply(&SandboxConfig::feed_worker(false)).expect("install the worker sandbox");

        // SAFETY: a plain `socket` call; the descriptor is left open.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        assert!(fd >= 0, "a UDP socket was refused");
        println!("{WORKER_PROBE_SOCKET_MADE}");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        // Seccomp kills the process here.
        let _ = std::net::UdpSocket::bind("127.0.0.1:0");
        println!("worker sandbox probe: binding was allowed");
    }

    /// The supervisor may bind sockets, for the resolver it starts, but the
    /// worker it also starts may not: run [`worker_sandbox_probe`] in a
    /// fresh process and check that it was killed for binding.
    #[test]
    fn binding_kills_the_worker_but_not_the_supervisor() {
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "fetch_isolation::worker_sandbox_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(WORKER_PROBE_ENV, "1")
            .output()
            .expect("run the probe");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains(WORKER_PROBE_SOCKET_MADE),
            "probe failed before binding ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGSYS),
            "probe was not killed for binding ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
    }

    /// Set to anything to make [`supervisor_exec_probe`] run.
    const EXEC_PROBE_ENV: &str = "KIKI_SUPERVISOR_EXEC_PROBE";

    /// Printed by the exec probe once it has run this executable again
    /// under the supervisor's sandbox, and been refused another.
    const EXEC_PROBE_PASSED: &str = "supervisor exec probe: passed";

    /// Not a test of its own: [`the_supervisor_may_execute_only_kiki`]
    /// runs it in a process of its own. It installs the supervisor's
    /// sandbox, then runs this executable again, as the supervisor runs
    /// kiki to start its children, and tries to run a copy of it: one
    /// that certainly exists and could run, but is not the file the
    /// sandbox lets the supervisor execute.
    #[test]
    fn supervisor_exec_probe() {
        use kiki_rss::sandbox::{apply, SandboxConfig};
        if std::env::var_os(EXEC_PROBE_ENV).is_none() {
            return;
        }
        let exe = std::env::current_exe().expect("test binary");
        let dir = TempDir::with_prefix("kiki-exec-probe").expect("create tempdir");
        let copy = dir.path().join("copy");
        std::fs::copy(&exe, &copy).expect("copy this executable");

        apply(&SandboxConfig::feed_fetcher(false)).expect("install the fetcher sandbox");

        // With the standard streams inherited, as the supervisor's children
        // have them: the sandbox grants no `/dev/null`.
        let listed = Command::new(&exe)
            .arg("--list")
            .status()
            .expect("run this executable again");
        assert!(
            listed.success(),
            "running this executable again failed: {listed}"
        );

        let err = Command::new(&copy)
            .arg("--list")
            .status()
            .expect_err("a copy of this executable was run");
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        println!("{EXEC_PROBE_PASSED}");
    }

    /// The supervisor starts its children by running the kiki executable
    /// again, and Landlock lets it execute nothing else: run
    /// [`supervisor_exec_probe`] in a fresh process.
    #[test]
    fn the_supervisor_may_execute_only_kiki() {
        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "fetch_isolation::supervisor_exec_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(EXEC_PROBE_ENV, "1")
            .output()
            .expect("run the probe");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(EXEC_PROBE_PASSED),
            "probe failed ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
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

    /// A feed named by hostname rather than IP is fetched, with the lookup
    /// done by the fetcher's resolver: a process of its own, with a
    /// stricter sandbox, and with none of the server's environment but
    /// what the C library's resolver reads.
    #[test]
    fn hostnames_are_resolved_by_the_resolver() {
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

        let worker = kiki.wait_for_fetcher_worker(None);
        let resolver = kiki.wait_for_fetcher_resolver(None);
        assert_ne!(worker, resolver);
        assert_eq!(seccomp_mode(resolver), Some(2));
        assert_stricter_than_the_supervisor(&kiki, resolver, "resolver");
        let environ = std::fs::read(format!("/proc/{resolver}/environ")).expect("read environ");
        let kept: Vec<String> = environ
            .split(|&b| b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect();
        assert!(
            kept.iter().all(|v| {
                let name = v.split('=').next().unwrap_or_default();
                [
                    "LOCALDOMAIN",
                    "RES_OPTIONS",
                    "HOSTALIASES",
                    "RUST_LOG",
                    feed_fetcher::RESOLVER_FD_ENV,
                ]
                .contains(&name)
            }),
            "the resolver got more of the environment than it needs: {kept:?}"
        );
        kiki.shutdown();
    }

    /// Set to a directory to make [`resolver_sandbox_probe`] run.
    const RESOLVER_PROBE_ENV: &str = "KIKI_RESOLVER_SANDBOX_PROBE";

    /// Set to the address of a TCP listener on a port other than DNS's,
    /// for [`resolver_sandbox_probe`] to fail to connect to.
    const RESOLVER_PROBE_TCP_ENV: &str = "KIKI_RESOLVER_SANDBOX_PROBE_TCP";

    const RESOLVER_PROBE_PASSED: &str = "resolver sandbox probe passed";

    /// Not a test of its own: [`the_resolver_sandbox_is_enforced`] runs it
    /// in a process of its own. It installs the resolver's sandbox as the
    /// resolver does, on top of the fetcher's, and checks that hostnames
    /// still resolve, while files other than the resolver's configuration,
    /// Unix sockets, and TCP ports other than DNS's are out of reach.
    #[test]
    fn resolver_sandbox_probe() {
        use kiki_rss::sandbox::{apply, SandboxConfig};
        use std::net::ToSocketAddrs;
        let Some(dir) = std::env::var_os(RESOLVER_PROBE_ENV).map(PathBuf::from) else {
            return;
        };
        let tcp: SocketAddr = std::env::var(RESOLVER_PROBE_TCP_ENV)
            .expect("probe TCP address")
            .parse()
            .expect("valid probe TCP address");
        // Asked before the sandbox goes up.
        let abi = landlock_abi();
        apply(&SandboxConfig::feed_fetcher(false)).expect("install the fetcher sandbox");
        apply(&SandboxConfig::feed_resolver(false)).expect("install the resolver sandbox");

        // What the resolver is for still works.
        let addrs: Vec<SocketAddr> = ("localhost", 0)
            .to_socket_addrs()
            .expect("resolve localhost")
            .collect();
        assert!(!addrs.is_empty());

        // That includes what musl's resolver does to query a name server:
        // bind a UDP socket to port 0, then send from it. A name that is in
        // /etc/hosts never gets that far, so it is done by hand.
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a UDP socket");
        let to_itself = udp.local_addr().expect("local address");
        udp.send_to(b"query", to_itself)
            .expect("send from a bound UDP socket");

        // No Unix sockets, which would reach the server's API socket; no
        // netlink ones either.
        let err = UnixStream::connect(dir.join("api.sock")).expect_err("Unix socket allowed");
        assert_eq!(err.raw_os_error(), Some(libc::EACCES), "{err}");
        // SAFETY: a plain `socket` call; the descriptor, if any, is closed.
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, 0) };
        assert!(fd < 0, "a netlink socket was allowed");

        if abi >= 1 {
            std::fs::read(dir.join("secret")).expect_err("reading a file was allowed");
            std::fs::read_dir("/etc/ssl").expect_err("listing the trust stores was allowed");
        }
        if abi >= 4 {
            let err = std::net::TcpStream::connect(tcp).expect_err("TCP connect was allowed");
            assert_eq!(err.raw_os_error(), Some(libc::EACCES), "{err}");
        } else {
            println!("Landlock ABI < 4; skipping the TCP port check");
        }
        println!("{RESOLVER_PROBE_PASSED}");
    }

    /// The resolver's sandbox really allows only what it claims to: run
    /// [`resolver_sandbox_probe`] in a fresh process, and check it passed
    /// rather than died.
    #[test]
    fn the_resolver_sandbox_is_enforced() {
        let dir = TempDir::with_prefix("kiki-resolver-probe").expect("create tempdir");
        std::fs::write(dir.path().join("secret"), "hidden").expect("write secret");
        let api = std::os::unix::net::UnixListener::bind(dir.path().join("api.sock"))
            .expect("bind the API socket");
        let tcp = TcpListener::bind("127.0.0.1:0").expect("bind a TCP listener");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "fetch_isolation::resolver_sandbox_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(RESOLVER_PROBE_ENV, dir.path())
            .env(
                RESOLVER_PROBE_TCP_ENV,
                tcp.local_addr().expect("addr").to_string(),
            )
            .output()
            .expect("run the probe");
        drop(api);

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(RESOLVER_PROBE_PASSED),
            "probe failed ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
    }

    /// Feeds are parsed in a process of their own, which installs a
    /// stricter sandbox on top of the one it inherits from the
    /// supervisor.
    #[test]
    fn feeds_are_parsed_in_a_separate_process_with_a_stricter_sandbox() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);
        let feed_id = create_feed(&mut kiki, addr);
        refresh_until_entries(&mut kiki, feed_id, 1);

        let worker = kiki.wait_for_fetcher_worker(None);
        let parser = kiki.wait_for_fetcher_parser(None);
        assert_ne!(worker, parser);
        assert_eq!(seccomp_mode(parser), Some(2));
        // The server's environment, which may hold credentials, is not
        // passed on: the parser gets only what the supervisor tells it.
        let environ = std::fs::read(format!("/proc/{parser}/environ")).expect("read environ");
        let names: Vec<String> = environ
            .split(|&b| b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| {
                String::from_utf8_lossy(v)
                    .split('=')
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert!(
            names.iter().all(|name| {
                [
                    "RUST_LOG",
                    feed_fetcher::PARSER_FD_ENV,
                    feed_fetcher::PARSER_THREADS_ENV,
                ]
                .contains(&name.as_str())
            }),
            "the parser got more of the environment than it needs: {names:?}"
        );
        assert_stricter_than_the_supervisor(&kiki, parser, "parser");
        kiki.shutdown();
    }

    /// A parser that dies is replaced, and parsing carries on.
    #[test]
    fn a_killed_parser_is_replaced_and_fetching_continues() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = Kiki::spawn(&[]);
        let first_feed = create_feed(&mut kiki, addr);
        refresh_until_entries(&mut kiki, first_feed, 1);
        let worker = kiki.wait_for_fetcher_worker(None);
        let first = kiki.wait_for_fetcher_parser(None);

        // SAFETY: SIGKILL to a process of our own; at worst ESRCH.
        unsafe {
            libc::kill(first as libc::pid_t, libc::SIGKILL);
        }
        assert!(wait_for_exit(first, Duration::from_secs(5)));

        let (other, _other_server) = spawn_local_rss_server();
        let second_feed = create_feed(&mut kiki, other);
        refresh_until_entries(&mut kiki, second_feed, 1);
        let second = kiki.wait_for_fetcher_parser(Some(first));
        assert_ne!(first, second);
        assert_eq!(
            kiki.wait_for_fetcher_worker(None),
            worker,
            "the worker must survive the parser"
        );
        kiki.shutdown();
    }

    /// Set to a directory to make [`parser_sandbox_probe`] run.
    const PROBE_ENV: &str = "KIKI_PARSER_SANDBOX_PROBE";

    /// Printed by the probe once the filesystem checks have passed, just
    /// before it tries to make a socket.
    const PROBE_FILES_DENIED: &str = "parser sandbox probe: files denied";

    /// Not a test of its own: [`the_parser_sandbox_is_enforced`] runs it in
    /// a process of its own. It installs the parser's sandbox as the
    /// parser does, on top of the fetcher's, checks that no file can be
    /// read, then tries to make a socket, which must kill it.
    #[test]
    fn parser_sandbox_probe() {
        use kiki_rss::sandbox::{apply, SandboxConfig};
        let Some(dir) = std::env::var_os(PROBE_ENV).map(PathBuf::from) else {
            return;
        };
        // Asked before the sandbox goes up.
        let abi = landlock_abi();
        apply(&SandboxConfig::feed_fetcher(false)).expect("install the fetcher sandbox");
        apply(&SandboxConfig::feed_parser(false)).expect("install the parser sandbox");

        // The fetcher may read the TLS trust stores; the parser may not
        // read anything, where the kernel has Landlock.
        if abi >= 1 {
            std::fs::read(dir.join("secret")).expect_err("reading a file was allowed");
            std::fs::read_dir("/etc/ssl").expect_err("listing the trust stores was allowed");
        } else {
            println!("Landlock unavailable; skipping the filesystem checks");
        }
        println!("{PROBE_FILES_DENIED}");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        // Seccomp kills the process here.
        let _ = std::net::UdpSocket::bind("127.0.0.1:0");
        println!("parser sandbox probe: a socket was allowed");
    }

    /// The parser's sandbox really denies what it claims to: run
    /// [`parser_sandbox_probe`] in a fresh process, and check that it got
    /// past the filesystem checks and was then killed for making a socket.
    #[test]
    fn the_parser_sandbox_is_enforced() {
        let dir = TempDir::with_prefix("kiki-parser-probe").expect("create tempdir");
        std::fs::write(dir.path().join("secret"), "hidden").expect("write secret");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "fetch_isolation::parser_sandbox_probe",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(PROBE_ENV, dir.path())
            .output()
            .expect("run the probe");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stdout.contains(PROBE_FILES_DENIED),
            "probe failed the filesystem checks ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
        assert_eq!(
            output.status.signal(),
            Some(libc::SIGSYS),
            "probe was not killed for making a socket ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
    }

    /// None of the fetcher's processes may outlive the server.
    #[test]
    fn the_fetcher_exits_with_the_server() {
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
        let supervisor = kiki.fetcher_supervisor_pids()[0];
        let worker = kiki.wait_for_fetcher_worker(None);
        let parser = kiki.wait_for_fetcher_parser(None);
        let resolver = kiki.wait_for_fetcher_resolver(None);
        kiki.shutdown();

        for pid in [supervisor, worker, parser, resolver] {
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

#[cfg(feature = "metrics")]
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

        /// Block until `kiki_plugins_loaded` reaches `n`, so a test never
        /// races a reload that is still in flight.
        fn wait_for_plugins_loaded(&mut self, n: u64, timeout: Duration) {
            let deadline = Instant::now() + timeout;
            loop {
                self.assert_still_running();
                let metrics = self.get("/metrics");
                let last = String::from_utf8_lossy(&metrics.body).into_owned();
                if metric_value(&last, "kiki_plugins_loaded") == Some(n as f64) {
                    return;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "kiki_plugins_loaded never reached {n} within {timeout:?}; \
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

    /// Spawn `kiki serve` with a plugin whose entrypoint is `source`
    /// installed, and wait for the server to report it loaded.
    fn spawn_with_script(extra_args: &[&str], source: &str) -> Kiki {
        spawn_with_configured_script(extra_args, source, serde_json::json!({}))
    }

    /// As [`spawn_with_script`], giving the plugin the config `config`.
    /// Plugins are only discovered when the server starts, so the plugin
    /// is installed before it is spawned.
    fn spawn_with_configured_script(
        extra_args: &[&str],
        source: &str,
        config: serde_json::Value,
    ) -> Kiki {
        let manifest = serde_json::json!({
            "name": "test-plugin",
            "version": "1.0.0",
            "engine": "lua",
            "config": config,
        });
        let manifest = toml::to_string(&manifest).expect("serialize manifest.toml");
        let mut kiki = Kiki::spawn_with(extra_args, |home| {
            let plugin = home.join("plugins").join("user").join("test-plugin");
            std::fs::create_dir_all(&plugin).expect("create plugin directory");
            std::fs::write(plugin.join("main.lua"), source).expect("write main.lua");
            std::fs::write(plugin.join("manifest.toml"), manifest).expect("write manifest.toml");
        });
        kiki.wait_for_plugins_loaded(1, Duration::from_secs(10));
        kiki
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

    /// End to end: a script installed as a plugin transforms a real
    /// entry, with the VM in the sandboxed child and the database in the
    /// server. This is the test that fails if anything in the IPC path —
    /// framing, serialisation, the child's sandbox — is wrong.
    #[test]
    fn an_isolated_script_transforms_an_ingested_entry() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = spawn_with_script(&[], TITLE_STAMPING_SCRIPT);
        assert_eq!(kiki.script_host_pids().len(), 1);

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[scripted] "),
            "entry title was not transformed by the isolated script host: {title:?}"
        );
        kiki.shutdown();
    }

    /// The server skips sending the script host events nothing handles,
    /// going by the handlers the host reports with each response. A handler
    /// registered after loading, here by a `plugin.load` handler, must
    /// still be reached.
    #[test]
    fn a_handler_registered_after_loading_still_runs() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = spawn_with_script(
            &[],
            r#"
            kiki.on("plugin.load", function()
                kiki.on("entry.ingest", function(entry)
                    entry.title = "[late] " .. entry.title
                    return entry
                end)
            end)
            "#,
        );

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[late] "),
            "a handler registered by plugin.load did not run: {title:?}"
        );
        kiki.shutdown();
    }

    /// A script's config crosses the IPC channel to the script host and
    /// reaches the script's top-level chunk.
    #[test]
    fn an_isolated_script_receives_its_config() {
        let (addr, _server) = spawn_local_rss_server();
        let mut kiki = spawn_with_configured_script(
            &[],
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
        let mut kiki = spawn_with_script(
            &[],
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
        let mut kiki = spawn_with_script(&["--no-script-isolation"], TITLE_STAMPING_SCRIPT);

        assert!(
            kiki.script_host_pids().is_empty(),
            "--no-script-isolation must not spawn a script host"
        );

        let feed_id = create_local_feed(&mut kiki, addr);
        let title = refresh_and_read_title(&mut kiki, feed_id);
        assert!(
            title.starts_with("[scripted] "),
            "in-process script did not transform the entry: {title:?}"
        );
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
        let mut kiki = spawn_with_script(
            &[],
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

    /// Plugins in the sandboxed host can reach the server through the
    /// `kiki` API: their store, tagging entries, and scans of stored
    /// entries, started from `plugin.load`. Editing the plugin reloads it,
    /// which runs `plugin.load` again.
    #[test]
    fn plugins_in_the_host_can_scan_and_tag_stored_entries() {
        const SCRIPT: &str = r#"
            local pattern = "spam"
            kiki.on("plugin.load", function()
                kiki.store.set("loads", (kiki.store.get("loads") or 0) + 1)
                kiki.entries.scan(function(entry)
                    if entry.title:find(pattern) then
                        table.insert(entry.tags, "system:hidden")
                    end
                    if entry.title:find("tagme") then
                        kiki.entries.tag(entry.id, "tagged")
                    end
                    return entry
                end)
            end)
        "#;
        let manifest = "name = 'scanner'\nversion = '1.0.0'\nengine = 'lua'\n";
        let mut kiki = Kiki::spawn_with(&[], |home| {
            let conn = rusqlite::Connection::open(home.join("kiki.db")).expect("open db");
            conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])
                .expect("insert feed");
            let feed = conn.last_insert_rowid();
            for title in ["ham", "spam", "tagme"] {
                conn.execute(
                    "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url)
                     VALUES (?1, 'rss', ?2, 0, ?2, 'u')",
                    rusqlite::params![feed, title],
                )
                .expect("insert entry");
            }
            let plugin = home.join("plugins").join("user").join("scanner");
            std::fs::create_dir_all(&plugin).expect("create plugin directory");
            std::fs::write(plugin.join("manifest.toml"), manifest).expect("write manifest");
            std::fs::write(plugin.join("main.lua"), SCRIPT).expect("write main.lua");
        });
        assert_eq!(kiki.script_host_pids().len(), 1);

        let home = kiki._dir.path().to_path_buf();
        let tags_of = |title: &str| -> Vec<String> {
            let conn = rusqlite::Connection::open(home.join("kiki.db")).expect("open db");
            let mut stmt = conn
                .prepare(
                    "SELECT t.name FROM entries e
                     JOIN entry_tags et ON et.entry_id = e.id
                     JOIN tags t ON t.id = et.tag_id
                     WHERE e.title = ?1 ORDER BY t.name",
                )
                .expect("prepare");
            stmt.query_map([title], |row| row.get(0))
                .expect("query")
                .collect::<Result<_, _>>()
                .expect("rows")
        };
        let wait_for = |what: &str, cond: &dyn Fn() -> bool| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !cond() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                thread::sleep(Duration::from_millis(50));
            }
        };

        wait_for("spam to be hidden", &|| {
            tags_of("spam") == ["system:hidden"]
        });
        wait_for("tagme to be tagged", &|| tags_of("tagme") == ["tagged"]);
        assert!(tags_of("ham").is_empty());
        let loads = || -> String {
            let conn = rusqlite::Connection::open(home.join("kiki.db")).expect("open db");
            conn.query_row(
                "SELECT value FROM plugin_store WHERE plugin = 'scanner' AND key = 'loads'",
                [],
                |row| row.get(0),
            )
            .unwrap_or_default()
        };
        assert_eq!(loads(), "1");

        // Editing the plugin reloads it, through the plugins directory
        // watcher, and its new scan hides ham too.
        std::fs::write(
            home.join("plugins")
                .join("user")
                .join("scanner")
                .join("main.lua"),
            SCRIPT.replace(r#"local pattern = "spam""#, r#"local pattern = "ham""#),
        )
        .expect("rewrite main.lua");
        wait_for("ham to be hidden", &|| tags_of("ham") == ["system:hidden"]);
        assert_eq!(loads(), "2");

        assert_eq!(kiki.script_host_pids().len(), 1);
        kiki.assert_still_running();
        kiki.shutdown();
    }

    /// A scan over entries too large to send to the host together, as
    /// full-text feeds make them, goes through every entry. One too large
    /// to send at all is skipped, and the scan carries on past it.
    #[test]
    fn scans_of_large_entries_go_through_every_entry() {
        const SCRIPT: &str = r#"
            kiki.on("plugin.load", function()
                kiki.entries.scan(function(entry)
                    table.insert(entry.tags, "system:hidden")
                    return entry
                end, function(summary)
                    kiki.store.set("scanned", summary.scanned)
                end)
            end)
        "#;
        let manifest = "name = 'scanner'\nversion = '1.0.0'\nengine = 'lua'\n";
        // 25 entries of 480 KiB, about 12 MB: one dispatch's worth, which
        // used to fail the scan for exceeding the 8 MiB frame limit.
        let large = "x".repeat(480 * 1024);
        let huge = "x".repeat(9 * 1024 * 1024);
        let mut kiki = Kiki::spawn_with(&[], |home| {
            let conn = rusqlite::Connection::open(home.join("kiki.db")).expect("open db");
            conn.execute("INSERT INTO feeds (title) VALUES ('f')", [])
                .expect("insert feed");
            let feed = conn.last_insert_rowid();
            let insert = |guid: String, content: &str| {
                conn.execute(
                    "INSERT INTO entries (feed_id, syndication_format, guid, published_at, title, url, content)
                     VALUES (?1, 'rss', ?2, 0, ?2, 'u', ?3)",
                    rusqlite::params![feed, guid, content],
                )
                .expect("insert entry");
            };
            for i in 0..25 {
                insert(format!("large-{i}"), &large);
            }
            insert("huge".to_string(), &huge);
            insert("small".to_string(), "x");
            let plugin = home.join("plugins").join("user").join("scanner");
            std::fs::create_dir_all(&plugin).expect("create plugin directory");
            std::fs::write(plugin.join("manifest.toml"), manifest).expect("write manifest");
            std::fs::write(plugin.join("main.lua"), SCRIPT).expect("write main.lua");
        });

        let db = kiki._dir.path().join("kiki.db");
        let deadline = Instant::now() + Duration::from_secs(30);
        let scanned = loop {
            kiki.assert_still_running();
            let conn = rusqlite::Connection::open(&db).expect("open db");
            let scanned: Option<String> = conn
                .query_row(
                    "SELECT value FROM plugin_store WHERE plugin = 'scanner' AND key = 'scanned'",
                    [],
                    |row| row.get(0),
                )
                .ok();
            if let Some(scanned) = scanned {
                break scanned;
            }
            assert!(Instant::now() < deadline, "the scan never finished");
            thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(scanned, "27");

        let conn = rusqlite::Connection::open(&db).expect("open db");
        let hidden: Vec<String> = conn
            .prepare(
                "SELECT e.guid FROM entries e
                 JOIN entry_tags et ON et.entry_id = e.id
                 JOIN tags t ON t.id = et.tag_id
                 WHERE t.name = 'system:hidden' ORDER BY e.id",
            )
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        let mut expected: Vec<String> = (0..25).map(|i| format!("large-{i}")).collect();
        expected.push("small".to_string());
        assert_eq!(hidden, expected);

        assert_eq!(kiki.script_host_pids().len(), 1);
        kiki.assert_still_running();
        kiki.shutdown();
    }
}

// --------------------------------------------------------------------
// Web UI sandbox
// --------------------------------------------------------------------

#[cfg(feature = "web-ui")]
mod web_ui {
    use super::*;
    use std::net::TcpStream;

    /// A running `kiki web` subprocess, with its `kiki serve` child, in its
    /// own temp data directory. Dropped instances are killed and reaped.
    struct KikiWeb {
        _dir: TempDir,
        socket: PathBuf,
        addr: SocketAddr,
        child: Option<Child>,
    }

    impl KikiWeb {
        /// Spawn `kiki web` with the given extra flags and wait for it to
        /// serve its index page.
        fn spawn(extra_args: &[&str]) -> Self {
            let dir = TempDir::with_prefix("kiki-web-sandbox-test").expect("create tempdir");
            let init_status = Command::new(KIKI_BIN)
                .args(["init", "--no-default-plugins"])
                .env("KIKI_HOME", dir.path())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn kiki init");
            assert!(init_status.success(), "kiki init failed: {init_status:?}");

            // Reserve a free port, then hand it to kiki.
            let addr = TcpListener::bind("127.0.0.1:0")
                .and_then(|l| l.local_addr())
                .expect("reserve a port");
            let socket = dir.path().join("kiki.sock");
            let child = Command::new(KIKI_BIN)
                .current_dir(dir.path())
                .arg("web")
                .arg("--listen")
                .arg(addr.to_string())
                .arg("--uds")
                .arg(&socket)
                .args(extra_args)
                .env("KIKI_HOME", dir.path())
                .env("RUST_LOG", "warn")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn kiki web");

            let mut web = KikiWeb {
                _dir: dir,
                socket,
                addr,
                child: Some(child),
            };
            web.wait_until_ready(Duration::from_secs(10));
            web
        }

        fn pid(&self) -> u32 {
            self.child.as_ref().expect("child").id()
        }

        /// Poll until the index page renders, which takes both the web UI
        /// and the server behind it.
        fn wait_until_ready(&mut self, timeout: Duration) {
            let start = Instant::now();
            while start.elapsed() < timeout {
                self.assert_still_running();
                if TcpStream::connect(self.addr).is_ok() && self.get("/").status == 200 {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            panic!("kiki web did not serve its index within {timeout:?}");
        }

        fn assert_still_running(&mut self) {
            let child = self.child.as_mut().expect("child");
            if let Some(status) = child.try_wait().expect("try_wait") {
                panic!(
                    "kiki web exited unexpectedly mid-test: {}",
                    describe_exit(status)
                );
            }
        }

        /// GET `path` from the web UI.
        fn get(&self, path: &str) -> HttpResponse {
            let mut stream = TcpStream::connect(self.addr).expect("connect to the web UI");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set read timeout");
            let req =
                format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            stream.write_all(req.as_bytes()).expect("write request");
            let mut raw = Vec::new();
            stream.read_to_end(&mut raw).expect("read response");
            let line = raw.split(|&b| b == b'\n').next().expect("empty response");
            let status = std::str::from_utf8(line)
                .expect("non-utf8 status line")
                .split_whitespace()
                .nth(1)
                .and_then(|c| c.parse().ok())
                .expect("status code");
            let split = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map_or(raw.len(), |p| p + 4);
            HttpResponse {
                status,
                body: raw[split..].to_vec(),
            }
        }

        /// The `kiki serve` child's PID.
        fn server_pid(&self) -> u32 {
            *child_pids_matching(self.pid(), "serve")
                .first()
                .expect("a kiki serve child")
        }

        /// Wait for `kiki web` to exit and return its status.
        fn wait(mut self) -> std::process::ExitStatus {
            let mut child = self.child.take().expect("child");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().expect("try_wait") {
                    return status;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    panic!("kiki web did not exit");
                }
                thread::sleep(Duration::from_millis(50));
            }
        }

        /// Send SIGTERM and assert a clean exit, with the server gone too.
        fn shutdown(self) {
            let server = self.server_pid();
            // SAFETY: signalling our own, unreaped child is always safe.
            unsafe {
                libc::kill(self.pid() as libc::pid_t, libc::SIGTERM);
            }
            let status = self.wait();
            assert!(status.success(), "kiki web: {}", describe_exit(status));
            assert!(
                wait_for_exit(server, Duration::from_secs(5)),
                "the kiki serve child outlived the web UI"
            );
        }
    }

    /// How many seccomp filters `pid` runs under.
    ///
    /// Compared against the test process's own count rather than zero, and
    /// rather than reading the `Seccomp` mode: a test harness may itself
    /// run under a filter (the Nix build sandbox does), which every process
    /// here inherits.
    fn seccomp_filters(pid: u32) -> u32 {
        let status =
            std::fs::read_to_string(format!("/proc/{pid}/status")).expect("read /proc status");
        status
            .lines()
            .find_map(|l| l.strip_prefix("Seccomp_filters:"))
            .and_then(|v| v.trim().parse().ok())
            .expect("a Seccomp_filters line (Linux 5.9+)")
    }

    impl Drop for KikiWeb {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    /// Every page renders with the web UI sandboxed — a newly denied
    /// syscall on a handler's path would kill it with SIGSYS — and the
    /// sandbox is really installed.
    #[test]
    fn the_sandboxed_web_ui_serves_every_page() {
        let (addr, _rss) = spawn_local_rss_server();
        let mut web = KikiWeb::spawn(&[]);
        assert!(
            seccomp_filters(web.pid()) > seccomp_filters(std::process::id()),
            "the web UI installed no seccomp filter of its own"
        );

        let feed = http_request(
            &web.socket,
            "POST",
            "/v1/feeds/create",
            Some(&format!(
                r#"{{"title":"web sandbox","url":"http://{addr}/feed.xml"}}"#
            )),
        )
        .assert_success()
        .json()["id"]
            .as_i64()
            .expect("feed id");
        http_request(
            &web.socket,
            "POST",
            &format!("/v1/feeds/refresh/{feed}"),
            Some(""),
        )
        .assert_success();
        let deadline = Instant::now() + Duration::from_secs(10);
        let entry = loop {
            web.assert_still_running();
            let entries = http_request(&web.socket, "GET", "/v1/entries?limit=1", None)
                .assert_success()
                .json();
            if let Some(id) = entries["entries"][0]["id"].as_i64() {
                break id;
            }
            assert!(Instant::now() < deadline, "the entry never appeared");
            thread::sleep(Duration::from_millis(200));
        };

        for path in [
            "/".to_owned(),
            "/feeds".to_owned(),
            format!("/feeds/{feed}"),
            format!("/entries/{entry}"),
            "/tags".to_owned(),
            "/search?q=hello".to_owned(),
            "/plugins".to_owned(),
        ] {
            let resp = web.get(&path);
            web.assert_still_running();
            assert_eq!(resp.status, 200, "{path}: {}", resp.body_str());
        }
        assert!(web
            .get(&format!("/entries/{entry}"))
            .body_str()
            .contains("hello from a sandboxed fetch"));
        web.shutdown();
    }

    #[test]
    fn no_sandbox_leaves_the_web_ui_unsandboxed() {
        let mut web = KikiWeb::spawn(&["--no-sandbox"]);
        assert_eq!(
            seccomp_filters(web.pid()),
            seccomp_filters(std::process::id()),
            "the web UI installed a seccomp filter despite --no-sandbox"
        );
        web.get("/feeds").assert_success();
        web.assert_still_running();
        web.shutdown();
    }

    /// Set by [`the_web_ui_sandbox_is_enforced`] when it re-runs this test
    /// binary to probe the sandbox; names the directory holding the probe's
    /// targets.
    const PROBE_ENV: &str = "KIKI_WEB_UI_SANDBOX_PROBE";

    /// The address of a TCP listener for the probe to try to reach.
    const PROBE_TCP_ENV: &str = "KIKI_WEB_UI_SANDBOX_PROBE_TCP";

    /// Printed by the probe once every check has passed.
    const PROBE_PASSED: &str = "web UI sandbox probe passed";

    /// The other half of [`the_web_ui_sandbox_is_enforced`]: does nothing
    /// unless that test ran it, in which case it installs the web UI's
    /// sandbox on itself and checks what gets through.
    #[test]
    fn web_ui_sandbox_probe() {
        let Some(dir) = std::env::var_os(PROBE_ENV).map(PathBuf::from) else {
            return;
        };
        let tcp: SocketAddr = std::env::var(PROBE_TCP_ENV)
            .expect("probe TCP address")
            .parse()
            .expect("valid probe TCP address");
        let secret = dir.join("secret");

        // Asked before the sandbox goes up.
        let abi = landlock_abi();
        kiki_rss::sandbox::apply(&kiki_rss::sandbox::SandboxConfig::web_ui(false))
            .expect("install the web UI sandbox");

        // What the web UI needs still works: reaching the API's socket.
        UnixStream::connect(dir.join("api.sock")).expect("connect to the API socket");

        // Nothing else on the network does: seccomp refuses the socket.
        let err = std::net::TcpStream::connect(tcp).expect_err("TCP connect was allowed");
        assert_eq!(err.raw_os_error(), Some(libc::EACCES), "{err}");
        TcpListener::bind("127.0.0.1:0").expect_err("TCP bind was allowed");
        std::net::UdpSocket::bind("127.0.0.1:0").expect_err("UDP socket was allowed");

        // Nor does the filesystem, where the kernel has Landlock.
        if abi >= 1 {
            std::fs::read(&secret).expect_err("reading a file was allowed");
            std::fs::read_dir("/").expect_err("listing / was allowed");
            std::fs::write(dir.join("new"), "x").expect_err("creating a file was allowed");
        } else {
            println!("Landlock unavailable; skipping the filesystem checks");
        }
        println!("{PROBE_PASSED}");
    }

    /// The web UI's sandbox really denies what it claims to: run
    /// [`web_ui_sandbox_probe`] in a fresh process, which the sandbox then
    /// confines, and check it passed rather than died with SIGSYS.
    #[test]
    fn the_web_ui_sandbox_is_enforced() {
        let dir = TempDir::with_prefix("kiki-web-probe").expect("create tempdir");
        std::fs::write(dir.path().join("secret"), "hidden").expect("write secret");
        let api = std::os::unix::net::UnixListener::bind(dir.path().join("api.sock"))
            .expect("bind the API socket");
        let tcp = TcpListener::bind("127.0.0.1:0").expect("bind a TCP listener");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "web_ui::web_ui_sandbox_probe", "--nocapture"])
            .env(PROBE_ENV, dir.path())
            .env(PROBE_TCP_ENV, tcp.local_addr().expect("addr").to_string())
            .output()
            .expect("run the probe");
        drop(api);

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success() && stdout.contains(PROBE_PASSED),
            "probe failed ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
    }

    /// The web UI notices, from inside its sandbox, that the server it
    /// started has died, and exits with an error rather than a signal.
    #[test]
    fn the_sandboxed_web_ui_exits_with_its_server() {
        let web = KikiWeb::spawn(&[]);
        // SAFETY: SIGKILL to a process this test tree owns.
        unsafe {
            libc::kill(web.server_pid() as libc::pid_t, libc::SIGKILL);
        }
        let status = web.wait();
        assert_eq!(
            status.code(),
            Some(1),
            "kiki web: {}",
            describe_exit(status)
        );
    }
}

// --------------------------------------------------------------------
// Landlock scoping: signals and abstract Unix sockets
// --------------------------------------------------------------------

mod landlock_scoping {
    use super::*;
    use kiki_rss::sandbox::{self, SandboxConfig};
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr as UnixAddr, UnixDatagram, UnixListener};

    /// Set by [`run_probe`] when it re-runs this test binary to probe a
    /// profile's scoping; names the profile.
    const PROBE_ENV: &str = "KIKI_SCOPE_PROBE";

    /// The PID of a process outside the probe's sandbox: the test that
    /// started it.
    const PROBE_PID_ENV: &str = "KIKI_SCOPE_PROBE_PID";

    /// The abstract name of a stream listener outside the probe's sandbox.
    /// A datagram socket waits at the same name with [`DGRAM_SUFFIX`].
    const PROBE_SOCKET_ENV: &str = "KIKI_SCOPE_PROBE_SOCKET";

    const DGRAM_SUFFIX: &str = "-dgram";

    /// What the probe sends over the datagram socket it connected before
    /// its sandbox went up, as the server does with `$NOTIFY_SOCKET`.
    const NOTIFY_MESSAGE: &[u8] = b"READY=1";

    /// Printed by the probe once every check has passed.
    const PROBE_PASSED: &str = "scope probe passed";

    /// Printed instead when the kernel predates scoping (Linux 6.12).
    const PROBE_SKIPPED: &str = "scope probe skipped";

    fn abstract_addr(name: &str) -> UnixAddr {
        UnixAddr::from_abstract_name(name.as_bytes()).expect("abstract address")
    }

    /// The other half of [`run_probe`]: does nothing unless that ran it,
    /// in which case it installs the named profile's Landlock rules on
    /// itself — the rules alone, since seccomp would refuse some of the
    /// probe's sockets before Landlock had its say — and checks what they
    /// let through.
    #[test]
    fn scope_probe() {
        let Some(profile) = std::env::var(PROBE_ENV).ok() else {
            return;
        };
        let outside: libc::pid_t = std::env::var(PROBE_PID_ENV)
            .expect("probe PID")
            .parse()
            .expect("valid probe PID");
        let name = std::env::var(PROBE_SOCKET_ENV).expect("probe socket name");

        let notify = UnixDatagram::unbound().expect("datagram socket");
        notify
            .connect_addr(&abstract_addr(&format!("{name}{DGRAM_SUFFIX}")))
            .expect("connect the datagram socket");

        let dir = TempDir::with_prefix("kiki-scope-probe").expect("create tempdir");
        let config = match profile.as_str() {
            "server" => SandboxConfig::server(
                dir.path().to_path_buf(),
                dir.path().to_path_buf(),
                dir.path().to_path_buf(),
                false,
            ),
            "script-host" => SandboxConfig::script_host(false),
            "feed-fetcher" => SandboxConfig::feed_fetcher(false),
            "web-ui" => SandboxConfig::web_ui(false),
            other => panic!("unknown profile {other}"),
        };
        sandbox::restrict_filesystem(&config).expect("install the Landlock rules");

        if landlock_abi() < 6 {
            println!("{PROBE_SKIPPED}");
            return;
        }

        // Abstract sockets outside the sandbox are out of reach...
        let err = UnixStream::connect_addr(&abstract_addr(&name))
            .expect_err("connecting to an outside abstract socket was allowed");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM), "{err}");
        // ...except for a datagram peer connected beforehand.
        notify
            .send(NOTIFY_MESSAGE)
            .expect("sending on the pre-connected datagram socket");

        // SAFETY: signal 0 only checks that the signal could be sent.
        let outside_result = unsafe { libc::kill(outside, 0) };
        if profile == "web-ui" {
            assert_eq!(outside_result, 0, "the web UI could not signal outside");
        } else {
            assert_eq!(outside_result, -1, "signalling outside was allowed");
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EPERM)
            );
        }

        // SAFETY: as above, to the probe itself.
        assert_eq!(
            unsafe { libc::kill(libc::getpid(), 0) },
            0,
            "could not signal itself"
        );

        // A child started under the sandbox nests in it, so may still be
        // stopped: what the server does to its children.
        //
        // SAFETY: the child calls only `pause` and `_exit`, both
        // async-signal-safe, so forking a multithreaded process is sound.
        let child = unsafe { libc::fork() };
        assert!(child >= 0, "fork failed");
        if child == 0 {
            unsafe {
                libc::pause();
                libc::_exit(0);
            }
        }
        // SAFETY: our own child, which we then reap.
        unsafe {
            assert_eq!(
                libc::kill(child, libc::SIGKILL),
                0,
                "could not stop a child"
            );
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFSIGNALED(status));
        }

        println!("{PROBE_PASSED}");
    }

    /// Run [`scope_probe`] for `profile` in a fresh process, and check it
    /// passed — and delivered its datagram — or skipped on an old kernel.
    fn run_probe(profile: &str) {
        let name = format!("kiki-scope-probe-{}-{profile}", std::process::id());
        let _listener =
            UnixListener::bind_addr(&abstract_addr(&name)).expect("bind the stream listener");
        let dgram = UnixDatagram::bind_addr(&abstract_addr(&format!("{name}{DGRAM_SUFFIX}")))
            .expect("bind the datagram socket");

        let output = Command::new(std::env::current_exe().expect("test binary"))
            .args(["--exact", "landlock_scoping::scope_probe", "--nocapture"])
            .env(PROBE_ENV, profile)
            .env(PROBE_PID_ENV, std::process::id().to_string())
            .env(PROBE_SOCKET_ENV, &name)
            .output()
            .expect("run the probe");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success()
                && (stdout.contains(PROBE_PASSED) || stdout.contains(PROBE_SKIPPED)),
            "{profile} probe failed ({}):\n{stdout}\n{stderr}",
            describe_exit(output.status)
        );
        if stdout.contains(PROBE_PASSED) {
            dgram
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set a read timeout");
            let mut buf = [0u8; 64];
            let n = dgram.recv(&mut buf).expect("receive the probe's datagram");
            assert_eq!(&buf[..n], NOTIFY_MESSAGE);
        }
    }

    #[test]
    fn the_server_is_scoped() {
        run_probe("server");
    }

    #[test]
    fn the_script_host_is_scoped() {
        run_probe("script-host");
    }

    #[test]
    fn the_feed_fetcher_is_scoped() {
        run_probe("feed-fetcher");
    }

    #[test]
    fn the_web_ui_is_scoped_for_abstract_sockets_only() {
        run_probe("web-ui");
    }

    /// The real server still tells the service manager it is ready over
    /// an abstract `$NOTIFY_SOCKET`, which it connects before its sandbox
    /// scopes abstract sockets away.
    #[test]
    fn the_sandboxed_server_notifies_over_an_abstract_socket() {
        let name = format!("kiki-notify-test-{}", std::process::id());
        let dgram = UnixDatagram::bind_addr(&abstract_addr(&name)).expect("bind the notify socket");
        dgram
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set a read timeout");

        let mut kiki = Kiki::spawn_with_env(&[], &[("NOTIFY_SOCKET", &format!("@{name}"))]);
        let mut buf = [0u8; 256];
        let n = dgram.recv(&mut buf).expect("receive READY=1");
        let msg = String::from_utf8_lossy(&buf[..n]);
        assert!(msg.contains("READY=1"), "unexpected notification: {msg:?}");
        kiki.assert_still_running();
        kiki.shutdown();
    }
}
