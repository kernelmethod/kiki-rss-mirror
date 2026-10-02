//! Linux-specific sandbox implementation (Landlock, seccomp-bpf, and
//! `PR_SET_MDWE`).

use super::{SandboxConfig, SandboxProfile};
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, BitFlags, LandlockStatus, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
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
/// Granted to the feed fetcher, which makes every one of Kiki's outbound
/// HTTP(S) requests: feeds, assets and favicons alike. The server is
/// granted them too, but only so that the fetcher, which it starts under
/// its own Landlock domain, can have them: see [`server_child_paths`].
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

/// The dynamic loader's cache and preload list, which it reads while
/// loading a program but does not keep mapped.
const RO_LOADER_PATHS: &[&str] = &["/etc/ld.so.cache", "/etc/ld.so.preload"];

/// Where `Stdio::null` points a child's standard streams. The server opens
/// it to start each child, read-only for stdin and write-only for stdout.
const NULL_DEVICE: &str = "/dev/null";

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
    restrict_filesystem(config)?;
    restrict_syscalls(config)
}

pub fn restrict_filesystem(config: &SandboxConfig) -> Result<()> {
    apply_landlock(config).context("installing landlock filesystem sandbox")
}

pub fn restrict_syscalls(config: &SandboxConfig) -> Result<()> {
    apply_mdwe(config).context("refusing writable and executable memory")?;
    apply_seccomp(config).context("installing seccomp-bpf syscall filter")
}

/// Refuse memory that is writable and executable, with
/// `prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN)` (Linux 6.3+).
///
/// From then on the kernel fails, with `EACCES`, any `mmap` asking for
/// `PROT_WRITE | PROT_EXEC`, and any `mmap` or `mprotect` that would make
/// a mapping executable that was not already — so injected code can no
/// longer be written into memory and then run. No Kiki process needs that:
/// Lua 5.4 is an interpreter, with no JIT. This is what systemd's
/// `MemoryDenyWriteExecute=` installs where the kernel has it, here
/// applied however Kiki is started.
///
/// The setting cannot be undone, and is inherited by forked children (the
/// feed fetcher's workers) and kept across `execve`. Like Landlock, it has
/// no log-only mode, so `log_only` does not affect it.
///
/// An older kernel that lacks it is logged and otherwise ignored.
fn apply_mdwe(config: &SandboxConfig) -> Result<()> {
    let profile = config.profile_name();
    match refuse_write_exec().context("prctl(PR_SET_MDWE)")? {
        Mdwe::Refused => {
            tracing::info!(profile, "mdwe: writable and executable memory refused");
        }
        Mdwe::AlreadyRefused => {
            // By systemd's MemoryDenyWriteExecute=, or inherited.
            tracing::info!(
                profile,
                "mdwe: writable and executable memory already refused"
            );
        }
        Mdwe::Unsupported => {
            tracing::warn!(
                profile,
                "mdwe: writable and executable memory NOT refused \
                 (requires Linux >= 6.3)"
            );
        }
    }
    Ok(())
}

/// What [`refuse_write_exec`] found or did.
#[derive(Debug, PartialEq, Eq)]
enum Mdwe {
    /// The process now refuses writable and executable memory.
    Refused,
    /// The process already refused it.
    AlreadyRefused,
    /// The kernel predates `PR_SET_MDWE`.
    Unsupported,
}

/// The bare `prctl` calls behind [`apply_mdwe`], which neither logs nor
/// allocates, so that a forked child can make them too.
fn refuse_write_exec() -> std::io::Result<Mdwe> {
    let flags = libc::PR_MDWE_REFUSE_EXEC_GAIN as libc::c_ulong;

    // SAFETY: `PR_GET_MDWE` takes no pointers and only reads the calling
    // process's flags; the unused arguments must be zero.
    let current = unsafe { libc::prctl(libc::PR_GET_MDWE, 0, 0, 0, 0) };
    if current >= 0 && current as libc::c_ulong & flags == flags {
        return Ok(Mdwe::AlreadyRefused);
    }

    // SAFETY: `PR_SET_MDWE` takes no pointers and only changes the calling
    // process's flags; the unused arguments must be zero.
    if unsafe { libc::prctl(libc::PR_SET_MDWE, flags, 0, 0, 0) } == 0 {
        return Ok(Mdwe::Refused);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EINVAL) {
        return Ok(Mdwe::Unsupported);
    }
    Err(err)
}

