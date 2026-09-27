use crate::cli::paths::{self, Env};
use crate::sandbox::{self, SandboxConfig};
use crate::server;
#[cfg(all(unix, feature = "lua"))]
use anyhow::anyhow;
use anyhow::{bail, Context, Result};
use clap::Args;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Arguments for the `kiki serve` subcommand.
///
/// Kiki serves over a Unix domain socket and nothing else. A socket is
/// reachable only by processes that can reach its path, which is access
/// control the server does not have to implement, authenticate, or get
/// right; a TCP listener has none of that. Put a reverse proxy in front to
/// expose Kiki over the network, and let it own the TLS and authentication
/// that job needs.
#[derive(Args)]
pub struct ServeArgs {
    /// Path to the Unix domain socket
    /// [default: $KIKI_SOCKET, $KIKI_RUNTIME_DIR/kiki.sock,
    /// $KIKI_HOME/kiki.sock, or $XDG_RUNTIME_DIR/kiki/kiki.sock]
    #[arg(short = 'u', long = "uds", value_name = "PATH")]
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

    /// Run Lua scripts inside the server process instead of an isolated
    /// child process.
    ///
    /// Scripts are the only code Kiki executes that it did not ship, and
    /// the isolated host holds no database handle, no filesystem access,
    /// and no sockets. Turning this on puts the Lua VM back in the same
    /// address space as the database.
    #[cfg(all(unix, feature = "lua"))]
    #[arg(long)]
    no_script_isolation: bool,
}

impl ServeArgs {
    pub fn run(&self) -> Result<()> {
        tracing_subscriber::fmt::init();

        let env = Env::from_process();

        let data_dir = paths::resolve_data_dir(&env)?;
        if !data_dir.path.is_dir() {
            bail!(
                "no Kiki data directory at {}; run `kiki init` to create one, or set \
                 $KIKI_HOME to point Kiki somewhere else",
                data_dir.path.display(),
            );
        }
        let db_path = data_dir.path.join(paths::DB_FILE_NAME);
        tracing::info!(
            path = %data_dir.path.display(),
            source = ?data_dir.source,
            "using data directory"
        );

        let socket_path = self.resolve_socket_path(&data_dir, &env)?;

        // The socket's parent directory has to exist before the sandbox is
        // installed: Landlock rules can only be attached to paths that
        // already exist, and once the ruleset is in force the process can no
        // longer create the directory itself.
        let socket_dir = ensure_socket_dir(&socket_path)?;

        // Spawn the children *before* the sandbox goes up: every profile
        // denies `execve`, so this is the last moment at which the server
        // can start a child process at all.
        let feed_fetcher = self.spawn_feed_fetcher()?;
        #[cfg(all(unix, feature = "lua"))]
        let script_host = self.spawn_script_host()?;

        if !self.no_sandbox {
            let config = build_sandbox_config(&db_path, socket_dir, self);
            sandbox::apply(&config).context("failed to install sandbox")?;
        } else {
            tracing::warn!(
                "sandbox disabled via --no-sandbox; process runs with full filesystem \
                 and syscall access"
            );
        }

        let builder = server::ServerBuilder::new(&db_path)
            .autofetch()
            .feed_fetcher(feed_fetcher)
            .socket_path(&socket_path);
        #[cfg(all(unix, feature = "lua"))]
        let builder = builder.script_host(script_host);
        let server = builder.build();

        std::thread::spawn(|| server.run())
            .join()
            .map_err(|_| anyhow::anyhow!("panic in server thread"))?
    }

    /// Resolve the socket the server will listen on, from the flags and
    /// the process environment, exactly as `kiki serve` itself would.
    ///
    /// # Errors
    ///
    /// Returns an error if no data directory can be resolved, or if the
    /// resolved socket path is too long to fit in a Unix socket address.
    #[cfg(feature = "web-ui")]
    pub fn socket_path(&self) -> Result<PathBuf> {
        let env = Env::from_process();
        let data_dir = paths::resolve_data_dir(&env)?;
        self.resolve_socket_path(&data_dir, &env)
    }

    /// Render these arguments back into a `kiki serve` command line that
    /// listens on `socket_path`, so another command can start a server
    /// child whose socket it already knows.
    #[cfg(feature = "web-ui")]
    pub fn to_argv(&self, socket_path: &Path) -> Vec<std::ffi::OsString> {
        let mut argv = vec!["--uds".into(), socket_path.as_os_str().to_owned()];
        if self.no_sandbox {
            argv.push("--no-sandbox".into());
        }
        if self.seccomp_log_only {
            argv.push("--seccomp-log-only".into());
        }
        #[cfg(all(unix, feature = "lua"))]
        if self.no_script_isolation {
            argv.push("--no-script-isolation".into());
        }
        argv
    }

