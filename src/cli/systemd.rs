use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::init::{default_directory, InitArgs};
use crate::cli::paths::{self, Env};

const SERVICE_NAME: &str = "kiki.service";

/// Arguments for the `kiki systemd` subcommand.
#[derive(Args)]
pub struct SystemdArgs {
    #[command(subcommand)]
    command: SystemdCommands,
}

#[derive(Subcommand)]
enum SystemdCommands {
    /// Install a user-level systemd service for kiki
    Install(InstallArgs),

    /// Uninstall the kiki systemd user service
    Uninstall(UninstallArgs),

    /// Show the status of the kiki systemd user service
    Status,
}

#[derive(Args)]
struct InstallArgs {
    /// Path to the kiki binary (defaults to the currently running executable)
    #[arg(long)]
    binary_path: Option<PathBuf>,

    /// Enable the service to start on login
    #[arg(long)]
    enable: bool,
}

#[derive(Args)]
struct UninstallArgs {
    /// Also remove the kiki data directory (database, config, etc.)
    #[arg(long, conflicts_with = "keep_data")]
    remove_data: bool,

    /// Keep the kiki data directory (skip the prompt)
    #[arg(long, conflicts_with = "remove_data")]
    keep_data: bool,
}

impl SystemdArgs {
    /// Run the selected systemd subcommand.
    pub fn run(&self) -> Result<()> {
        match &self.command {
            SystemdCommands::Install(args) => install(args),
            SystemdCommands::Uninstall(args) => uninstall(args),
            SystemdCommands::Status => status(),
        }
    }
}

/// Returns the path to `~/.config/systemd/user/kiki.service`.
fn service_file_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("unable to determine platform config directory")?;
    Ok(config_dir.join("systemd/user").join(SERVICE_NAME))
}

/// How the generated unit accounts for the directory holding the socket.
#[derive(Debug, PartialEq, Eq)]
enum UnitRuntimeDir {
    /// The socket sits in the data directory. `ReadWritePaths=` on that
    /// directory already covers it.
    InDataDir,

    /// The socket sits inside `$XDG_RUNTIME_DIR`, which systemd owns:
    /// `RuntimeDirectory=` creates it with the right mode and removes it
    /// again when the service stops. The string is the path relative to
    /// `$XDG_RUNTIME_DIR`, which is the form that directive takes.
    Managed(String),

    /// The socket sits somewhere else entirely — `$KIKI_RUNTIME_DIR`
    /// pointing outside `$XDG_RUNTIME_DIR`. The unit can only ask for write
    /// access to it; whoever named the directory owns its lifecycle.
    External(String),
}

/// Decide how the unit should account for the socket's directory.
fn unit_runtime_dir(
    socket_dir: &Path,
    data_dir: &Path,
    platform_runtime_dir: Option<&Path>,
) -> UnitRuntimeDir {
    if socket_dir == data_dir {
        return UnitRuntimeDir::InDataDir;
    }

    // `RuntimeDirectory=` names a path relative to $XDG_RUNTIME_DIR, so it
    // can only manage a directory that actually sits inside it.
    let relative = platform_runtime_dir
        .and_then(|root| socket_dir.strip_prefix(root).ok())
        .and_then(|rel| rel.to_str())
        .filter(|rel| !rel.is_empty());

    match relative {
        Some(name) => UnitRuntimeDir::Managed(name.to_string()),
        None => UnitRuntimeDir::External(socket_dir.display().to_string()),
    }
}

/// The locations `install` resolved in the installing shell, which the unit
/// has to state again: a systemd user service inherits none of that shell's
/// environment.
struct UnitPaths<'a> {
    /// Kiki's home — `WorkingDirectory=`, `ReadWritePaths=`, and the value
    /// of `Environment=KIKI_HOME=` when `named_home`.
    data_dir: &'a str,

    /// Whether `data_dir` came from `$KIKI_HOME` rather than the platform
    /// default. If it did the unit sets `$KIKI_HOME` too, so the service
    /// resolves the same paths the CLI just did; if it did not, the unit
    /// says nothing and lets the platform default apply again.
    named_home: bool,

    /// `$KIKI_RUNTIME_DIR` from the installing shell, if it was set.
    kiki_runtime_dir: Option<&'a str>,

    /// `$KIKI_SOCKET` from the installing shell, if it was set.
    kiki_socket: Option<&'a str>,

    /// How the unit accounts for the directory the socket lives in.
    runtime: UnitRuntimeDir,
}

