//! Linux-specific sandbox implementation (Landlock + seccomp-bpf).

use super::{SandboxConfig, SandboxProfile};
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    ABI,
};
use std::path::{Path, PathBuf};

/// Read-only system paths granted to the sandboxed server process. Covers
/// the DNS resolver's configuration files and the common CA-certificate
/// locations on Debian/Ubuntu, Fedora/RHEL, Arch, and musl-based systems.
/// Missing paths are silently skipped.
const RO_SYSTEM_PATHS: &[&str] = &[
    // DNS + name resolution
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
    // Time zone data (chrono reads /etc/localtime, some crates read zoneinfo)
    "/etc/localtime",
    "/usr/share/zoneinfo",
    // TLS trust stores
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/usr/share/ca-certificates",
    "/usr/local/share/ca-certificates",
    "/usr/lib/ssl",
    // Process/system introspection (CPU topology, limits, cgroup info
    // that tokio, reqwest/hyper, num_cpus, and friends may consult).
    "/proc",
    "/sys",
    // Entropy
    "/dev/urandom",
    "/dev/random",
];

pub fn apply(config: &SandboxConfig) -> Result<()> {
    apply_landlock(config).context("installing landlock filesystem sandbox")?;
    apply_seccomp(config).context("installing seccomp-bpf syscall filter")?;
    Ok(())
}

/// Read-write and read-only path sets for a profile.
///
/// The script host gets neither: an empty ruleset that handles every
/// access right denies the entire filesystem, which is exactly what a
/// process that only ever talks to an inherited socket needs.
fn landlock_paths(profile: &SandboxProfile) -> (Vec<PathBuf>, Vec<&'static Path>) {
    match profile {
        SandboxProfile::Server {
            data_dir,
            socket_dir,
        } => {
            let mut rw_paths: Vec<PathBuf> = vec![data_dir.clone()];
            if let Some(d) = socket_dir {
                if !rw_paths.iter().any(|p| paths_equal(p, d)) {
                    rw_paths.push(d.clone());
                }
            }
            let ro_paths: Vec<&Path> = RO_SYSTEM_PATHS
                .iter()
                .map(Path::new)
                .filter(|p| p.exists())
                .collect();
            (rw_paths, ro_paths)
        }
        SandboxProfile::ScriptHost => (Vec::new(), Vec::new()),
    }
}

fn apply_landlock(config: &SandboxConfig) -> Result<()> {
    // ABI::V5 is supported on Linux 6.7+; the landlock crate's
    // compatibility layer transparently downgrades on older kernels
    // rather than failing outright.
    let abi = ABI::V5;
    let all = AccessFs::from_all(abi);
    let read_only = AccessFs::from_read(abi);

    let (rw_paths, ro_paths) = landlock_paths(&config.profile);

    let ruleset = Ruleset::default()
        .handle_access(all)?
        .create()?
        .add_rules(path_beneath_rules(&rw_paths, all))?
        .add_rules(path_beneath_rules(&ro_paths, read_only))?;

    let status = ruleset.restrict_self()?;
    let profile = config.profile_name();
    match status.ruleset {
        RulesetStatus::FullyEnforced => {
            tracing::info!(
                profile,
                rw_paths = ?rw_paths,
                ro_paths = ?ro_paths,
                "landlock: filesystem sandbox fully enforced"
            );
        }
        RulesetStatus::PartiallyEnforced => {
            tracing::warn!(
                profile,
                "landlock: filesystem sandbox only partially enforced \
                 (kernel may not support all requested access types)"
            );
        }
        RulesetStatus::NotEnforced => {
            tracing::warn!(
                profile,
                "landlock: filesystem sandbox NOT enforced \
                 (requires Linux >= 5.13 with CONFIG_SECURITY_LANDLOCK=y \
                 and the landlock LSM enabled at boot)"
            );
        }
    }
    Ok(())
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    std::fs::canonicalize(a)
        .and_then(|a| std::fs::canonicalize(b).map(|b| a == b))
        .unwrap_or_else(|_| a == b)
}

/// A curated denylist of syscalls no Kiki process ever legitimately makes
/// at runtime. Blocking them costs nothing and eliminates the most
/// dangerous post-exploitation primitives (ptrace, module loading,
/// namespace escape, kexec, etc.). Syscalls not on this list are allowed,
/// so a future dependency adding a new benign syscall won't surprise us
/// in production.
const DENIED_COMMON: &[i64] = &[
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_pivot_root,
    libc::SYS_chroot,
    libc::SYS_unshare,
    libc::SYS_setns,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_kexec_load,
    libc::SYS_kexec_file_load,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_add_key,
    libc::SYS_request_key,
    libc::SYS_keyctl,
    libc::SYS_reboot,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_settimeofday,
    libc::SYS_adjtimex,
    libc::SYS_clock_settime,
    libc::SYS_clock_adjtime,
    libc::SYS_personality,
    libc::SYS_acct,
    libc::SYS_quotactl,
    libc::SYS_mbind,
    libc::SYS_migrate_pages,
    libc::SYS_move_pages,
    libc::SYS_uselib,
    // No Kiki process spawns children after its sandbox is installed —
    // the server spawns the script host *before* calling `apply`.
    // Blocking exec means an attacker who gains code execution still
    // can't pivot to running arbitrary binaries.
    libc::SYS_execve,
    libc::SYS_execveat,
];

