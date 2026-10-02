//! Telling a service manager such as systemd how the server is doing, over
//! the `sd_notify` protocol: that it is ready, that it is still alive, and
//! that it is stopping.
//!
//! The protocol is one datagram per message, sent to the Unix socket named
//! by `$NOTIFY_SOCKET`. The socket is connected in [`Notifier::from_env`],
//! which must run before the sandbox is installed: afterwards the server
//! could no longer reach it. Under a unit with `WatchdogSec=`, systemd also
//! sets `$WATCHDOG_USEC`, and restarts the service if it goes that long
//! without a `WATCHDOG=1`.
//!
//! There is no `sd_notify` outside Unix: there a `$NOTIFY_SOCKET` cannot
//! be connected to, and the server carries on without a notifier.

use std::io;
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::time::Duration;

/// The socket notifications are sent over.
#[cfg(unix)]
type Socket = UnixDatagram;

/// See the `unix` variant. Uninhabited, since no [`Notifier`] can be made
/// here.
#[cfg(not(unix))]
#[derive(Debug)]
enum Socket {}

/// Names the socket to notify.
const NOTIFY_SOCKET: &str = "NOTIFY_SOCKET";

/// How long the service manager waits between watchdog pings before it
/// declares the service hung, in microseconds.
const WATCHDOG_USEC: &str = "WATCHDOG_USEC";

/// The process the watchdog is meant for, when set.
const WATCHDOG_PID: &str = "WATCHDOG_PID";

/// A connection to the service manager.
#[derive(Debug)]
pub struct Notifier {
    socket: Socket,
    /// How often to ping the watchdog, if the service manager wants it.
    watchdog: Option<Duration>,
}

impl Notifier {
    /// Connect to the socket named by `$NOTIFY_SOCKET`, if it is set, and
    /// clear the variables, so the child processes the server starts do
    /// not try to notify on its behalf.
    ///
    /// Returns `Ok(None)` when not run under a service manager that wants
    /// notifications.
    ///
    /// **Must be called while the process is still single-threaded**, since
    /// it changes the environment, and before the sandbox is installed.
    ///
    /// # Errors
    ///
    /// Fails if `$NOTIFY_SOCKET` names a socket that cannot be connected
    /// to.
    pub fn from_env() -> io::Result<Option<Notifier>> {
        let Some(path) = std::env::var_os(NOTIFY_SOCKET).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let watchdog = watchdog_interval(
            std::env::var(WATCHDOG_USEC).ok().as_deref(),
            std::env::var(WATCHDOG_PID).ok().as_deref(),
            std::process::id(),
        );
        for var in [NOTIFY_SOCKET, WATCHDOG_USEC, WATCHDOG_PID] {
            std::env::remove_var(var);
        }

        let socket = connect(std::path::Path::new(&path))?;
        Ok(Some(Notifier { socket, watchdog }))
    }

    /// A notifier sending to the socket at `path`, with the watchdog
    /// pinged every `watchdog`, for tests that play the service manager.
    #[cfg(test)]
    pub(crate) fn for_socket(
        path: &std::path::Path,
        watchdog: Option<Duration>,
    ) -> io::Result<Notifier> {
        let socket = connect(path)?;
        Ok(Notifier { socket, watchdog })
    }

    /// How often to call [`Self::watchdog`], or `None` if the service
    /// manager has no watchdog for this process.
    pub fn watchdog_interval(&self) -> Option<Duration> {
        self.watchdog
    }

    /// Say that the server is up and accepting connections.
    pub fn ready(&self) {
        self.send("READY=1\nSTATUS=Serving");
    }

    /// Say that the server is still alive and well.
    pub fn watchdog(&self) {
        self.send("WATCHDOG=1");
    }

    /// Say that the server is shutting down.
    pub fn stopping(&self) {
        self.send("STOPPING=1");
    }

    #[cfg(unix)]
    fn send(&self, msg: &str) {
        if let Err(e) = self.socket.send(msg.as_bytes()) {
            tracing::warn!("could not notify the service manager ({msg:?}): {e}");
        }
    }

    #[cfg(not(unix))]
    fn send(&self, _msg: &str) {
        match self.socket {}
    }
}

/// A socket connected to `path`, which names an abstract socket when it
/// starts with `@`.
#[cfg(unix)]
fn connect(path: &std::path::Path) -> io::Result<Socket> {
    use std::os::unix::ffi::OsStrExt;
    let socket = UnixDatagram::unbound()?;
    match path.as_os_str().as_bytes().strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        Some(name) => {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            socket.connect_addr(&addr)?;
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "abstract sockets are only supported on Linux",
            ))
        }
        None => socket.connect(path)?,
    }
    Ok(socket)
}

/// See the `unix` variant.
#[cfg(not(unix))]
fn connect(path: &std::path::Path) -> io::Result<Socket> {
    let _ = path;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "sd_notify is only supported on Unix",
    ))
}

/// How often to ping a watchdog that times out after `usec` microseconds:
/// twice per timeout, as `sd_watchdog_enabled(3)` recommends. `None` when
/// there is no watchdog, or it is meant for another process than `pid`.
fn watchdog_interval(usec: Option<&str>, watchdog_pid: Option<&str>, pid: u32) -> Option<Duration> {
    if let Some(for_pid) = watchdog_pid {
        if for_pid.trim().parse::<u32>().ok() != Some(pid) {
            return None;
        }
    }
    let usec: u64 = usec?.trim().parse().ok().filter(|&u| u > 0)?;
    Some(Duration::from_micros(usec) / 2)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_watchdog_is_pinged_twice_per_timeout() {
        assert_eq!(
            watchdog_interval(Some("60000000"), None, 7),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            watchdog_interval(Some("60000000"), Some("7"), 7),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn no_watchdog_unless_asked_for_this_process() {
        assert_eq!(watchdog_interval(None, None, 7), None);
        assert_eq!(watchdog_interval(Some("0"), None, 7), None);
        assert_eq!(watchdog_interval(Some("junk"), None, 7), None);
        assert_eq!(watchdog_interval(Some("60000000"), Some("8"), 7), None);
    }

    #[cfg(unix)]
    #[test]
    fn messages_reach_the_socket() {
        let dir = tempfile::TempDir::with_prefix("kiki_").unwrap();
        let path = dir.path().join("notify");
        let listener = UnixDatagram::bind(&path).unwrap();
        let notifier = Notifier::for_socket(&path, None).unwrap();

        let mut buf = [0u8; 64];
        for (send, expected) in [
            (Notifier::ready as fn(&Notifier), "READY=1\nSTATUS=Serving"),
            (Notifier::watchdog, "WATCHDOG=1"),
            (Notifier::stopping, "STOPPING=1"),
        ] {
            send(&notifier);
            let n = listener.recv(&mut buf).unwrap();
            assert_eq!(
                std::str::from_utf8(buf.get(..n).unwrap()).unwrap(),
                expected
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abstract_sockets_are_supported() {
        use std::os::linux::net::SocketAddrExt;
        let name = format!("kiki-notify-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixDatagram::bind_addr(&addr).unwrap();
        let socket = connect(std::path::Path::new(&format!("@{name}"))).unwrap();
        socket.send(b"READY=1").unwrap();
        let mut buf = [0u8; 16];
        let n = listener.recv(&mut buf).unwrap();
        assert_eq!(buf.get(..n).unwrap(), b"READY=1");
    }
}