/// Generate the systemd unit file contents.
///
/// The emitted unit layers systemd's process-hardening directives
/// (see systemd.exec(5)) on top of the in-process Landlock + seccomp
/// filters that `kiki serve` installs at startup. Directives that would
/// conflict with user-level execution (e.g. `PrivateUsers=yes`,
/// `ProtectHome=yes`) are deliberately omitted.
///
/// Every one of Kiki's variables that the installing shell had set is
/// repeated in `Environment=`, so the service resolves exactly what the CLI
/// just did. The unit names no directory on a command line: `kiki init` and
/// `kiki serve` both read them out of the environment.
///
/// `ProtectSystem=strict` leaves the filesystem read-only, so anything Kiki
/// writes has to be carved back out: the data directory always, and the
/// socket's directory when that is somewhere else. See [`UnitRuntimeDir`].
fn generate_unit_file(binary: &str, paths: &UnitPaths) -> String {
    let UnitPaths {
        data_dir,
        named_home,
        kiki_runtime_dir,
        kiki_socket,
        runtime,
    } = paths;

    let mut environment = String::new();
    if *named_home {
        environment.push_str(&format!("Environment=\"KIKI_HOME={data_dir}\"\n"));
    }
    if let Some(dir) = kiki_runtime_dir {
        environment.push_str(&format!("Environment=\"KIKI_RUNTIME_DIR={dir}\"\n"));
    }
    if let Some(socket) = kiki_socket {
        environment.push_str(&format!("Environment=\"KIKI_SOCKET={socket}\"\n"));
    }

    let (runtime_dir, extra_writable) = match runtime {
        UnitRuntimeDir::InDataDir => (String::new(), String::new()),
        UnitRuntimeDir::Managed(name) => (
            format!("RuntimeDirectory={name}\nRuntimeDirectoryMode=0700\n"),
            String::new(),
        ),
        UnitRuntimeDir::External(dir) => (String::new(), format!("ReadWritePaths=\"{dir}\"\n")),
    };

    format!(
        "\
[Unit]
Description=Kiki RSS feed aggregator (user)
After=default.target

[Service]
Type=simple
WorkingDirectory={data_dir}
{environment}{runtime_dir}ExecStartPre=\"{binary}\" init --check
ExecStart=\"{binary}\" serve
Restart=on-failure
RestartSec=5

# Hardening — see systemd.exec(5)
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths=\"{data_dir}\"
{extra_writable}PrivateTmp=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
LockPersonality=yes
MemoryDenyWriteExecute=yes
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6
SystemCallArchitectures=native
SystemCallFilter=@system-service @sandbox
SystemCallFilter=~@privileged @resources @mount @swap @reboot @module @debug @cpu-emulation @obsolete @raw-io @keyring
UMask=0077

[Install]
WantedBy=default.target
"
    )
}

/// Run a `systemctl --user` command, logging a warning on failure.
fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    let mut cmd_args = vec!["--user"];
    cmd_args.extend_from_slice(args);

    let output = Command::new("systemctl")
        .args(&cmd_args)
        .output()
        .with_context(|| format!("failed to run systemctl {}", cmd_args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!(
            "systemctl {} exited with {}: {}",
            cmd_args.join(" "),
            output.status,
            stderr.trim()
        );
    }

    Ok(output)
}

