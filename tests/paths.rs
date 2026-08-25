//! Integration tests for where `kiki serve` puts its Unix socket, and for
//! how it treats a socket path that is already occupied.
//!
//! These spawn the real binary, because the behaviour under test is the
//! interaction between path resolution, the sandbox, and `bind(2)` — none of
//! which the in-process library tests exercise.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tempdir::TempDir;

const KIKI_BIN: &str = env!("CARGO_BIN_EXE_kiki");

/// A `kiki serve` subprocess, killed and reaped on drop.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Initialize a data directory and return it along with its temp dir guard.
fn init_home() -> (TempDir, PathBuf) {
    let td = TempDir::new("kiki-paths-test").expect("create tempdir");
    let home = td.path().to_path_buf();

    let status = Command::new(KIKI_BIN)
        .arg("init")
        .arg(&home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn kiki init");
    assert!(status.success(), "kiki init failed: {status:?}");

    (td, home)
}

/// Spawn `kiki serve` with `$KIKI_HOME` set to `home` and nothing else
/// pointing it anywhere in particular.
fn spawn_serve(home: &Path) -> Child {
    Command::new(KIKI_BIN)
        .arg("serve")
        .env("KIKI_HOME", home)
        .env_remove("KIKI_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kiki serve")
}

/// Poll until `socket` accepts a connection, or panic.
fn wait_until_listening(socket: &Path, child: &mut Child) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!("kiki exited before listening ({status:?}): {stderr}");
        }
        if socket.exists() && UnixStream::connect(socket).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("kiki never started listening on {}", socket.display());
}

/// Wait for a process to exit and return its stderr.
fn wait_for_failure(mut child: Child) -> String {
    let status = child.wait().expect("wait for child");
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    assert!(
        !status.success(),
        "expected kiki to exit with a failure, got {status:?}"
    );
    stderr
}

/// `$KIKI_HOME` alone is enough for `kiki init` — no `--auto`, no directory
/// argument — and `kiki migrate` then finds the same database from an
/// unrelated working directory.
#[test]
fn kiki_home_governs_init_and_migrate() {
    let td = TempDir::new("kiki-paths-test").expect("create tempdir");
    let home = td.path().join("home");
    let elsewhere = td.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).expect("create dir");

    let init = Command::new(KIKI_BIN)
        .arg("init")
        .env("KIKI_HOME", &home)
        .current_dir(&elsewhere)
        .output()
        .expect("spawn kiki init");
    assert!(
        init.status.success(),
        "kiki init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(home.join("kiki.db").exists());

    // Run from a directory with no kiki.db in it, so only $KIKI_HOME can
    // point migrate at the right database.
    let migrate = Command::new(KIKI_BIN)
        .arg("migrate")
        .arg("--dry-run")
        .env("KIKI_HOME", &home)
        .current_dir(&elsewhere)
        .output()
        .expect("spawn kiki migrate");
    assert!(
        migrate.status.success(),
        "kiki migrate failed: {}",
        String::from_utf8_lossy(&migrate.stderr)
    );
    assert!(
        String::from_utf8_lossy(&migrate.stdout).contains("up to date"),
        "unexpected migrate output: {}",
        String::from_utf8_lossy(&migrate.stdout)
    );
}

/// `$KIKI_HOME` displaces both the platform data directory and the runtime
/// directory: the database and the socket both land inside it.
#[test]
fn kiki_home_holds_both_the_database_and_the_socket() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");

    let mut child = spawn_serve(&home);
    wait_until_listening(&socket, &mut child);
    let _guard = Server(child);

    assert!(home.join("kiki.db").exists());
    assert!(socket.exists());
}

/// The socket path doubles as a single-instance guard: a second server must
/// refuse to start rather than unlink the first one's socket out from under
/// it.
#[test]
fn a_second_server_refuses_to_steal_the_socket() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");

    let mut first = spawn_serve(&home);
    wait_until_listening(&socket, &mut first);
    let _guard = Server(first);

    let stderr = wait_for_failure(spawn_serve(&home));
    assert!(
        stderr.contains("already listening"),
        "second server should have reported the first one; got: {stderr}"
    );

    // The original server is untouched and still serving.
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "the first server's socket should still be live"
    );
}

/// The socket goes wherever the data directory was named, so the original
/// `kiki init . && kiki serve` workflow still puts it at `./kiki.sock` — even
/// with a perfectly good runtime directory available.
#[test]
fn a_data_dir_named_by_the_current_directory_holds_the_socket() {
    let (_td, home) = init_home();

    // A valid runtime directory: owned by us, mode 0700. The cwd rule must
    // still win, or the socket would land here instead.
    let runtime = home.join("run");
    std::fs::create_dir(&runtime).expect("create runtime dir");
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700))
        .expect("chmod runtime dir");

    let socket = home.join("kiki.sock");
    let mut child = Command::new(KIKI_BIN)
        .arg("serve")
        .current_dir(&home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env_remove("KIKI_HOME")
        .env_remove("KIKI_SOCKET")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn kiki serve");

    wait_until_listening(&socket, &mut child);
    let _guard = Server(child);

    assert!(
        !runtime.join("kiki").exists(),
        "the socket should not have gone to the runtime directory"
    );
}

/// A socket file left behind by a killed server is stale, not occupied, and
/// gets cleared on the next start.
#[test]
fn a_stale_socket_file_does_not_block_startup() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");

    // Binding and dropping leaves the file in place with nothing listening,
    // which is the state an unclean shutdown leaves behind.
    let listener = UnixListener::bind(&socket).expect("bind stale socket");
    drop(listener);
    assert!(socket.exists());

    let mut child = spawn_serve(&home);
    wait_until_listening(&socket, &mut child);
    let _guard = Server(child);
}

/// A non-socket file at the socket path is somebody else's, and Kiki must
/// not delete it.
#[test]
fn a_regular_file_at_the_socket_path_is_not_removed() {
    let (_td, home) = init_home();
    let socket = home.join("kiki.sock");
    std::fs::write(&socket, b"important").expect("write file");

    let stderr = wait_for_failure(spawn_serve(&home));
    assert!(
        stderr.contains("not a socket"),
        "expected a refusal to remove a non-socket; got: {stderr}"
    );
    assert_eq!(
        std::fs::read(&socket).expect("read file"),
        b"important",
        "the file should have been left alone"
    );
}
