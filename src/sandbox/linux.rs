//! Linux-specific sandbox implementation (Landlock + seccomp-bpf).

use super::{SandboxConfig, SandboxProfile};
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetStatus, Scope, ABI,
};
use std::path::{Path, PathBuf};

/// Read-only paths needed to resolve hostnames and handle local time: the
/// resolver's configuration files and time zone data.
///
/// Granted to the server only. The feed fetcher asks the server to
/// resolve names for it (see [`crate::process::feed_fetcher`]) and keeps
/// time in UTC.
const RO_RESOLVER_PATHS: &[&str] = &[
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
];

/// Read-only paths needed to verify TLS certificates: the common
/// CA-certificate locations on Debian/Ubuntu, Fedora/RHEL, Arch, and
/// musl-based systems, and the entropy devices. Missing paths are
/// silently skipped.
///
/// Granted to both the server (which still fetches assets) and the feed
/// fetcher.
const RO_TLS_PATHS: &[&str] = &[
    // TLS trust stores
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/usr/share/ca-certificates",
    "/usr/local/share/ca-certificates",
    "/usr/lib/ssl",
    // Entropy
    "/dev/urandom",
    "/dev/random",
];

/// Read-only introspection paths granted to the server only: CPU
/// topology, limits, and cgroup info that tokio, num_cpus, and friends
/// may consult.
///
/// Deliberately withheld from the feed fetcher. Read access to `/proc`
/// includes other same-user processes' entries — the server's among them
/// — and everything the fetcher's runtime reads there has a fallback.
const RO_INTROSPECTION_PATHS: &[&str] = &["/proc", "/sys"];

fn existing(paths: &'static [&'static str]) -> impl Iterator<Item = &'static Path> {
    paths.iter().map(Path::new).filter(|p| p.exists())
}

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
            let ro_paths: Vec<&Path> = existing(RO_RESOLVER_PATHS)
                .chain(existing(RO_TLS_PATHS))
                .chain(existing(RO_INTROSPECTION_PATHS))
                .collect();
            (rw_paths, ro_paths)
        }
        SandboxProfile::ScriptHost => (Vec::new(), Vec::new()),
        SandboxProfile::FeedFetcher => (Vec::new(), existing(RO_TLS_PATHS).collect()),
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

    let mut ruleset = Ruleset::default().handle_access(all)?;
    if matches!(config.profile, SandboxProfile::FeedFetcher) {
        // Handling `BindTcp` with no rule allowing any port denies TCP
        // binds outright (Linux 6.7+, ABI v4) — redundant with seccomp's
        // `bind` denial, but independent of it. Scoping (Linux 6.12+, ABI
        // v6) stops the fetcher reaching abstract Unix sockets or
        // signalling processes outside its own sandbox — the server among
        // them. Both degrade silently on older kernels, as the filesystem
        // rules do; seccomp covers the socket side there.
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp)?
            .scope(Scope::AbstractUnixSocket | Scope::Signal)?;
    }
    let ruleset = ruleset
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

/// Extra syscalls denied to the feed fetcher: binding an address and
/// accepting connections. It only ever connects out, and since the server
/// resolves hostnames for it, it never calls `getaddrinfo` — which would
/// otherwise need to bind a netlink socket.
const DENIED_FEED_FETCHER: &[i64] = &[libc::SYS_bind, libc::SYS_listen, libc::SYS_accept4];

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
        SandboxProfile::FeedFetcher => &[DENIED_FEED_FETCHER, arch_specific_sockets],
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

    if matches!(config.profile, SandboxProfile::FeedFetcher) {
        apply_unix_socket_filter(arch, config.log_only)?;
    }

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

/// Refuse `socket(AF_UNIX, ...)` with `EACCES`.
///
/// The feed fetcher needs Internet sockets but has no use for Unix ones —
/// its channel to the server is inherited, and the supervisor makes its
/// worker channels with `socketpair`, which this does not touch. Without
/// it, a compromised fetcher could connect to the server's API socket,
/// which carries no authentication of its own.
///
/// The call fails with an error rather than killing the process: nothing
/// in the fetcher should try, but a library that probes for a local
/// service (as glibc's resolver does for nscd) should see it as absent
/// rather than crash-loop the worker. Installed as a second filter: the
/// kernel applies every installed filter and takes the most severe
/// verdict.
fn apply_unix_socket_filter(arch: seccompiler::TargetArch, log_only: bool) -> Result<()> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
        SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;

    let is_unix = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_UNIX as u64,
    )
    .context("building the AF_UNIX condition")?;
    let rules: BTreeMap<i64, Vec<SeccompRule>> = [(
        libc::SYS_socket,
        vec![SeccompRule::new(vec![is_unix]).context("building the AF_UNIX rule")?],
    )]
    .into_iter()
    .collect();

    let match_action = if log_only {
        SeccompAction::Log
    } else {
        SeccompAction::Errno(libc::EACCES as u32)
    };
    let filter = SeccompFilter::new(rules, SeccompAction::Allow, match_action, arch)
        .context("constructing the AF_UNIX seccomp filter")?;
    let program: BpfProgram = filter
        .try_into()
        .context("compiling the AF_UNIX seccomp filter")?;
    apply_filter_all_threads(&program).context("installing the AF_UNIX seccomp filter")?;
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
    fn feed_fetcher_gets_only_read_only_tls_paths() {
        let (rw, ro) = landlock_paths(&SandboxProfile::FeedFetcher);
        assert!(rw.is_empty(), "the feed fetcher must get no writable paths");
        assert!(
            !ro.iter()
                .any(|p| p.starts_with("/proc") || p.starts_with("/sys")),
            "the feed fetcher must not be able to read /proc or /sys: {ro:?}"
        );
        for p in &ro {
            assert!(
                RO_TLS_PATHS.iter().any(|allowed| Path::new(allowed) == *p),
                "unexpected read-only path for the feed fetcher: {p:?}"
            );
        }
    }

    #[test]
    fn the_feed_fetcher_may_connect_but_not_accept() {
        assert!(DENIED_FEED_FETCHER.contains(&libc::SYS_bind));
        assert!(DENIED_FEED_FETCHER.contains(&libc::SYS_listen));
        assert!(DENIED_FEED_FETCHER.contains(&libc::SYS_accept4));
        assert!(!DENIED_FEED_FETCHER.contains(&libc::SYS_socket));
        assert!(!DENIED_FEED_FETCHER.contains(&libc::SYS_connect));
    }

    #[test]
    fn every_profile_denies_exec() {
        assert!(DENIED_COMMON.contains(&libc::SYS_execve));
        assert!(DENIED_COMMON.contains(&libc::SYS_execveat));
    }
}