/// Install the user-level systemd service.
fn install(args: &InstallArgs) -> Result<()> {
    let binary = match &args.binary_path {
        Some(p) => p.clone(),
        None => std::env::current_exe()
            .context("unable to determine current executable path")?
            .canonicalize()
            .context("unable to canonicalize current executable path")?,
    };

    let binary_str = binary.display().to_string();

    let env = Env::from_process();
    let data_dir = default_directory()?;
    let data_dir_str = data_dir.display().to_string();
    let named_home = env.kiki_home.is_some();

    // Initialize the data directory (idempotent — skips if already set up)
    let init_args = InitArgs::with_check();
    init_args
        .run()
        .context("failed to initialize kiki data directory")?;

    // Resolve the socket through the same function `kiki serve` uses, so
    // the unit grants write access to the directory the service will
    // actually bind in. `install` names a well-known data directory rather
    // than picking one up from the current directory, so the source is
    // whether `$KIKI_HOME` named it.
    let resolved = paths::DataDir {
        path: data_dir.clone(),
        source: if named_home {
            paths::DataDirSource::KikiHome
        } else {
            paths::DataDirSource::Platform
        },
    };
    let socket_path = paths::resolve_socket_path(None, &resolved, &env);
    let socket_dir = socket_path
        .parent()
        .context("resolved socket path has no parent directory")?;

    let unit_paths = UnitPaths {
        data_dir: &data_dir_str,
        named_home,
        kiki_runtime_dir: env.kiki_runtime_dir.as_deref().and_then(Path::to_str),
        kiki_socket: env.kiki_socket.as_deref().and_then(Path::to_str),
        runtime: unit_runtime_dir(socket_dir, &data_dir, env.runtime_dir.as_deref()),
    };

    let unit_contents = generate_unit_file(&binary_str, &unit_paths);

    let service_path = service_file_path()?;
    let parent = service_path
        .parent()
        .context("service file path has no parent")?;

    fs::create_dir_all(parent).with_context(|| format!("unable to create directory {parent:?}"))?;

    fs::write(&service_path, &unit_contents)
        .with_context(|| format!("unable to write service file to {service_path:?}"))?;

    println!("Wrote service file to {}", service_path.display());

    if let Err(e) = systemctl(&["daemon-reload"]) {
        println!("Warning: failed to reload systemd daemon: {e:#}");
        println!("You may need to run: systemctl --user daemon-reload");
    }

    if args.enable {
        if let Err(e) = systemctl(&["enable", SERVICE_NAME]) {
            println!("Warning: failed to enable service: {e:#}");
            println!("You may need to run: systemctl --user enable {SERVICE_NAME}");
        } else {
            println!("Service enabled.");
        }
    }

    println!();
    println!("Next steps:");
    if !args.enable {
        println!("  systemctl --user enable {SERVICE_NAME}   # start on login");
    }
    println!("  systemctl --user start {SERVICE_NAME}    # start now");
    println!("  systemctl --user status {SERVICE_NAME}   # check status");
    println!();
    println!("A user service stops when your last session ends, which also removes");
    println!("the runtime directory holding kiki's socket. To keep it running:");
    println!("  loginctl enable-linger $USER");

    Ok(())
}

/// Prompt the user with a y/n question. Returns `true` for yes.
fn prompt_yes_no(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .context("failed to read from stdin")?;

    Ok(matches!(input.trim(), "y" | "Y" | "yes" | "Yes" | "YES"))
}

/// Uninstall the user-level systemd service.
fn uninstall(args: &UninstallArgs) -> Result<()> {
    let service_path = service_file_path()?;

    if !service_path.exists() {
        println!("Service file not found at {}", service_path.display());
        return Ok(());
    }

    // Stop and disable before removing
    let _ = systemctl(&["stop", SERVICE_NAME]);
    let _ = systemctl(&["disable", SERVICE_NAME]);

    fs::remove_file(&service_path)
        .with_context(|| format!("unable to remove service file at {service_path:?}"))?;

    println!("Removed {}", service_path.display());

    if let Err(e) = systemctl(&["daemon-reload"]) {
        println!("Warning: failed to reload systemd daemon: {e:#}");
        println!("You may need to run: systemctl --user daemon-reload");
    }

    // Handle data directory removal
    let data_dir = default_directory()?;
    if data_dir.exists() {
        let should_remove = if args.remove_data {
            true
        } else if args.keep_data {
            false
        } else {
            prompt_yes_no(&format!("Remove data directory {}?", data_dir.display()))?
        };

        if should_remove {
            fs::remove_dir_all(&data_dir)
                .with_context(|| format!("unable to remove data directory {data_dir:?}"))?;
            println!("Removed {}", data_dir.display());
        }
    }

    Ok(())
}

