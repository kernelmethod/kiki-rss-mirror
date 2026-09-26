use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use crate::cli::init::{default_directory, InitArgs};
use crate::cli::paths::Env;

const SERVICE_NAME: &str = "kiki.service";

/// Arguments for the `kiki service` subcommand.
#[derive(Args)]
pub struct ServiceArgs {
    #[command(subcommand)]
    command: ServiceCommands,
}

#[derive(Subcommand)]
enum ServiceCommands {
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

    /// TCP port to listen on (defaults to UDS mode)
    #[arg(short, long)]
    port: Option<u16>,

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

impl ServiceArgs {
    /// Run the selected service subcommand.
    pub fn run(&self) -> Result<()> {
        match &self.command {
            ServiceCommands::Install(args) => install(args),
            ServiceCommands::Uninstall(args) => uninstall(args),
            ServiceCommands::Status => status(),
        }
    }
}

/// Returns the path to `~/.config/systemd/user/kiki.service`.
fn service_file_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().context("unable to determine platform config directory")?;
    Ok(config_dir.join("systemd/user").join(SERVICE_NAME))
}

/// Generate the systemd unit file contents.
///
/// The emitted unit layers systemd's process-hardening directives
/// (see systemd.exec(5)) on top of the in-process Landlock + seccomp
/// filters that `kiki serve` installs at startup. Directives that would
/// conflict with user-level execution (e.g. `PrivateUsers=yes`,
/// `ProtectHome=yes`) are deliberately omitted.
///
/// `named_home` says whether `data_dir` came from `$KIKI_HOME` in the
/// installing shell. If it did, the unit sets `$KIKI_HOME` too, so the
/// service resolves the same paths the CLI just did — a systemd user service
/// does not inherit the installing shell's environment. If it did not, the
/// unit says nothing and lets `kiki serve` fall back to the platform
/// defaults, which is what puts the socket in the runtime directory.
fn generate_unit_file(binary: &str, data_dir: &str, named_home: bool, port: Option<u16>) -> String {
    let listen_args = match port {
        Some(port) => format!(" -p {port}"),
        None => String::new(),
    };

    let environment = if named_home {
        format!("Environment=KIKI_HOME={data_dir}\n")
    } else {
        String::new()
    };

    // With no $KIKI_HOME the socket defaults to
    // `$XDG_RUNTIME_DIR/kiki/kiki.sock`. `ProtectSystem=strict` below would
    // leave that read-only, so the unit has to declare it; `RuntimeDirectory=`
    // also gets systemd to create it with the right mode and remove it again
    // when the service stops. With $KIKI_HOME set the socket lives in the
    // data directory instead, and no runtime directory is needed.
    let runtime_dir = if port.is_some() || named_home {
        String::new()
    } else {
        "RuntimeDirectory=kiki\nRuntimeDirectoryMode=0700\n".to_string()
    };

    format!(
        "\
[Unit]
Description=Kiki RSS feed aggregator (user)
After=default.target

[Service]
Type=simple
WorkingDirectory={data_dir}
{environment}{runtime_dir}ExecStartPre=\"{binary}\" init --check \"{data_dir}\"
ExecStart=\"{binary}\" serve{listen_args}
Restart=on-failure
RestartSec=5

# Hardening — see systemd.exec(5)
NoNewPrivileges=yes
ProtectSystem=strict
ReadWritePaths={data_dir}
PrivateTmp=yes
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
SystemCallFilter=@system-service
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

    let data_dir = default_directory()?;
    let data_dir_str = data_dir.display().to_string();
    let named_home = Env::from_process().kiki_home.is_some();

    // Initialize the data directory (idempotent — skips if already set up)
    let init_args = InitArgs::auto_with_check();
    init_args
        .run()
        .context("failed to initialize kiki data directory")?;

    let unit_contents = generate_unit_file(&binary_str, &data_dir_str, named_home, args.port);

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

    /// Without `$KIKI_HOME` the socket defaults into the runtime directory,
    /// which the unit must declare or `ProtectSystem=strict` leaves it
    /// read-only.
    #[test]
    fn uds_unit_declares_a_runtime_directory() {
        let unit = generate_unit_file("/usr/bin/kiki", DATA_DIR, false, None);

        assert!(unit.contains("RuntimeDirectory=kiki\n"));
        assert!(unit.contains("RuntimeDirectoryMode=0700\n"));
        assert!(!unit.contains("Environment=KIKI_HOME"));
        assert!(unit.contains("ExecStart=\"/usr/bin/kiki\" serve\n"));
    }

    /// Installed from a shell with `$KIKI_HOME` set, the unit carries it
    /// forward — a user service inherits nothing from that shell. The socket
    /// then lives in the data directory, so no runtime directory is needed.
    #[test]
    fn unit_carries_kiki_home_forward_when_it_named_the_directory() {
        let unit = generate_unit_file("/usr/bin/kiki", "/srv/kiki", true, None);

        assert!(unit.contains("Environment=KIKI_HOME=/srv/kiki\n"));
        assert!(!unit.contains("RuntimeDirectory"));
        assert!(unit.contains("ExecStart=\"/usr/bin/kiki\" serve\n"));
    }

    /// A TCP server binds no socket, so it needs no runtime directory.
    #[test]
    fn tcp_unit_omits_the_runtime_directory() {
        let unit = generate_unit_file("/usr/bin/kiki", DATA_DIR, false, Some(8000));

        assert!(!unit.contains("RuntimeDirectory"));
        assert!(unit.contains("ExecStart=\"/usr/bin/kiki\" serve -p 8000\n"));
    }

    /// The data directory is quoted, so a home directory containing a space
    /// does not split into two arguments.
    #[test]
    fn unit_quotes_paths_passed_to_the_binary() {
        let unit = generate_unit_file("/usr/bin/kiki", "/home/ada lovelace/kiki", false, None);

        assert!(unit
            .contains("ExecStartPre=\"/usr/bin/kiki\" init --check \"/home/ada lovelace/kiki\"\n"));
    }
}
