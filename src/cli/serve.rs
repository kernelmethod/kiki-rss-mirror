use crate::cli::paths::{self, Env};
use crate::sandbox::{self, SandboxConfig};
use crate::server;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Where the server will listen, resolved from the CLI arguments and the
/// process environment.
#[derive(Debug, Clone)]
enum Listener {
    /// A Unix domain socket at the given path.
    Uds(PathBuf),

    /// A TCP socket address.
    Tcp(SocketAddr),
}

#[derive(Args)]
pub struct ServeArgs {
    /// Listen on a TCP port
    #[arg(short, long, conflicts_with = "socket_path")]
    port: Option<u16>,

    /// IP address to bind the TCP listener to. Only meaningful with --port.
    #[arg(
        short = 'b',
        long,
        default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST),
        requires = "port",
        conflicts_with = "socket_path",
    )]
    bind: IpAddr,

    /// Path to the Unix domain socket
    /// [default: $KIKI_SOCKET, $KIKI_HOME/kiki.sock, or
    /// $XDG_RUNTIME_DIR/kiki/kiki.sock]
    #[arg(
        short = 'u',
        long = "uds",
        value_name = "PATH",
        conflicts_with = "port"
    )]
    socket_path: Option<PathBuf>,

    /// Disable the OS-level sandbox (Landlock + seccomp-bpf on Linux).
    ///
    /// Only use this if the sandbox is demonstrably causing a failure —
    /// running without it exposes the full filesystem and syscall surface
    /// to any post-exploitation code path.
    #[arg(long)]
    no_sandbox: bool,

    /// Run the seccomp filter in log-only mode instead of killing on
    /// violation. Useful when tightening the denylist or diagnosing an
    /// unexpected SIGSYS in production. Landlock is unaffected.
    #[arg(long)]
    seccomp_log_only: bool,
}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        let env = Env::from_process();

        let data_dir = paths::resolve_data_dir(&env)?;
        if !data_dir.path.is_dir() {
            bail!(
                "no Kiki data directory at {}; run `kiki init {}` to create one, or set \
                 $KIKI_HOME",
                data_dir.path.display(),
                data_dir.path.display(),
            );
        }
        let db_path = data_dir.path.join(paths::DB_FILE_NAME);
        tracing::info!(
            path = %data_dir.path.display(),
            source = ?data_dir.source,
            "using data directory"
        );

        let listener = self.resolve_listener(&data_dir, &env)?;

        // The socket's parent directory has to exist before the sandbox is
        // installed: Landlock rules can only be attached to paths that
        // already exist, and once the ruleset is in force the process can no
        // longer create the directory itself.
        let socket_dir = match &listener {
            Listener::Uds(path) => Some(ensure_socket_dir(path)?),
            Listener::Tcp(_) => None,
        };

        if !self.no_sandbox {
            let config = build_sandbox_config(&db_path, socket_dir, self);
            sandbox::apply(&config).context("failed to install sandbox")?;
        } else {
            tracing::warn!(
                "sandbox disabled via --no-sandbox; process runs with full filesystem \
                 and syscall access"
            );
        }

        let mut builder = server::ServerBuilder::new(&db_path).autofetch();
        builder = match &listener {
            Listener::Uds(path) => builder.socket_path(path),
            Listener::Tcp(addr) => builder.bind_addr(*addr),
        };
        let server = builder.build();

        std::thread::spawn(|| server.run())
            .join()
            .map_err(|_| anyhow::anyhow!("panic in server thread"))?
    }

    /// Resolve where the server should listen.
    ///
    /// `--port` selects TCP; otherwise the server listens on a Unix domain
    /// socket whose path is resolved by [`paths::resolve_socket_path`].
    ///
    /// # Errors
    ///
    /// Returns an error if the resolved socket path is too long to fit in a
    /// Unix socket address.
    fn resolve_listener(&self, data_dir: &paths::DataDir, env: &Env) -> Result<Listener> {
        match self.port {
            Some(port) => Ok(Listener::Tcp(SocketAddr::new(self.bind, port))),
            None => {
                let path = paths::resolve_socket_path(self.socket_path.as_deref(), data_dir, env);
                paths::validate_socket_path(&path)?;
                Ok(Listener::Uds(path))
            }
        }
    }
}