    /// Start the isolated feed fetcher.
    ///
    /// There is no opt-out: the fetcher has the server resolve hostnames
    /// for it, so it works wherever the server does, and a spawn failure
    /// is fatal rather than a silent fall back to fetching untrusted feeds
    /// next to the database. `--no-sandbox` still lifts the fetcher's
    /// sandbox along with the server's.
    fn spawn_feed_fetcher(&self) -> Result<crate::process::FeedFetcherHandle> {
        #[cfg(unix)]
        {
            use crate::process::feed_fetcher::FeedFetcherHost;
            let host = FeedFetcherHost::spawn(self.seccomp_log_only, self.no_sandbox)
                .context("failed to start the isolated feed fetcher")?;
            Ok(Some(std::sync::Arc::new(host)))
        }
        #[cfg(not(unix))]
        {
            Ok(None)
        }
    }

    /// Start the isolated Lua script host, unless the operator opted out.
    ///
    /// A spawn failure is fatal rather than a silent fall back to the
    /// in-process VM: quietly running user scripts next to the database
    /// because a `fork` failed would be a security downgrade nobody
    /// asked for. The error names the flag that makes it explicit.
    #[cfg(all(unix, feature = "lua"))]
    fn spawn_script_host(&self) -> Result<crate::process::ScriptHostHandle> {
        use crate::process::script_host::ScriptHost;
        use std::sync::Arc;

        if self.no_script_isolation {
            tracing::warn!(
                "script isolation disabled via --no-script-isolation; Lua runs in the \
                 server process, with the same database and filesystem access it has"
            );
            return Ok(None);
        }

        let host = ScriptHost::spawn(self.seccomp_log_only, self.no_sandbox).map_err(|e| {
            anyhow!(
                "failed to start the isolated Lua script host: {e:#}. Pass \
                 --no-script-isolation to run scripts in the server process instead."
            )
        })?;
        Ok(Some(Arc::new(host)))
    }

