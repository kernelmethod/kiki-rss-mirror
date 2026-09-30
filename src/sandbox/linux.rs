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
/// musl-based systems. Missing paths are silently skipped.
///
/// Granted only to the feed fetcher, which makes every one of Kiki's
/// outbound HTTP(S) requests: feeds, assets and favicons alike.
const RO_TLS_PATHS: &[&str] = &[
    "/etc/ssl",
    "/etc/pki",
    "/etc/ca-certificates",
    "/usr/share/ca-certificates",
    "/usr/local/share/ca-certificates",
    "/usr/lib/ssl",
];

/// The entropy devices, read-only, for anything that reads them rather
/// than calling `getrandom`. Granted to the server and the feed fetcher.
const RO_ENTROPY_PATHS: &[&str] = &["/dev/urandom", "/dev/random"];

/// Read-only introspection paths granted to the server only: CPU
/// topology, limits, and cgroup info that tokio, num_cpus, and friends
/// may consult.
///
/// Deliberately withheld from the feed fetcher. Read access to `/proc`
/// includes other same-user processes' entries — the server's among them
/// — and everything the fetcher's runtime reads there has a fallback.
const RO_INTROSPECTION_PATHS: &[&str] = &["/proc", "/sys"];

fn existing(paths: &'static [&'static str]) -> impl Iterator<Item = PathBuf> {
    paths.iter().map(PathBuf::from).filter(|p| p.exists())
}

/// Trust-store locations named by `SSL_CERT_FILE` and `SSL_CERT_DIR`
/// (the latter a `:`-separated list), which the TLS stack consults
/// before the defaults in [`RO_TLS_PATHS`]. Missing paths are skipped.
///
/// Without these, a system whose only CA bundle lives elsewhere — Nix
/// builds and some NixOS setups point `SSL_CERT_FILE` into the store —
/// leaves the feed fetcher with no trust store, and it cannot build its
/// HTTP client at all.
fn tls_env_paths() -> Vec<PathBuf> {
    let file = std::env::var_os("SSL_CERT_FILE").map(PathBuf::from);
    let dirs = std::env::var_os("SSL_CERT_DIR")
        .map(|v| std::env::split_paths(&v).collect::<Vec<_>>())
        .unwrap_or_default();
    file.into_iter()
        .chain(dirs)
        .filter(|p| !p.as_os_str().is_empty() && p.exists())
        .collect()
}

/// How deep [`symlink_targets`] looks below each directory it is given.
/// Trust stores and zoneinfo nest a level or two; this leaves headroom
/// without walking anything unbounded.
const SYMLINK_SEARCH_DEPTH: usize = 4;

/// Where the symlinks below the directories in `paths` lead, for those
/// that lead outside all of `paths`.
///
/// Landlock checks the file a path finally resolves to, not the path
/// itself, so a rule on a directory does not cover a symlink inside it
/// that points elsewhere. That is how NixOS lays out `/etc`: the CA
/// bundles in `/etc/ssl/certs` (and `/etc/hosts`, `/etc/nsswitch.conf`,
/// ...) are symlinks into `/nix/store`, and without their targets the
/// sandboxed processes find no trust store at all. Rules on the paths
/// themselves are unaffected — Landlock opens them following symlinks —
/// so only symlinks *inside* a granted directory need this.
///
/// Symlinked directories are granted whole and not descended into.
/// Dangling symlinks, and anything that cannot be read, are skipped.
fn symlink_targets(paths: &[PathBuf]) -> Vec<PathBuf> {
    let roots: Vec<PathBuf> = paths
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
    let mut targets = Vec::new();
    for root in &roots {
        collect_symlink_targets(root, SYMLINK_SEARCH_DEPTH, &roots, &mut targets);
    }
    targets
}

fn collect_symlink_targets(dir: &Path, depth: usize, roots: &[PathBuf], out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // not a directory, or unreadable
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_symlink() {
            let Ok(target) = std::fs::canonicalize(&path) else {
                continue; // dangling
            };
            if !roots.iter().any(|r| target.starts_with(r)) && !out.contains(&target) {
                out.push(target);
            }
        } else if file_type.is_dir() && depth > 0 {
            collect_symlink_targets(&path, depth - 1, roots, out);
        }
    }
}

/// Read-only paths for the TLS trust stores, including those named by
/// `SSL_CERT_FILE`/`SSL_CERT_DIR` and wherever their symlinks lead, and
/// the entropy devices.
fn tls_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = existing(RO_TLS_PATHS).chain(tls_env_paths()).collect();
    paths.extend(symlink_targets(&paths));
    paths.extend(existing(RO_ENTROPY_PATHS));
    paths
}