/// Paths the server is granted for its children's sake, as
/// `(executable, read_only, read_write)`.
///
/// The server installs its Landlock rules before it starts its children
/// (see [`crate::sandbox::restrict_filesystem`]), so each child runs under
/// the server's rules as well as its own, and can reach only what both
/// allow. Between them, the children need to:
///
/// * be executed: the kiki executable itself, and every file the server
///   has mapped — the dynamic loader and the shared libraries, which a
///   child, being the same executable, loads again — plus the loader's
///   cache and preload list;
/// * have their standard streams pointed at `/dev/null`;
/// * verify TLS certificates, in the feed fetcher's case.
///
/// The server cannot execute anything itself once its seccomp filter is
/// up, and gains no secrets from the rest: the executable and libraries
/// are already mapped into it, and trust stores hold public certificates.
fn server_child_paths() -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let mut executable: Vec<PathBuf> = std::env::current_exe().into_iter().collect();
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        for path in mapped_files(&maps) {
            if !executable.contains(&path) {
                executable.push(path);
            }
        }
    }
    let read_only = tls_paths()
        .into_iter()
        .chain(existing(RO_LOADER_PATHS))
        .collect();
    (executable, read_only, vec![PathBuf::from(NULL_DEVICE)])
}

/// The files mapped into a process, from the contents of its
/// `/proc/<pid>/maps`, leaving out those deleted since they were mapped.
fn mapped_files(maps: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for line in maps.lines() {
        // The pathname is the last field and the only one with a `/` in
        // it; anonymous mappings have none, or a pseudo-path like `[heap]`.
        let Some(start) = line.find('/') else {
            continue;
        };
        let path = &line[start..];
        if path.ends_with(" (deleted)") {
            continue;
        }
        let path = PathBuf::from(path);
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
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
        SandboxProfile::ScriptHost | SandboxProfile::FeedParser | SandboxProfile::WebUi => {
            (Vec::new(), Vec::new())
        }
        SandboxProfile::FeedFetcher => (Vec::new(), tls_paths()),
    }
}

/// The Landlock ABI the rulesets are written against: v6 (Linux 6.12+)
/// adds the scopes in [`landlock_scopes`] to v5's filesystem rights. The
/// landlock crate's compatibility layer downgrades to whatever the
/// running kernel supports rather than failing outright, and reports the
/// downgrade as [`RulesetStatus::PartiallyEnforced`].
const LANDLOCK_ABI: ABI = ABI::V6;

/// What a profile is barred from reaching outside its own Landlock domain
/// (and the domains nested inside it, which its children run in).
///
/// * Every profile but the web UI is scoped for signals. None of them
///   signals anything but itself and its own children: the server stops
///   the feed fetcher and the script host, which it starts after its
///   Landlock rules are in place (see
///   [`crate::sandbox::restrict_filesystem`]), and the fetcher kills its
///   own forked workers. The web UI stops its `kiki serve` child with
///   `SIGTERM`, and that child was started outside the web UI's sandbox.
/// * Every profile is scoped for abstract Unix sockets, which no Kiki
///   process connects to once its sandbox is up. The server's one
///   abstract socket, the service manager's `$NOTIFY_SOCKET` when that is
///   one, is connected before (see [`crate::notify`]), and Landlock lets a
///   datagram socket keep sending to the peer it was already connected to.
fn landlock_scopes(profile: &SandboxProfile) -> BitFlags<Scope> {
    match profile {
        SandboxProfile::WebUi => Scope::AbstractUnixSocket.into(),
        SandboxProfile::Server { .. }
        | SandboxProfile::ScriptHost
        | SandboxProfile::FeedFetcher
        | SandboxProfile::FeedParser => Scope::AbstractUnixSocket | Scope::Signal,
    }
}

