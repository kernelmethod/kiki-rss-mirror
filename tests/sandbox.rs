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

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tempdir::TempDir;

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
        let dir = TempDir::new("kiki-sandbox-test").expect("create tempdir");

        let init_status = Command::new(KIKI_BIN)
            .arg("init")
            .arg(dir.path())
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
