//! Integration tests for API tokens: `kiki token`, and the server checking
//! them from inside its sandbox.
//!
//! These spawn the real binary, so that tokens are created and checked
//! under the same Landlock and seccomp filters the server runs under in
//! production.

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
    let td = TempDir::with_prefix("kiki-tokens-test").expect("create tempdir");
    let home = td.path().to_path_buf();
    let status = Command::new(KIKI_BIN)
        .arg("init")
        .env("KIKI_HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn kiki init");
    assert!(status.success(), "kiki init failed: {status:?}");
    (td, home)
}

fn kiki_token(home: &Path, args: &[&str]) -> Output {
    Command::new(KIKI_BIN)
        .arg("token")
        .args(args)
        .env("KIKI_HOME", home)
        .output()
        .expect("spawn kiki token")
}

fn create_token(home: &Path, name: &str, scopes: &str) -> String {
    let out = kiki_token(home, &["create", name, "--scopes", scopes]);
    assert!(
        out.status.success(),
        "kiki token create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf-8 token")
        .trim()
        .to_owned()
}

/// Send a bodiless request and return its status code.
fn status(socket: &Path, method: &str, path: &str, token: Option<&str>) -> u16 {
    request(socket, method, path, token, "").0
}

/// Send a request with the JSON `body`, and return its status code and
/// body.
fn request(
    socket: &Path,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, String) {
    let mut stream = UnixStream::connect(socket).expect("connect to the API");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set timeout");
    let auth = token
        .map(|t| format!("Authorization: Bearer {t}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Content-Type: application/json\r\n\
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

/// The sandboxed server holds every request that carries a token to it.
#[test]
fn server_checks_tokens() {
    let (_td, home) = init_home();
    let reader = create_token(&home, "reader", "reader");
    let admin = create_token(&home, "admin", "admin");
    let socket = home.join("kiki.sock");
    let _server = spawn_serve(&home, &socket);
    let socket = socket.as_path();

    assert_eq!(status(socket, "GET", "/v1/tokens", None), 200);
    assert_eq!(
        status(socket, "GET", "/v1/feeds", Some("kiki_1_wrong")),
        401
    );
    assert_eq!(status(socket, "GET", "/v1/feeds", Some(&reader)), 200);
    assert_eq!(status(socket, "GET", "/v1/tokens", Some(&reader)), 403);
    assert_eq!(status(socket, "GET", "/v1/tokens", Some(&admin)), 200);

    // A token revoked while the server runs is refused at once.
    let out = kiki_token(&home, &["revoke", "reader"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(status(socket, "GET", "/v1/feeds", Some(&reader)), 401);

    // Tokens can be created through the API too, from inside the sandbox.
    let (code, body) = request(
        socket,
        "POST",
        "/v1/tokens",
        Some(&admin),
        r#"{"name": "made-over-the-api", "scopes": ["read"]}"#,
    );
    assert_eq!(code, 201, "{body}");
    assert!(body.contains("\"token\":\"kiki_"), "{body}");

    assert_eq!(status(socket, "POST", "/v1/shutdown", Some(&admin)), 200);
}

/// `kiki token ls` lists what `create` made, and never the tokens
/// themselves.
#[test]
fn token_ls_lists_tokens() {
    let (_td, home) = init_home();
    let token = create_token(&home, "phone", "curator");
    assert!(token.starts_with("kiki_"));

    let out = kiki_token(&home, &["ls"]);
    assert!(out.status.success());
    let listing = String::from_utf8_lossy(&out.stdout);
    assert!(listing.contains("phone"), "{listing}");
    assert!(listing.contains("read,state,tags"), "{listing}");
    assert!(!listing.contains(&token), "{listing}");

    let out = kiki_token(&home, &["create", "phone", "--scopes", "read"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("already exists"));

    let out = kiki_token(&home, &["create", "x", "--scopes", "root"]);
    assert!(!out.status.success());
}