/// Create the directory that the Unix socket will be placed in, and return
/// it.
///
/// A directory Kiki creates itself is made owner-only: the socket carries no
/// authentication of its own, so reachability is the access control. A
/// directory that already exists is left alone — under systemd, for
/// instance, `RuntimeDirectory=` has already created it with the ownership
/// and mode the unit asked for.
///
/// # Errors
///
/// Returns an error if the directory cannot be created or its permissions
/// cannot be set.
fn ensure_socket_dir(socket_path: &Path) -> Result<PathBuf> {
    let dir = parent_or_cwd(socket_path);

    if !dir.exists() {
        fs::create_dir_all(&dir)
            .with_context(|| format!("unable to create socket directory {dir:?}"))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("unable to set permissions on socket directory {dir:?}"))?;
    }

    Ok(dir)
}

/// Build a [`SandboxConfig`] from the CLI arguments and the paths the
/// server will use.
///
/// The sandbox needs read-write access to:
///   * the directory containing the SQLite database (which also contains
///     the `assets/` cache tree), and
///   * the parent directory of the Unix socket, if the server is
///     listening on a UDS (so the socket file can be created/unlinked).
fn build_sandbox_config(
    db_path: &Path,
    socket_dir: Option<PathBuf>,
    args: &ServeArgs,
) -> SandboxConfig {
    SandboxConfig {
        data_dir: parent_or_cwd(db_path),
        socket_dir,
        log_only: args.seccomp_log_only,
    }
}