fn apply_landlock(config: &SandboxConfig) -> Result<()> {
    let all = AccessFs::from_all(LANDLOCK_ABI);
    let read_only = AccessFs::from_read(LANDLOCK_ABI);
    let scopes = landlock_scopes(&config.profile);

    let (rw_paths, ro_paths) = landlock_paths(&config.profile);
    let (exec_paths, child_ro_paths, child_rw_paths) = match config.profile {
        SandboxProfile::Server { .. } => server_child_paths(),
        _ => Default::default(),
    };

    let mut ruleset = Ruleset::default().handle_access(all)?.scope(scopes)?;
    if matches!(config.profile, SandboxProfile::FeedFetcher) {
        // Handling `BindTcp` with no rule allowing any port denies TCP
        // binds outright (Linux 6.7+, ABI v4) — redundant with seccomp's
        // `bind` denial, but independent of it. It degrades silently on
        // older kernels, as the filesystem rules do.
        ruleset = ruleset.handle_access(AccessNet::BindTcp)?;
    }
    if matches!(
        config.profile,
        SandboxProfile::WebUi | SandboxProfile::FeedParser
    ) {
        // Handling both TCP rights with no rule allowing any port denies
        // every TCP bind and connect (Linux 6.7+, ABI v4). The web UI's
        // listener was bound before the sandbox went up, and the API is
        // reached over a Unix socket, so it needs neither; the parser
        // needs no network at all, and seccomp denies it sockets anyway.
        ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?;
    }
    let ruleset = ruleset
        .create()?
        .add_rules(path_beneath_rules(&rw_paths, all))?
        .add_rules(path_beneath_rules(&ro_paths, read_only))?
        .add_rules(path_beneath_rules(
            &exec_paths,
            AccessFs::Execute | AccessFs::ReadFile,
        ))?
        .add_rules(path_beneath_rules(&child_ro_paths, read_only))?
        .add_rules(path_beneath_rules(
            &child_rw_paths,
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))?;

    let status = ruleset.restrict_self()?;
    let profile = config.profile_name();
    let effective_abi = match status.landlock {
        LandlockStatus::Available { effective_abi, .. } => Some(effective_abi),
        LandlockStatus::NotEnabled | LandlockStatus::NotImplemented => None,
    };
    match status.ruleset {
        RulesetStatus::FullyEnforced => {
            tracing::info!(
                profile,
                abi = ?effective_abi,
                rw_paths = ?rw_paths,
                ro_paths = ?ro_paths,
                child_paths = exec_paths.len() + child_ro_paths.len() + child_rw_paths.len(),
                scopes = ?scopes,
                "landlock: sandbox fully enforced"
            );
        }
        RulesetStatus::PartiallyEnforced => {
            tracing::warn!(
                profile,
                abi = ?effective_abi,
                wanted_abi = ?LANDLOCK_ABI,
                "landlock: sandbox only partially enforced \
                 (the kernel does not support every requested restriction)"
            );
        }
        RulesetStatus::NotEnforced => {
            tracing::warn!(
                profile,
                "landlock: sandbox NOT enforced \
                 (requires Linux >= 5.13 with CONFIG_SECURITY_LANDLOCK=y \
                 and the landlock LSM enabled at boot)"
            );
        }
    }
    // Scoping arrived well after the filesystem rules, so say plainly
    // when it is what the kernel lacks.
    if effective_abi.is_some_and(|abi| abi < ABI::V6) {
        tracing::warn!(
            profile,
            abi = ?effective_abi,
            scopes = ?scopes,
            "landlock: signal and abstract Unix socket scoping NOT enforced \
             (requires Linux >= 6.12)"
        );
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

/// Extra syscalls denied to the script host, and to the feed fetcher's
/// parser: everything that creates a socket or attaches one to a peer.
///
/// The host does its IPC over a socket pair it inherited at exec time (the
/// parser, over one it inherited at fork time), so it needs no way to make
/// a new one. `send`/`recv` on an already-open
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
        SandboxProfile::ScriptHost | SandboxProfile::FeedParser => {
            &[DENIED_SCRIPT_HOST, arch_specific_sockets]
        }
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
        SandboxProfile::ScriptHost | SandboxProfile::FeedParser => None,
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

    /// Once [`refuse_write_exec`] has run, memory can neither be mapped writable
    /// and executable nor made executable afterwards, but read-write and
    /// read-execute mappings still work. Runs in a forked child, since the
    /// setting cannot be undone.
    #[test]
    fn mdwe_refuses_writable_and_executable_memory() {
        // SAFETY: `PR_GET_MDWE` takes no pointers; see `apply_mdwe`.
        let supported = unsafe { libc::prctl(libc::PR_GET_MDWE, 0, 0, 0, 0) } >= 0;
        if !supported {
            eprintln!("skipping: the kernel lacks PR_SET_MDWE (Linux < 6.3)");
            return;
        }

        // Exit codes from the child, one per check that failed.
        const SET_FAILED: i32 = 10;
        const RWX_MAPPED: i32 = 11;
        const RW_REFUSED: i32 = 12;
        const EXEC_GAINED: i32 = 13;
        const RX_REFUSED: i32 = 14;

        // SAFETY: the child only makes raw syscalls and calls `_exit`, so
        // it takes no lock another thread of the test harness could hold.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", std::io::Error::last_os_error());
        if pid == 0 {
            let code = (|| {
                if refuse_write_exec().ok() != Some(Mdwe::Refused) {
                    return SET_FAILED;
                }
                let map = |prot| {
                    // SAFETY: an anonymous private mapping touches no
                    // existing memory.
                    unsafe {
                        libc::mmap(
                            std::ptr::null_mut(),
                            4096,
                            prot,
                            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                            -1,
                            0,
                        )
                    }
                };
                let rw = libc::PROT_READ | libc::PROT_WRITE;
                let rx = libc::PROT_READ | libc::PROT_EXEC;
                if map(rw | libc::PROT_EXEC) != libc::MAP_FAILED {
                    return RWX_MAPPED;
                }
                let page = map(rw);
                if page == libc::MAP_FAILED {
                    return RW_REFUSED;
                }
                // SAFETY: `page` is the page just mapped, and nothing
                // reads or writes it.
                if unsafe { libc::mprotect(page, 4096, rx) } == 0 {
                    return EXEC_GAINED;
                }
                if map(rx) == libc::MAP_FAILED {
                    return RX_REFUSED;
                }
                0
            })();
            // SAFETY: `_exit` skips the harness's atexit handlers and
            // destructors, which belong to the parent.
            unsafe { libc::_exit(code) };
        }

        let mut status = 0;
        // SAFETY: `pid` is the child forked above, and `status` is a valid
        // pointer for the call.
        let waited = unsafe { libc::waitpid(pid, &mut status, 0) };
        assert_eq!(waited, pid, "waitpid: {}", std::io::Error::last_os_error());
        assert!(libc::WIFEXITED(status), "child did not exit: {status:#x}");
        assert_eq!(libc::WEXITSTATUS(status), 0, "child failed a check");
    }

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

    /// The pathname is everything from the first `/`, spaces included;
    /// anonymous and pseudo-path mappings, deleted files and repeats are
    /// left out.
    #[test]
    fn mapped_files_are_read_from_proc_maps() {
        let maps = "\
55d0c0a00000-55d0c0a01000 r--p 00000000 fd:01 1048602                    /nix/store/abc-kiki/bin/kiki
55d0c0a01000-55d0c0a02000 r-xp 00001000 fd:01 1048602                    /nix/store/abc-kiki/bin/kiki
55d0c2a2c000-55d0c2a4d000 rw-p 00000000 00:00 0                          [heap]
7f2b1c000000-7f2b1c021000 rw-p 00000000 00:00 0 
7f2b1d000000-7f2b1d028000 r--p 00000000 fd:01 1050000                    /usr/lib/x86_64-linux-gnu/libc.so.6
7f2b1e000000-7f2b1e001000 r--p 00000000 fd:01 1050001                    /opt/a dir/lib.so
7f2b1f000000-7f2b1f001000 rw-s 00000000 00:01 1050002                    /memfd:scratch (deleted)
7ffd4a5e1000-7ffd4a602000 rw-p 00000000 00:00 0                          [stack]
";
        assert_eq!(
            mapped_files(maps),
            vec![
                PathBuf::from("/nix/store/abc-kiki/bin/kiki"),
                PathBuf::from("/usr/lib/x86_64-linux-gnu/libc.so.6"),
                PathBuf::from("/opt/a dir/lib.so"),
            ]
        );
    }

    /// The server's children start under its Landlock rules, so it is
    /// granted what they need to: run this executable, point their streams
    /// at `/dev/null`, and, for the feed fetcher, read the TLS trust stores.
    #[test]
    fn the_server_is_granted_what_its_children_need() {
        let (exec, ro, rw) = server_child_paths();
        let exe = std::env::current_exe().unwrap();
        assert!(exec.contains(&exe), "{exe:?} missing from {exec:?}");
        for p in tls_paths() {
            assert!(ro.contains(&p), "trust store {p:?} missing from {ro:?}");
        }
        assert_eq!(rw, vec![PathBuf::from(NULL_DEVICE)]);
    }

    /// Every HTTP(S) request is made by the feed fetcher, so the server's
    /// own rules include no TLS trust stores; it is granted them only
    /// for the fetcher's sake (see the test above).
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

    /// Every profile is kept from abstract Unix sockets outside its
    /// sandbox, and every profile but the web UI, which stops a `kiki
    /// serve` started outside its own, from signalling outside it.
    #[test]
    fn every_profile_but_the_web_ui_is_scoped_for_signals() {
        let server = SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
        };
        for profile in [
            server,
            SandboxProfile::ScriptHost,
            SandboxProfile::FeedFetcher,
            SandboxProfile::WebUi,
        ] {
            let scopes = landlock_scopes(&profile);
            assert!(scopes.contains(Scope::AbstractUnixSocket));
            assert_eq!(
                scopes.contains(Scope::Signal),
                !matches!(profile, SandboxProfile::WebUi)
            );
        }
    }
}