    /// Resolve the path of the socket the server should listen on, as
    /// [`paths::resolve_socket_path`] defines it.
    ///
    /// # Errors
    ///
    /// Returns an error if the resolved socket path is too long to fit in a
    /// Unix socket address.
    fn resolve_socket_path(&self, data_dir: &paths::DataDir, env: &Env) -> Result<PathBuf> {
        let path = paths::resolve_socket_path(self.socket_path.as_deref(), data_dir, env);
        paths::validate_socket_path(&path)?;
        Ok(path)
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
///   * the parent directory of the Unix socket, so the socket file can be
///     created and unlinked.
fn build_sandbox_config(db_path: &Path, socket_dir: PathBuf, args: &ServeArgs) -> SandboxConfig {
    SandboxConfig::server(parent_or_cwd(db_path), socket_dir, args.seccomp_log_only)
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
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxProfile;
    use clap::Parser;
    use tempfile::TempDir;

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

    fn try_parse(argv: &[&str]) -> Result<ServeArgs, clap::Error> {
        Ok(TestCli::try_parse_from(std::iter::once("kiki").chain(argv.iter().copied()))?.serve)
    }

    /// Kiki serves over a Unix socket only: the TCP flags are gone, and
    /// asking for one is an error rather than a silently ignored argument.
    #[test]
    fn the_tcp_flags_are_rejected() {
        for argv in [
            vec!["--port", "8000"],
            vec!["-p", "8000"],
            vec!["--bind", "0.0.0.0"],
            vec!["-b", "0.0.0.0"],
        ] {
            assert!(try_parse(&argv).is_err(), "{argv:?} should no longer parse");
        }
    }

    /// The socket flag still parses under both spellings.
    #[test]
    fn the_uds_flag_still_parses() -> Result<()> {
        for argv in [vec!["--uds", "/tmp/k.sock"], vec!["-u", "/tmp/k.sock"]] {
            let args = try_parse(&argv)?;
            assert_eq!(args.socket_path.as_deref(), Some(Path::new("/tmp/k.sock")));
        }
        Ok(())
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
        let socket_dir = parent_or_cwd(&args.resolve_socket_path(&data_dir, env)?);
        Ok(build_sandbox_config(
            &data_dir.path.join(paths::DB_FILE_NAME),
            socket_dir,
            &args,
        ))
    }

    fn config_from(argv: &[&str]) -> SandboxConfig {
        config_from_env(argv, &Env::default()).unwrap()
    }

    /// Unpack the server profile, failing the test if `serve` somehow
    /// built any other one.
    fn server_paths(config: &SandboxConfig) -> (&PathBuf, &PathBuf) {
        match &config.profile {
            SandboxProfile::Server {
                data_dir,
                socket_dir,
            } => (data_dir, socket_dir),
            _ => panic!("serve must build a Server profile"),
        }
    }

    #[test]
    fn the_socket_directory_is_granted() {
        let config = config_from(&[]);
        let (data_dir, socket_dir) = server_paths(&config);
        assert_eq!(data_dir, &PathBuf::from("/data"));
        assert_eq!(socket_dir, &PathBuf::from("/data"));
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

        let (_, socket_dir) = server_paths(&config);
        assert_eq!(socket_dir, &PathBuf::from("/run/user/1000/kiki"));
        Ok(())
    }

    /// The same holds for a runtime directory named by `$KIKI_RUNTIME_DIR`:
    /// the sandbox grants it, and without the `kiki/` subdirectory the
    /// platform default would have added.
    #[test]
    fn uds_mode_grants_a_named_runtime_dir() -> Result<()> {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let config = config_from_env(&[], &env)?;

        let (data_dir, socket_dir) = server_paths(&config);
        assert_eq!(data_dir, &PathBuf::from("/data"));
        assert_eq!(socket_dir, &PathBuf::from("/run/kiki"));
        Ok(())
    }

    #[test]
    fn default_socket_honours_kiki_runtime_dir() -> Result<()> {
        let env = Env {
            kiki_runtime_dir: Some(PathBuf::from("/run/kiki")),
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let socket = parse(&[]).resolve_socket_path(&platform_dir(), &env)?;
        assert_eq!(socket, Path::new("/run/kiki/kiki.sock"));
        Ok(())
    }

    #[test]
    fn default_socket_is_in_the_runtime_dir() -> Result<()> {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let socket = parse(&[]).resolve_socket_path(&platform_dir(), &env)?;
        assert_eq!(socket, Path::new("/run/user/1000/kiki/kiki.sock"));
        Ok(())
    }

    #[test]
    fn uds_flag_overrides_the_default() -> Result<()> {
        let env = Env {
            runtime_dir: Some(PathBuf::from("/run/user/1000")),
            ..Env::default()
        };
        let socket =
            parse(&["--uds", "/tmp/elsewhere.sock"]).resolve_socket_path(&platform_dir(), &env)?;
        assert_eq!(socket, Path::new("/tmp/elsewhere.sock"));
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
        let socket = args.resolve_socket_path(&data_dir, &env)?;

        assert_eq!(socket, Path::new("/srv/kiki/kiki.sock"));
        Ok(())
    }

    #[test]
    fn an_over_long_socket_path_is_rejected() {
        let long = format!("/{}/kiki.sock", "a".repeat(paths::MAX_SOCKET_PATH_LEN));
        assert!(parse(&["--uds", &long])
            .resolve_socket_path(&platform_dir(), &Env::default())
            .is_err());
    }

    #[test]
    fn socket_dir_is_created_owner_only() -> Result<()> {
        let td = TempDir::with_prefix("kiki_")?;
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
        let td = TempDir::with_prefix("kiki_")?;
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

    /// Whatever flags were given survive a round trip through `to_argv`,
    /// so a `kiki serve` child sees exactly what the parent was asked for,
    /// listening on the socket the parent resolved.
    #[cfg(feature = "web-ui")]
    #[test]
    fn to_argv_round_trips() {
        let mut argvs = vec![vec![], vec!["--no-sandbox", "--seccomp-log-only"]];
        #[cfg(all(unix, feature = "lua"))]
        argvs.push(vec!["--no-script-isolation"]);

        for argv in argvs {
            let rendered = parse(&argv).to_argv(Path::new("/tmp/k.sock"));
            let rendered: Vec<&str> = rendered.iter().map(|s| s.to_str().unwrap()).collect();
            let expected: Vec<&str> = ["--uds", "/tmp/k.sock"].into_iter().chain(argv).collect();
            assert_eq!(rendered, expected);
        }
    }

    /// Script isolation is the default; opting out has to be explicit.
    #[cfg(all(unix, feature = "lua"))]
    #[test]
    fn script_isolation_is_on_unless_opted_out() {
        assert!(!parse(&[]).no_script_isolation);
        assert!(parse(&["--no-script-isolation"]).no_script_isolation);
    }

    /// `--no-script-isolation` returns no handle, so the server falls
    /// back to the in-process VM rather than half-wiring an absent child.
    #[cfg(all(unix, feature = "lua"))]
    #[test]
    fn opting_out_yields_no_script_host() {
        let handle = parse(&["--no-script-isolation"])
            .spawn_script_host()
            .expect("opting out must not fail");
        assert!(handle.is_none());
    }
}
