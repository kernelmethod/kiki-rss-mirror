//! Integration tests for how `kiki web` manages the `kiki serve` child it
//! starts.

#![cfg(all(target_os = "linux", feature = "web-ui"))]

use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

const KIKI_BIN: &str = env!("CARGO_BIN_EXE_kiki");

/// A subprocess, killed and reaped on drop.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The state letter in `/proc/<pid>/stat` (`R`, `S`, `Z`, ...) and the
/// parent's pid, or `None` once the process is gone.
fn proc_state(pid: u32) -> Option<(char, u32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may contain spaces, so the
    // fields are counted from the last closing parenthesis.
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

/// The pids of the live processes whose parent is `parent`.
fn children_of(parent: u32) -> Vec<u32> {
    std::fs::read_dir("/proc")
        .expect("read /proc")
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| matches!(proc_state(pid), Some((s, ppid)) if ppid == parent && s != 'Z'))
        .collect()
}

/// Whether `pid` has exited: it is gone, or a zombie waiting to be reaped
/// by whichever process adopted it.
fn has_exited(pid: u32) -> bool {
    !matches!(proc_state(pid), Some((s, _)) if s != 'Z')
}

/// Poll until `socket` accepts a connection, or panic.
fn wait_until_listening(socket: &Path, child: &mut Child) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!("kiki web exited before the server listened: {status:?}");
        }
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("the server never started listening on {}", socket.display());
}

/// Killing `kiki web` outright, so that it cannot stop its server, still
/// stops the server, rather than leaving it running with the database and
/// the socket.
#[test]
fn the_server_stops_when_the_web_ui_is_killed() {
    let td = TempDir::with_prefix("kiki-web-test").expect("create tempdir");
    let home = td.path();
    let status = Command::new(KIKI_BIN)
        .arg("init")
        .env("KIKI_HOME", home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn kiki init");
    assert!(status.success(), "kiki init failed: {status:?}");

    // A free port for the web UI to listen on.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("find a free port")
        .port();
    let mut web = Reaped(
        Command::new(KIKI_BIN)
            .args(["web", "--listen", &format!("127.0.0.1:{port}")])
            .env("KIKI_HOME", home)
            .env_remove("KIKI_SOCKET")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn kiki web"),
    );
    wait_until_listening(&home.join("kiki.sock"), &mut web.0);

    let servers = children_of(web.0.id());
    assert_eq!(
        servers.len(),
        1,
        "expected one server child, got {servers:?}"
    );
    let server = servers[0];
    // Killed by the test if it outlives the check below.
    struct KillOnDrop(u32);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            // SAFETY: `kill` is a plain syscall; at worst the pid is gone
            // and it fails with ESRCH.
            unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGKILL) };
        }
    }
    let _server_guard = KillOnDrop(server);

    web.0.kill().expect("SIGKILL kiki web");
    web.0.wait().expect("reap kiki web");

    let start = Instant::now();
    while !has_exited(server) {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "the server (pid {server}) kept running after the web UI was killed"
        );
        thread::sleep(Duration::from_millis(50));
    }
}