/// Return the parent directory of `p`, treating a relative path with no
/// parent component (e.g. `kiki.db`) as referring to the current
/// directory.
fn parent_or_cwd(p: &Path) -> PathBuf {
    p.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::Parser;
    use tempdir::TempDir;

    /// Wrapper so the `Args`-derived [`ServeArgs`] can be exercised
    /// through real argv parsing, covering the flag names too.
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn parse(argv: &[&str]) -> ServeArgs {
        TestCli::parse_from(std::iter::once("kiki").chain(argv.iter().copied())).serve
    }

    /// A data directory nobody named — the platform default, which is what
    /// leaves the socket to the runtime directory.
    fn platform_dir() -> paths::DataDir {
        paths::DataDir {
            path: PathBuf::from("/data"),
            source: paths::DataDirSource::Platform,
        }
    }

    /// The sandbox config the given argv produces, with the socket path
    /// resolved against `env` and the database in `/data`.
    fn config_from_env(argv: &[&str], env: &Env) -> Result<SandboxConfig> {
        let args = parse(argv);
        let data_dir = platform_dir();
        let socket_dir = match args.resolve_listener(&data_dir, env)? {
            Listener::Uds(path) => Some(parent_or_cwd(&path)),
            Listener::Tcp(_) => None,
        };
        Ok(build_sandbox_config(
            &data_dir.path.join(paths::DB_FILE_NAME),
            socket_dir,
            &args,
        ))
    }

    fn config_from(argv: &[&str]) -> SandboxConfig {
        config_from_env(argv, &Env::default()).unwrap()
    }

    #[test]
    fn uds_mode_grants_the_socket_directory() {
        let config = config_from(&[]);
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.socket_dir, Some(PathBuf::from("/data")));
    }

    #[test]
    fn tcp_mode_grants_no_socket_directory() {
        let config = config_from(&["--port", "8000"]);
        assert_eq!(config.data_dir, PathBuf::from("/data"));
        assert_eq!(config.socket_dir, None);
    }

    #[test]
    fn seccomp_log_only_flag_reaches_the_config() {
        assert!(!config_from(&[]).log_only);
        assert!(config_from(&["--seccomp-log-only"]).log_only);
    }

    /// When the socket defaults into the runtime directory, that is the
    /// directory the sandbox has to grant — the data directory alone would
    /// leave the server unable to create its socket.
    #[test]
    fn uds_mode_grants_the_runtime_dir_when_the_socket_lives_there() -> Result<()> {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let config = config_from_env(&[], &env)?;

        assert_eq!(
            config.socket_dir,
            Some(PathBuf::from("/run/user/1000/kiki"))
        );
        Ok(())
    }

    #[test]
    fn port_selects_a_tcp_listener() -> Result<()> {
        let listener =
            parse(&["--port", "8000"]).resolve_listener(&platform_dir(), &Env::default())?;
        assert!(matches!(listener, Listener::Tcp(addr) if addr.port() == 8000));
        Ok(())
    }

    #[test]
    fn default_listener_is_a_socket_in_the_runtime_dir() -> Result<()> {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let listener = parse(&[]).resolve_listener(&platform_dir(), &env)?;
        assert!(
            matches!(&listener, Listener::Uds(p) if p == Path::new("/run/user/1000/kiki/kiki.sock")),
            "unexpected listener: {listener:?}"
        );
        Ok(())
    }

    #[test]
    fn uds_flag_overrides_the_default() -> Result<()> {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let listener =
            parse(&["--uds", "/tmp/elsewhere.sock"]).resolve_listener(&platform_dir(), &env)?;
        assert!(
            matches!(&listener, Listener::Uds(p) if p == Path::new("/tmp/elsewhere.sock")),
            "unexpected listener: {listener:?}"
        );
        Ok(())
    }

    /// A data directory named by `$KIKI_HOME` takes the socket with it, so
    /// the sandbox must grant that directory rather than the runtime one.
    #[test]
    fn uds_mode_grants_a_named_data_dir() -> Result<()> {
        let env = Env {
            kiki_home: Some(PathBuf::from("/srv/kiki")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let args = parse(&[]);
        let data_dir = paths::DataDir {
            path: PathBuf::from("/srv/kiki"),
            source: paths::DataDirSource::KikiHome,
        };
        let listener = args.resolve_listener(&data_dir, &env)?;

        assert!(
            matches!(&listener, Listener::Uds(p) if p == Path::new("/srv/kiki/kiki.sock")),
            "unexpected listener: {listener:?}"
        );
        Ok(())
    }

    #[test]
    fn an_over_long_socket_path_is_rejected() {
        let long = format!("/{}/kiki.sock", "a".repeat(paths::MAX_SOCKET_PATH_LEN));
        assert!(parse(&["--uds", &long])
            .resolve_listener(&platform_dir(), &Env::default())
            .is_err());
    }

    #[test]
    fn socket_dir_is_created_owner_only() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let dir = td.path().join("runtime").join("kiki");
        let created = ensure_socket_dir(&dir.join("kiki.sock"))?;

        assert_eq!(created, dir);
        assert_eq!(
            fs::metadata(&dir)?.permissions().mode() & 0o777,
            0o700,
            "a socket directory Kiki creates should not be reachable by other users"
        );
        Ok(())
    }

    /// An existing directory keeps whatever mode its owner gave it — under
    /// systemd that is `RuntimeDirectory=`/`RuntimeDirectoryMode=`.
    #[test]
    fn socket_dir_that_already_exists_is_left_alone() -> Result<()> {
        let td = TempDir::new("kiki_")?;
        let dir = td.path().join("runtime");
        fs::create_dir(&dir)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750))?;

        ensure_socket_dir(&dir.join("kiki.sock"))?;

        assert_eq!(fs::metadata(&dir)?.permissions().mode() & 0o777, 0o750);
        Ok(())
    }

    #[test]
    fn relative_socket_path_resolves_against_the_current_directory() {
        assert_eq!(parent_or_cwd(Path::new("kiki.sock")), Path::new("."));
    }
}
