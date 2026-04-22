//! Linux-specific sandbox implementation (Landlock + seccomp-bpf).

use super::SandboxConfig;
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
    ABI,
};
use std::path::{Path, PathBuf};

/// Read-only system paths granted to the sandboxed process. Covers the
/// DNS resolver's configuration files and the common CA-certificate
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

fn apply_landlock(config: &SandboxConfig) -> Result<()> {
    // ABI::V5 is supported on Linux 6.7+; the landlock crate's
    // compatibility layer transparently downgrades on older kernels
    // rather than failing outright.
    let abi = ABI::V5;
    let all = AccessFs::from_all(abi);
    let read_only = AccessFs::from_read(abi);

    let mut rw_paths: Vec<PathBuf> = vec![config.data_dir.clone()];
    if let Some(d) = &config.socket_dir {
        if !rw_paths.iter().any(|p| paths_equal(p, d)) {
            rw_paths.push(d.clone());
        }
    }

    let ro_paths: Vec<&Path> = RO_SYSTEM_PATHS
        .iter()
        .map(Path::new)
        .filter(|p| p.exists())
        .collect();

    let ruleset = Ruleset::default()
        .handle_access(all)?
        .create()?
        .add_rules(path_beneath_rules(&rw_paths, all))?
        .add_rules(path_beneath_rules(&ro_paths, read_only))?;

    let status = ruleset.restrict_self()?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => {
            tracing::info!(
                rw_paths = ?rw_paths,
                ro_paths = ?ro_paths,
                "landlock: filesystem sandbox fully enforced"
            );
        }
        RulesetStatus::PartiallyEnforced => {
            tracing::warn!(
                "landlock: filesystem sandbox only partially enforced \
                 (kernel may not support all requested access types)"
            );
        }
        RulesetStatus::NotEnforced => {
            tracing::warn!(
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

fn apply_seccomp(config: &SandboxConfig) -> Result<()> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;

    let arch = match detect_arch() {
        Some(a) => a,
        None => {
            tracing::warn!(
                "seccomp: unsupported target architecture, skipping syscall filter \
                 (landlock still active)"
            );
            return Ok(());
        }
    };

    // A curated denylist of syscalls that Kiki never legitimately makes
    // at runtime. Blocking them costs nothing and eliminates the most
    // dangerous post-exploitation primitives (ptrace, module loading,
    // namespace escape, kexec, etc.). Syscalls not on this list are
    // allowed, so a future dependency adding a new benign syscall won't
    // surprise us in production.
    let denied: &[i64] = &[
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
        // Kiki never spawns subprocesses. Blocking exec means an attacker
        // who gains code execution still can't pivot to running arbitrary
        // binaries.
        libc::SYS_execve,
        libc::SYS_execveat,
    ];

    // Architecture-specific entries: libc only exposes these constants
    // on the architectures where they exist as real syscalls.
    #[cfg(target_arch = "x86_64")]
    let arch_specific: &[i64] = &[libc::SYS_ioperm, libc::SYS_iopl];
    #[cfg(not(target_arch = "x86_64"))]
    let arch_specific: &[i64] = &[];

    let rules: BTreeMap<i64, Vec<SeccompRule>> = denied
        .iter()
        .chain(arch_specific.iter())
        .map(|&nr| (nr, Vec::new()))
        .collect();

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
            "seccomp: syscall filter installed in LOG-ONLY mode — violations will be logged \
             via the kernel audit subsystem but not blocked"
        );
    } else {
        tracing::info!(
            denied_syscalls = denied.len() + arch_specific.len(),
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