/// Read-only paths for name resolution and time zones, including wherever
/// their symlinks lead.
fn resolver_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = existing(RO_RESOLVER_PATHS).collect();
    paths.extend(symlink_targets(&paths));
    paths
}

pub fn apply(config: &SandboxConfig) -> Result<()> {
    apply_landlock(config).context("installing landlock filesystem sandbox")?;
    apply_seccomp(config).context("installing seccomp-bpf syscall filter")?;
    Ok(())
}

/// Read-write and read-only path sets for a profile.
///
/// The script host and the web UI get neither: an empty ruleset that
/// handles every access right denies the entire filesystem, which is
/// exactly what a process that only ever talks to sockets needs.
fn landlock_paths(profile: &SandboxProfile) -> (Vec<PathBuf>, Vec<PathBuf>) {
    match profile {
        SandboxProfile::Server {
            data_dir,
            socket_dir,
            temp_dir,
        } => {
            // The socket usually lives in the data directory, in which case
            // the one rule already covers it.
            let mut rw_paths: Vec<PathBuf> = vec![data_dir.clone()];
            if !paths_equal(data_dir, socket_dir) {
                rw_paths.push(socket_dir.clone());
            }
            // So does SQLite's temp directory, unless `SQLITE_TMPDIR`
            // named one elsewhere.
            if !rw_paths.iter().any(|p| path_within(temp_dir, p)) {
                rw_paths.push(temp_dir.clone());
            }
            // No TLS trust stores: the server makes no HTTP(S) requests
            // of its own, as the feed fetcher downloads assets too.
            let ro_paths: Vec<PathBuf> = resolver_paths()
                .into_iter()
                .chain(existing(RO_ENTROPY_PATHS))
                .chain(existing(RO_INTROSPECTION_PATHS))
                .collect();
            (rw_paths, ro_paths)
        }
        SandboxProfile::ScriptHost | SandboxProfile::WebUi => (Vec::new(), Vec::new()),
        SandboxProfile::FeedFetcher => (Vec::new(), tls_paths()),
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
    if matches!(config.profile, SandboxProfile::WebUi) {
        // Handling both TCP rights with no rule allowing any port denies
        // every TCP bind and connect (Linux 6.7+, ABI v4). The listener was
        // bound before the sandbox went up, and the API is reached over a
        // Unix socket, so the web UI needs neither. Signals stay unscoped:
        // the web UI stops its `kiki serve` child with `SIGTERM`, and that
        // child was started outside this sandbox.
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
            .scope(Scope::AbstractUnixSocket)?;
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

/// Whether `path` is `dir` or lies below it, comparing canonical paths
/// where both exist.
fn path_within(path: &Path, dir: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(dir)) {
        (Ok(path), Ok(dir)) => path.starts_with(dir),
        _ => path.starts_with(dir),
    }
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
    // io_uring performs I/O on the process's behalf without passing
    // through seccomp at all, and has been a steady source of kernel
    // exploits; userfaultfd is the classic primitive for widening kernel
    // race windows. Tokio and SQLite use neither.
    libc::SYS_io_uring_setup,
    libc::SYS_io_uring_enter,
    libc::SYS_io_uring_register,
    libc::SYS_userfaultfd,
    // Reaching into, or comparing, another process's resources:
    // ptrace-adjacent, and never needed.
    libc::SYS_pidfd_getfd,
    libc::SYS_process_madvise,
    libc::SYS_kcmp,
    // Opening files by handle sidesteps path-based checks.
    libc::SYS_name_to_handle_at,
    libc::SYS_open_by_handle_at,
    // Host administration.
    libc::SYS_fanotify_init,
    libc::SYS_syslog,
    libc::SYS_vhangup,
    libc::SYS_sethostname,
    libc::SYS_setdomainname,
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

/// The only address families the server may create sockets in: Unix, for
/// its API listener and for resolvers that answer over a local socket
/// (nscd, sssd, systemd-resolved), and IPv4/IPv6, for DNS queries sent
/// straight to a name server. Everything else — netlink, packet, `AF_ALG`,
/// Bluetooth, and the rest of the long tail of rarely-exercised kernel
/// protocol code — is refused.
///
/// glibc's `getaddrinfo` also opens a netlink socket to learn which
/// address families the host has configured; when that fails it assumes
/// both, and resolution carries on. The systemd unit's
/// `RestrictAddressFamilies=` makes the same choice.
const SERVER_SOCKET_FAMILIES: &[i32] = &[libc::AF_UNIX, libc::AF_INET, libc::AF_INET6];

/// Which `socket(2)` address families a profile may use.
enum SocketDomains {
    /// Refuse these families and allow every other.
    Deny(&'static [i32]),
    /// Allow only these families and refuse every other.
    AllowOnly(&'static [i32]),
}

/// Extra syscalls denied to the web UI: binding an address and listening.
/// Its listener is bound and listening before the sandbox goes up, and it
/// only ever accepts on that one, so `accept4` stays allowed.
const DENIED_WEB_UI: &[i64] = &[libc::SYS_bind, libc::SYS_listen];

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
        SandboxProfile::WebUi => &[DENIED_WEB_UI],
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

    let domains = match config.profile {
        // Unix, IPv4 and IPv6 sockets only.
        SandboxProfile::Server { .. } => Some(SocketDomains::AllowOnly(SERVER_SOCKET_FAMILIES)),
        // Internet sockets only: no Unix ones.
        SandboxProfile::FeedFetcher => Some(SocketDomains::Deny(&[libc::AF_UNIX])),
        // Unix sockets only: no Internet (or netlink, or packet) ones.
        SandboxProfile::WebUi => Some(SocketDomains::AllowOnly(&[libc::AF_UNIX])),
        // Every socket call is already denied outright.
        SandboxProfile::ScriptHost => None,
    };
    if let Some(domains) = domains {
        apply_socket_domain_filter(arch, &domains, config.log_only)?;
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

/// Refuse with `EACCES` every `socket(domain, ...)` call whose `domain`
/// `domains` does not allow.
///
/// For the feed fetcher this refuses Unix sockets: it needs Internet
/// sockets but has no use for Unix ones — its channel to the server is
/// inherited, and the supervisor makes its worker channels with
/// `socketpair`, which this does not touch. Without it, a compromised
/// fetcher could connect to the server's API socket, which carries no
/// authentication of its own.
///
/// For the web UI it refuses everything *but* Unix sockets: it talks to
/// the API over the server's Unix socket and to browsers over a listener
/// it bound before the sandbox went up, so a new Internet socket could
/// only be a compromised web UI reaching out.
///
/// For the server it allows [`SERVER_SOCKET_FAMILIES`] alone.
///
/// The call fails with an error rather than killing the process: nothing
/// should try, but a library that probes for a local service (as glibc's
/// resolver does for nscd) should see it as absent rather than crash the
/// process. Installed as a separate filter: the kernel applies every
/// installed filter and takes the most severe verdict.
fn apply_socket_domain_filter(
    arch: seccompiler::TargetArch,
    domains: &SocketDomains,
    log_only: bool,
) -> Result<()> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
        SeccompCondition, SeccompFilter, SeccompRule,
    };
    use std::collections::BTreeMap;

    let domain_is = |op: SeccompCmpOp, family: i32| {
        SeccompCondition::new(0, SeccompCmpArgLen::Dword, op, family as u64)
            .context("building a socket domain condition")
    };
    // A syscall matches when any one rule does, and a rule when all of
    // its conditions do.
    let socket_rules: Vec<SeccompRule> = match domains {
        SocketDomains::Deny(families) => families
            .iter()
            .map(|&f| {
                SeccompRule::new(vec![domain_is(SeccompCmpOp::Eq, f)?])
                    .context("building a socket domain rule")
            })
            .collect::<Result<_>>()?,
        SocketDomains::AllowOnly(families) => {
            let conditions = families
                .iter()
                .map(|&f| domain_is(SeccompCmpOp::Ne, f))
                .collect::<Result<_>>()?;
            vec![SeccompRule::new(conditions).context("building the socket domain rule")?]
        }
    };
    let rules: BTreeMap<i64, Vec<SeccompRule>> =
        [(libc::SYS_socket, socket_rules)].into_iter().collect();

    let match_action = if log_only {
        SeccompAction::Log
    } else {
        SeccompAction::Errno(libc::EACCES as u32)
    };
    let filter = SeccompFilter::new(rules, SeccompAction::Allow, match_action, arch)
        .context("constructing the socket domain seccomp filter")?;
    let program: BpfProgram = filter
        .try_into()
        .context("compiling the socket domain seccomp filter")?;
    apply_filter_all_threads(&program).context("installing the socket domain seccomp filter")?;
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

    /// A symlink below a granted directory that leads outside it — the
    /// NixOS `/etc/ssl/certs` layout — has its target granted too; one
    /// that stays inside, or dangles, adds nothing.
    #[test]
    fn symlinks_leading_outside_granted_dirs_are_followed() {
        let td = tempfile::TempDir::with_prefix("kiki_sandbox").unwrap();
        let store = td.path().join("store");
        let etc_ssl = td.path().join("etc/ssl");
        std::fs::create_dir_all(store.join("certs-dir")).unwrap();
        std::fs::create_dir_all(etc_ssl.join("certs")).unwrap();
        std::fs::write(store.join("ca-bundle.crt"), "").unwrap();
        std::fs::write(etc_ssl.join("local.pem"), "").unwrap();

        let certs = etc_ssl.join("certs");
        let link = |target: &Path, name: &str| {
            std::os::unix::fs::symlink(target, certs.join(name)).unwrap();
        };
        link(&store.join("ca-bundle.crt"), "ca-certificates.crt");
        link(&store.join("certs-dir"), "extra");
        link(&etc_ssl.join("local.pem"), "local.pem");
        link(&td.path().join("missing"), "dangling");

        let mut targets = symlink_targets(&[etc_ssl]);
        targets.sort();
        let store = std::fs::canonicalize(&store).unwrap();
        assert_eq!(
            targets,
            vec![store.join("ca-bundle.crt"), store.join("certs-dir")]
        );
    }

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
            socket_dir: PathBuf::from("/run/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
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
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
        });
        assert_eq!(rw, vec![PathBuf::from("/var/lib/kiki")]);
    }

    /// A temp directory outside the data directory — one named by
    /// `SQLITE_TMPDIR` — is granted too, or SQLite could not create its
    /// temporary files there.
    #[test]
    fn server_profile_grants_a_temp_dir_outside_the_data_dir() {
        let (rw, _) = landlock_paths(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/tmp/kiki"),
        });
        assert_eq!(
            rw,
            vec![
                PathBuf::from("/var/lib/kiki"),
                PathBuf::from("/var/tmp/kiki")
            ]
        );
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
        let env_paths = tls_env_paths();
        for p in &ro {
            assert!(
                RO_TLS_PATHS
                    .iter()
                    .chain(RO_ENTROPY_PATHS)
                    .any(|allowed| Path::new(allowed) == p)
                    || env_paths.contains(p),
                "unexpected read-only path for the feed fetcher: {p:?}"
            );
        }
    }

    /// Every HTTP(S) request is made by the feed fetcher, so the server has
    /// no use for the TLS trust stores.
    #[test]
    fn server_gets_no_tls_trust_stores() {
        let (_, ro) = landlock_paths(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
        });
        for p in &ro {
            assert!(
                !RO_TLS_PATHS.iter().any(|tls| p.starts_with(tls)),
                "the server must not be granted a TLS trust store: {p:?}"
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
    fn web_ui_gets_no_filesystem_access() {
        let (rw, ro) = landlock_paths(&SandboxProfile::WebUi);
        assert!(rw.is_empty(), "the web UI must get no writable paths");
        assert!(ro.is_empty(), "the web UI must get no readable paths");
    }

    /// The web UI accepts on the listener it bound before the sandbox, and
    /// connects to the API's Unix socket, but opens no new listener.
    #[test]
    fn the_web_ui_may_accept_and_connect_but_not_listen() {
        assert!(DENIED_WEB_UI.contains(&libc::SYS_bind));
        assert!(DENIED_WEB_UI.contains(&libc::SYS_listen));
        assert!(!DENIED_WEB_UI.contains(&libc::SYS_accept4));
        assert!(!DENIED_WEB_UI.contains(&libc::SYS_socket));
        assert!(!DENIED_WEB_UI.contains(&libc::SYS_connect));
    }

    /// The server keeps the families its API listener and the system
    /// resolver need, and nothing else.
    #[test]
    fn the_server_may_only_create_unix_and_internet_sockets() {
        for family in [libc::AF_UNIX, libc::AF_INET, libc::AF_INET6] {
            assert!(SERVER_SOCKET_FAMILIES.contains(&family));
        }
        for family in [libc::AF_NETLINK, libc::AF_PACKET, libc::AF_ALG] {
            assert!(!SERVER_SOCKET_FAMILIES.contains(&family));
        }
    }

    #[test]
    fn every_profile_denies_io_uring_and_userfaultfd() {
        assert!(DENIED_COMMON.contains(&libc::SYS_io_uring_setup));
        assert!(DENIED_COMMON.contains(&libc::SYS_userfaultfd));
    }

    #[test]
    fn every_profile_denies_exec() {
        assert!(DENIED_COMMON.contains(&libc::SYS_execve));
        assert!(DENIED_COMMON.contains(&libc::SYS_execveat));
    }
}