/// Show the status of the user-level systemd service.
fn status() -> Result<()> {
    let output = Command::new("systemctl")
        .args(["--user", "status", SERVICE_NAME])
        .output()
        .context("failed to run systemctl --user status")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !stdout.is_empty() {
        print!("{stdout}");
    }
    if !stderr.is_empty() {
        eprint!("{stderr}");
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod test {
    use super::*;

    const DATA_DIR: &str = "/home/rey/.local/share/kiki";
    const XDG_RUNTIME: &str = "/run/user/1000";

    /// A `UnitPaths` for the plain case: platform data directory, socket in
    /// `$XDG_RUNTIME_DIR/kiki`, no variables set in the installing shell.
    fn paths() -> UnitPaths<'static> {
        UnitPaths {
            data_dir: DATA_DIR,
            named_home: false,
            kiki_runtime_dir: None,
            kiki_socket: None,
            runtime: UnitRuntimeDir::Managed("kiki".to_string()),
        }
    }

    // ----------------------------------------------------------------
    // unit_runtime_dir
    // ----------------------------------------------------------------

    /// A socket in the data directory needs nothing extra: the unit already
    /// makes that directory writable.
    #[test]
    fn a_socket_in_the_data_dir_needs_no_extra_grant() {
        assert_eq!(
            unit_runtime_dir(
                Path::new(DATA_DIR),
                Path::new(DATA_DIR),
                Some(Path::new(XDG_RUNTIME))
            ),
            UnitRuntimeDir::InDataDir
        );
    }

    /// The default socket directory sits inside `$XDG_RUNTIME_DIR`, so
    /// systemd can own it — `RuntimeDirectory=` takes the relative name.
    #[test]
    fn a_socket_under_xdg_runtime_dir_is_managed_by_systemd() {
        assert_eq!(
            unit_runtime_dir(
                Path::new("/run/user/1000/kiki"),
                Path::new(DATA_DIR),
                Some(Path::new(XDG_RUNTIME))
            ),
            UnitRuntimeDir::Managed("kiki".to_string())
        );
    }

    /// `$KIKI_RUNTIME_DIR` inside `$XDG_RUNTIME_DIR` is managed the same
    /// way, under whatever name it chose.
    #[test]
    fn a_named_runtime_dir_under_xdg_is_managed_too() {
        assert_eq!(
            unit_runtime_dir(
                Path::new("/run/user/1000/feeds"),
                Path::new(DATA_DIR),
                Some(Path::new(XDG_RUNTIME))
            ),
            UnitRuntimeDir::Managed("feeds".to_string())
        );
    }

    /// A runtime directory outside `$XDG_RUNTIME_DIR` is not systemd's to
    /// create, so the unit only asks for write access.
    #[test]
    fn a_runtime_dir_outside_xdg_is_external() {
        assert_eq!(
            unit_runtime_dir(
                Path::new("/srv/run/kiki"),
                Path::new(DATA_DIR),
                Some(Path::new(XDG_RUNTIME))
            ),
            UnitRuntimeDir::External("/srv/run/kiki".to_string())
        );
    }

    /// With no `$XDG_RUNTIME_DIR` at all there is nothing to be relative
    /// to, so any socket directory is external.
    #[test]
    fn without_xdg_runtime_dir_a_socket_dir_is_external() {
        assert_eq!(
            unit_runtime_dir(Path::new("/run/kiki"), Path::new(DATA_DIR), None),
            UnitRuntimeDir::External("/run/kiki".to_string())
        );
    }

    // ----------------------------------------------------------------
    // generate_unit_file
    // ----------------------------------------------------------------

    /// The socket defaults into the runtime directory, which the unit must
    /// declare or `ProtectSystem=strict` leaves it read-only.
    #[test]
    fn uds_unit_declares_a_runtime_directory() {
        let unit = generate_unit_file("/usr/bin/kiki", &paths());

        assert!(unit.contains("RuntimeDirectory=kiki\n"));
        assert!(unit.contains("RuntimeDirectoryMode=0700\n"));
        assert!(!unit.contains("Environment="));
        assert!(unit.contains("ExecStart=\"/usr/bin/kiki\" serve\n"));
    }

    /// Installed from a shell with `$KIKI_HOME` set, the unit carries it
    /// forward — a user service inherits nothing from that shell. The socket
    /// then lives in the data directory, so no runtime directory is needed.
    #[test]
    fn unit_carries_kiki_home_forward_when_it_named_the_directory() {
        let unit = generate_unit_file(
            "/usr/bin/kiki",
            &UnitPaths {
                data_dir: "/srv/kiki",
                named_home: true,
                runtime: UnitRuntimeDir::InDataDir,
                ..paths()
            },
        );

        assert!(unit.contains("Environment=\"KIKI_HOME=/srv/kiki\"\n"));
        assert!(!unit.contains("RuntimeDirectory"));
        assert!(unit.contains("ExecStart=\"/usr/bin/kiki\" serve\n"));
    }

    /// `$KIKI_RUNTIME_DIR` is carried forward for the same reason, and the
    /// directory it names is the one systemd is asked to create.
    #[test]
    fn unit_carries_kiki_runtime_dir_forward() {
        let unit = generate_unit_file(
            "/usr/bin/kiki",
            &UnitPaths {
                kiki_runtime_dir: Some("/run/user/1000/feeds"),
                runtime: UnitRuntimeDir::Managed("feeds".to_string()),
                ..paths()
            },
        );

        assert!(unit.contains("Environment=\"KIKI_RUNTIME_DIR=/run/user/1000/feeds\"\n"));
        assert!(unit.contains("RuntimeDirectory=feeds\n"));
        assert!(unit.contains("RuntimeDirectoryMode=0700\n"));
    }

    /// A runtime directory systemd cannot create is granted through
    /// `ReadWritePaths=` instead, or `ProtectSystem=strict` would leave the
    /// server unable to bind.
    #[test]
    fn unit_grants_write_access_to_an_external_runtime_dir() {
        let unit = generate_unit_file(
            "/usr/bin/kiki",
            &UnitPaths {
                kiki_runtime_dir: Some("/srv/run/kiki"),
                runtime: UnitRuntimeDir::External("/srv/run/kiki".to_string()),
                ..paths()
            },
        );

        assert!(unit.contains("Environment=\"KIKI_RUNTIME_DIR=/srv/run/kiki\"\n"));
        assert!(!unit.contains("RuntimeDirectory"));
        assert!(unit.contains("ReadWritePaths=\"/srv/run/kiki\"\n"));
        assert!(unit.contains(&format!("ReadWritePaths=\"{DATA_DIR}\"\n")));
    }

    /// `$KIKI_SOCKET` pins the socket file itself, and is carried forward
    /// like the rest.
    #[test]
    fn unit_carries_kiki_socket_forward() {
        let unit = generate_unit_file(
            "/usr/bin/kiki",
            &UnitPaths {
                kiki_socket: Some("/run/user/1000/kiki/custom.sock"),
                ..paths()
            },
        );

        assert!(unit.contains("Environment=\"KIKI_SOCKET=/run/user/1000/kiki/custom.sock\"\n"));
    }

    /// Paths are quoted, so a binary or directory containing a space does
    /// not split into two arguments or two `ReadWritePaths=` entries.
    #[test]
    fn unit_quotes_paths_containing_spaces() {
        let unit = generate_unit_file(
            "/opt/my tools/kiki",
            &UnitPaths {
                data_dir: "/home/ada lovelace/kiki",
                named_home: true,
                runtime: UnitRuntimeDir::InDataDir,
                ..paths()
            },
        );

        assert!(unit.contains("ExecStartPre=\"/opt/my tools/kiki\" init --check\n"));
        assert!(unit.contains("ExecStart=\"/opt/my tools/kiki\" serve\n"));
        assert!(unit.contains("Environment=\"KIKI_HOME=/home/ada lovelace/kiki\"\n"));
        assert!(unit.contains("ReadWritePaths=\"/home/ada lovelace/kiki\"\n"));
    }

    /// The subcommands take no directory argument: the unit hands them the
    /// environment and nothing else.
    #[test]
    fn unit_passes_no_directory_to_the_binary() {
        let unit = generate_unit_file("/usr/bin/kiki", &paths());

        assert!(unit.contains("ExecStartPre=\"/usr/bin/kiki\" init --check\n"));
        assert!(!unit.contains(&format!("init --check \"{DATA_DIR}\"")));
    }

    /// `kiki serve` installs its own Landlock and seccomp filters on startup.
    /// Those syscalls live in `@sandbox`, not `@system-service`, and a
    /// syscall outside the allowlist gets the process killed with SIGSYS.
    #[test]
    fn syscall_filter_allows_installing_the_sandbox() {
        let unit = generate_unit_file("/usr/bin/kiki", &paths());

        assert!(unit.contains("SystemCallFilter=@system-service @sandbox\n"));
    }
}
