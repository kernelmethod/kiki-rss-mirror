//! Integration tests for `kiki config`, run against a sandboxed `kiki
//! serve` changing the same config file through the settings API.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const KIKI_BIN: &str = env!("CARGO_BIN_EXE_kiki");

/// A `kiki serve` subprocess, killed and reaped on drop.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn init_home() -> (TempDir, PathBuf) {
    let td = TempDir::with_prefix("kiki-config-test").expect("create tempdir");
    let home = td.path().to_path_buf();
    let status = Command::new(KIKI_BIN)
        .args(["init", "--no-default-plugins"])
        .env("KIKI_HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn kiki init");
    assert!(status.success(), "kiki init failed: {status:?}");
    (td, home)
}

fn kiki_config(home: &Path, args: &[&str]) -> Output {
    let out = Command::new(KIKI_BIN)
        .arg("config")
        .args(args)
        .env("KIKI_HOME", home)
        .output()
        .expect("spawn kiki config");
    assert!(
        out.status.success(),
        "kiki config {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Send a request with the JSON `body`, and return its status code and
/// body.
fn request(socket: &Path, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to the API");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("send request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("read response");
    let code = response
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("malformed response: {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, body)
}

/// Spawn `kiki serve` in `home`, and wait for it to listen on its socket,
/// `home/kiki.sock`.
fn spawn_serve(home: &Path, socket: &Path) -> Server {
    let mut server = Server(
        Command::new(KIKI_BIN)
            .arg("serve")
            .env("KIKI_HOME", home)
            .env_remove("KIKI_SOCKET")
            .env_remove("KIKI_RUNTIME_DIR")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn kiki serve"),
    );

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(status) = server.0.try_wait().expect("try_wait") {
            let mut stderr = String::new();
            if let Some(mut pipe) = server.0.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("kiki exited before listening ({status:?}): {stderr}");
        }
        if UnixStream::connect(socket).is_ok() {
            return server;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("kiki never started listening on {}", socket.display());
}

/// Wait until `GET path` answers with a body containing `needle`.
fn wait_for(socket: &Path, path: &str, needle: &str) {
    let start = Instant::now();
    let mut body = String::new();
    while start.elapsed() < Duration::from_secs(10) {
        let (code, b) = request(socket, "GET", path, "");
        assert_eq!(code, 200, "GET {path}: {b}");
        if b.contains(needle) {
            return;
        }
        body = b;
        thread::sleep(Duration::from_millis(50));
    }
    panic!("GET {path} never showed {needle}; last answer: {body}");
}

/// A running server applies what `kiki config set` saves, and `kiki config
/// get` sees what the API saves.
#[test]
fn the_cli_and_the_api_see_each_others_changes() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");
    let _server = spawn_serve(&home, &socket);

    kiki_config(&home, &["set", "asset_cache.enabled", "false"]);
    wait_for(&socket, "/v1/settings/asset-cache", "\"enabled\":false");

    let (code, body) = request(
        &socket,
        "PUT",
        "/v1/settings/feed-fetch",
        r#"{"timeout_seconds": 42}"#,
    );
    assert_eq!(code, 200, "{body}");
    let out = kiki_config(&home, &["get", "feed_fetch.timeout_seconds"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "42\n");
}

/// The server and `kiki config set`, saving the same file at the same
/// time, never save over each other's changes: each save is built on the
/// one before it.
#[test]
fn concurrent_saves_from_the_cli_and_the_api_are_all_kept() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");
    let _server = spawn_serve(&home, &socket);
    const ROUNDS: u64 = 25;

    thread::scope(|s| {
        s.spawn(|| {
            for i in 1..=ROUNDS {
                let body = format!(r#"{{"timeout_seconds": {i}}}"#);
                let (code, body) = request(&socket, "PUT", "/v1/settings/feed-fetch", &body);
                assert_eq!(code, 200, "{body}");
            }
        });
        s.spawn(|| {
            for i in 1..=ROUNDS {
                let backoff = (1000 + i).to_string();
                kiki_config(&home, &["set", "feed_fetch.max_backoff_seconds", &backoff]);
            }
        });
    });

    let out = kiki_config(&home, &["get", "--overrides"]);
    let overrides = String::from_utf8_lossy(&out.stdout);
    assert!(
        overrides.contains(&format!("timeout_seconds = {ROUNDS}\n")),
        "{overrides}"
    );
    assert!(
        overrides.contains(&format!("max_backoff_seconds = {}\n", 1000 + ROUNDS)),
        "{overrides}"
    );
    wait_for(
        &socket,
        "/v1/settings/feed-fetch",
        &format!("\"max_backoff_seconds\":{}", 1000 + ROUNDS),
    );
}