/// Extra syscalls denied to the script host: everything that creates a
/// socket or attaches one to a peer.
///
/// The host does its IPC over a socket pair it inherited at exec time, so
/// it needs no way to make a new one. `send`/`recv` on an already-open
/// descriptor stay allowed — std reads a `UnixStream` with `recv(2)` —
/// but with no way to obtain another descriptor, the only peer the host
/// can ever reach is the server that spawned it.
const DENIED_SCRIPT_HOST: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_socketpair,
    libc::SYS_connect,
    libc::SYS_bind,
    libc::SYS_listen,
    libc::SYS_accept4,
];

fn apply_seccomp(config: &SandboxConfig) -> Result<()> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;

    let arch = match detect_arch() {
        Some(a) => a,
        None => {
            tracing::warn!(
                profile = config.profile_name(),
                "seccomp: unsupported target architecture, skipping syscall filter \
                 (landlock still active)"
            );
            return Ok(());
        }
    };

    // Architecture-specific entries: libc only exposes these constants
    // on the architectures where they exist as real syscalls.
    #[cfg(target_arch = "x86_64")]
    let arch_specific: &[i64] = &[libc::SYS_ioperm, libc::SYS_iopl];
    #[cfg(not(target_arch = "x86_64"))]
    let arch_specific: &[i64] = &[];

    // `accept` is a distinct syscall only on architectures that predate
    // `accept4`; aarch64 and riscv64 expose `accept4` alone.
    #[cfg(target_arch = "x86_64")]
    let arch_specific_sockets: &[i64] = &[libc::SYS_accept];
    #[cfg(not(target_arch = "x86_64"))]
    let arch_specific_sockets: &[i64] = &[];

    let profile_denied: &[&[i64]] = match config.profile {
        SandboxProfile::Server { .. } => &[],
        SandboxProfile::ScriptHost => &[DENIED_SCRIPT_HOST, arch_specific_sockets],
    };

    let denied: Vec<i64> = DENIED_COMMON
        .iter()
        .chain(arch_specific.iter())
        .chain(profile_denied.iter().flat_map(|s| s.iter()))
        .copied()
        .collect();

    let rules: BTreeMap<i64, Vec<SeccompRule>> =
        denied.iter().map(|&nr| (nr, Vec::new())).collect();

    let match_action = if config.log_only {
        SeccompAction::Log
    } else {
        SeccompAction::KillProcess
    };

    let filter = SeccompFilter::new(rules, SeccompAction::Allow, match_action, arch)
        .context("constructing seccomp filter")?;
    let program: BpfProgram = filter.try_into().context("compiling seccomp BPF program")?;
    apply_filter_all_threads(&program).context("installing seccomp BPF program")?;

    if config.log_only {
        tracing::warn!(
            profile = config.profile_name(),
            "seccomp: syscall filter installed in LOG-ONLY mode — violations will be logged \
             via the kernel audit subsystem but not blocked"
        );
    } else {
        tracing::info!(
            profile = config.profile_name(),
            denied_syscalls = denied.len(),
            "seccomp: syscall filter installed (denylist, kill-on-violation)"
        );
    }
    Ok(())
}

fn detect_arch() -> Option<seccompiler::TargetArch> {
    use seccompiler::TargetArch;
    #[cfg(target_arch = "x86_64")]
    {
        Some(TargetArch::x86_64)
    }
    #[cfg(target_arch = "aarch64")]
    {
        Some(TargetArch::aarch64)
    }
    #[cfg(target_arch = "riscv64")]
    {
        Some(TargetArch::riscv64)
    }
    #[cfg(not(any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )))]
    {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn script_host_gets_no_filesystem_access() {
        let (rw, ro) = landlock_paths(&SandboxProfile::ScriptHost);
        assert!(rw.is_empty(), "script host must get no writable paths");
        assert!(ro.is_empty(), "script host must get no readable paths");
    }

    #[test]
    fn server_profile_grants_data_and_socket_dirs() {
        let (rw, _) = landlock_paths(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: Some(PathBuf::from("/run/kiki")),
        });
        assert_eq!(
            rw,
            vec![PathBuf::from("/var/lib/kiki"), PathBuf::from("/run/kiki")]
        );
    }

    #[test]
    fn server_profile_deduplicates_identical_dirs() {
        let (rw, _) = landlock_paths(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: Some(PathBuf::from("/var/lib/kiki")),
        });
        assert_eq!(rw, vec![PathBuf::from("/var/lib/kiki")]);
    }

    #[test]
    fn socket_syscalls_are_denied_only_to_the_script_host() {
        assert!(!DENIED_COMMON.contains(&libc::SYS_socket));
        assert!(DENIED_SCRIPT_HOST.contains(&libc::SYS_socket));
        assert!(DENIED_SCRIPT_HOST.contains(&libc::SYS_connect));
    }

    #[test]
    fn every_profile_denies_exec() {
        assert!(DENIED_COMMON.contains(&libc::SYS_execve));
        assert!(DENIED_COMMON.contains(&libc::SYS_execveat));
    }
}
