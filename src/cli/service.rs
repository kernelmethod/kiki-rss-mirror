use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;

use crate::cli::init::{default_directory, InitArgs};

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
fn generate_unit_file(binary: &str, data_dir: &str, listen_args: &str) -> String {
    format!(
        "\
[Unit]
Description=Kiki RSS feed aggregator (user)
After=default.target

[Service]
Type=simple
WorkingDirectory={data_dir}
ExecStartPre={binary} init --auto --check
ExecStart={binary} serve{listen_args}
Restart=on-failure
RestartSec=5

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

    let listen_args = match args.port {
        Some(port) => format!(" -p {port}"),
        None => String::new(),
    };

    // Initialize the data directory (idempotent — skips if already set up)
    let init_args = InitArgs::auto_with_check();
    init_args
        .run()
        .context("failed to initialize kiki data directory")?;

    let unit_contents = generate_unit_file(&binary_str, &data_dir_str, &listen_args);

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
