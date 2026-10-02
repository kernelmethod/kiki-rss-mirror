//! Linux-specific sandbox implementation (Landlock, seccomp-bpf, and
//! `PR_SET_MDWE`).

use super::{SandboxConfig, SandboxProfile};
use anyhow::{Context, Result};
use landlock::{
    path_beneath_rules, Access, AccessFs, AccessNet, BitFlags, LandlockStatus, NetPort, Ruleset,
    RulesetAttr, RulesetCreatedAttr, RulesetStatus, Scope, ABI,
};
use std::path::{Path, PathBuf};

/// Read-only paths needed to resolve hostnames: the C library resolver's
/// configuration files.
///
/// Granted to the feed fetcher's resolver, which resolves every hostname
/// Kiki looks up (see [`crate::process::feed_fetcher`]). The fetcher's
/// supervisor, which starts the resolver under its own Landlock domain, is
/// granted them too, and so is the server, which starts the fetcher under
/// its: see [`server_child_paths`].
const RO_DNS_PATHS: &[&str] = &[
    "/etc/resolv.conf",
    "/etc/nsswitch.conf",
    "/etc/hosts",
    "/etc/host.conf",
    "/etc/gai.conf",
    "/etc/services",
    "/etc/protocols",
];

/// Read-only paths needed to handle local time. Granted to the server
/// only; the feed fetcher keeps time in UTC.
const RO_TIME_ZONE_PATHS: &[&str] = &[
    // chrono reads /etc/localtime, some crates read zoneinfo
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
fn dns_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = existing(RO_DNS_PATHS).collect();
    paths.extend(symlink_targets(&paths));
    paths
}

fn time_zone_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = existing(RO_TIME_ZONE_PATHS).collect();
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
/// The setting cannot be undone, and is inherited by forked children and
/// kept across `execve` (so the feed fetcher's worker, parser and resolver
/// start out with it). Like Landlock, it has
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
/// * verify TLS certificates and resolve hostnames, in the feed fetcher's
///   case.
///
/// The server cannot execute anything itself once its seccomp filter is
/// up, and gains no secrets from the rest: the executable and libraries
/// are already mapped into it, trust stores hold public certificates, and
/// the resolver's configuration is readable by every user.
fn server_child_paths() -> (Vec<PathBuf>, Vec<PathBuf>, Vec<PathBuf>) {
    let (executable, loader) = self_exec_paths();
    let read_only = tls_paths()
        .into_iter()
        .chain(dns_paths())
        .chain(loader)
        .collect();
    (executable, read_only, vec![PathBuf::from(NULL_DEVICE)])
}

/// What a process needs to run the kiki executable again, as
/// `(executable, read_only)`: the executable itself and every file this
/// process has mapped — the dynamic loader and the shared libraries,
/// which the new process loads again — to execute and read, and the
/// loader's cache and preload list to read.
fn self_exec_paths() -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut executable: Vec<PathBuf> = std::env::current_exe().into_iter().collect();
    if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
        for path in mapped_files(&maps) {
            if !executable.contains(&path) {
                executable.push(path);
            }
        }
    }
    (executable, existing(RO_LOADER_PATHS).collect())
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
            // No TLS trust stores or resolver configuration: the server
            // makes no HTTP(S) requests and resolves no hostnames of its
            // own, as the feed fetcher does both.
            let ro_paths: Vec<PathBuf> = time_zone_paths()
                .into_iter()
                .chain(existing(RO_ENTROPY_PATHS))
                .chain(existing(RO_INTROSPECTION_PATHS))
                .collect();
            (rw_paths, ro_paths)
        }
        SandboxProfile::ScriptHost | SandboxProfile::FeedParser | SandboxProfile::WebUi => {
            (Vec::new(), Vec::new())
        }
        // The supervisor starts the resolver, whose domain nests inside its
        // own, so it holds the resolver's paths for it, and the worker's
        // too: the configuration is readable by every user. What it needs
        // to start them is added in `apply_landlock`.
        SandboxProfile::FeedWorker => (Vec::new(), tls_paths()),
        SandboxProfile::FeedFetcher => (
            Vec::new(),
            tls_paths().into_iter().chain(dns_paths()).collect(),
        ),
        SandboxProfile::FeedResolver => (Vec::new(), dns_paths()),
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
///   own workers. The web UI stops its `kiki serve` child with
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
        | SandboxProfile::FeedWorker
        | SandboxProfile::FeedParser
        | SandboxProfile::FeedResolver => Scope::AbstractUnixSocket | Scope::Signal,
    }
}

fn apply_landlock(config: &SandboxConfig) -> Result<()> {
    let all = AccessFs::from_all(LANDLOCK_ABI);
    let read_only = AccessFs::from_read(LANDLOCK_ABI);
    let scopes = landlock_scopes(&config.profile);

    let (rw_paths, ro_paths) = landlock_paths(&config.profile);
    let (exec_paths, child_ro_paths, child_rw_paths) = match config.profile {
        SandboxProfile::Server { .. } => server_child_paths(),
        // The supervisor starts its worker, parser and resolver by running
        // the kiki executable again. They inherit its standard streams, so
        // need no `/dev/null`.
        SandboxProfile::FeedFetcher => {
            let (executable, loader) = self_exec_paths();
            (executable, loader, Vec::new())
        }
        _ => Default::default(),
    };

    let mut ruleset = Ruleset::default().handle_access(all)?.scope(scopes)?;
    if matches!(
        config.profile,
        SandboxProfile::FeedFetcher | SandboxProfile::FeedWorker
    ) {
        // Handling `BindTcp` with no rule allowing any port denies TCP
        // binds outright (Linux 6.7+, ABI v4) — for the worker, redundant
        // with seccomp's `bind` denial, but independent of it. It degrades
        // silently on older kernels, as the filesystem rules do.
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
    if matches!(config.profile, SandboxProfile::FeedResolver) {
        // TCP connections only to port 53, for DNS replies too large for
        // UDP, and no binds (Linux 6.7+, ABI v4). Landlock has no say over
        // UDP, which ordinary queries use.
        ruleset = ruleset.handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?;
    }
    let mut ruleset = ruleset.create()?;
    if matches!(config.profile, SandboxProfile::FeedResolver) {
        ruleset = ruleset.add_rule(NetPort::new(DNS_PORT, AccessNet::ConnectTcp))?;
    }
    let ruleset = ruleset
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

/// The port DNS is served on, over UDP and TCP alike.
const DNS_PORT: u16 = 53;

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

/// Syscalls every profile may make: what the standard library, the C
/// library and the allocator need to run threads, manage memory, handle
/// signals, tell the time, and use the descriptors the process already
/// holds — among them the socket pair each child talks to its parent over.
///
/// Opening and inspecting files is here too. Landlock, not seccomp,
/// decides which files a profile can reach, and for the profiles granted
/// none an `open` fails with `EACCES` rather than killing the process.
///
/// Anything a profile's lists leave out kills the process, so these err
/// towards what the C library *might* call — `ppoll` as well as `poll`,
/// `prlimit64` behind `getrlimit`, the time syscalls behind the vDSO — over
/// only what a trace of the test suite happened to see.
const ALLOWED_COMMON: &[i64] = &[
    // Memory.
    libc::SYS_brk,
    libc::SYS_mmap,
    libc::SYS_munmap,
    libc::SYS_mremap,
    libc::SYS_mprotect,
    libc::SYS_madvise,
    // Threads. `clone` is allowed only without namespace flags, and
    // `clone3` is refused with `ENOSYS` so that the C library falls back
    // to `clone`: see [`apply_seccomp`].
    libc::SYS_clone,
    libc::SYS_clone3,
    libc::SYS_futex,
    libc::SYS_set_robust_list,
    libc::SYS_set_tid_address,
    libc::SYS_rseq,
    libc::SYS_sched_yield,
    libc::SYS_sched_getaffinity,
    libc::SYS_exit,
    libc::SYS_exit_group,
    // Who the process is.
    libc::SYS_getpid,
    libc::SYS_getppid,
    libc::SYS_gettid,
    libc::SYS_getuid,
    libc::SYS_geteuid,
    libc::SYS_getgid,
    libc::SYS_getegid,
    libc::SYS_uname,
    libc::SYS_prctl,
    libc::SYS_prlimit64,
    // Signals. `tgkill` is how `abort` raises `SIGABRT`, so a panic that
    // aborts still reports as one.
    libc::SYS_rt_sigaction,
    libc::SYS_rt_sigprocmask,
    libc::SYS_rt_sigreturn,
    libc::SYS_sigaltstack,
    libc::SYS_tgkill,
    // How the kernel resumes a sleep or wait a signal interrupted — a
    // `clock_nanosleep`, say — once the handler returns. Seccomp sees it
    // like any other syscall.
    libc::SYS_restart_syscall,
    // Time.
    libc::SYS_clock_gettime,
    libc::SYS_clock_getres,
    libc::SYS_gettimeofday,
    libc::SYS_nanosleep,
    libc::SYS_clock_nanosleep,
    // Descriptors already open. `ioctl` is narrowed to a few requests:
    // see [`ALLOWED_IOCTLS`].
    libc::SYS_read,
    libc::SYS_readv,
    libc::SYS_write,
    libc::SYS_writev,
    libc::SYS_lseek,
    libc::SYS_close,
    libc::SYS_fcntl,
    libc::SYS_ioctl,
    libc::SYS_ppoll,
    libc::SYS_recvfrom,
    libc::SYS_recvmsg,
    libc::SYS_sendto,
    libc::SYS_sendmsg,
    libc::SYS_shutdown,
    // Files, as far as Landlock lets.
    libc::SYS_openat,
    libc::SYS_fstat,
    libc::SYS_newfstatat,
    libc::SYS_statx,
    libc::SYS_readlinkat,
    libc::SYS_faccessat,
    libc::SYS_faccessat2,
    libc::SYS_getdents64,
    libc::SYS_getcwd,
    libc::SYS_getrandom,
];

/// The legacy forms of [`ALLOWED_COMMON`]'s syscalls that x86_64 still
/// has, and its C library still calls. Newer architectures have only the
/// `*at` and `p*` forms.
#[cfg(target_arch = "x86_64")]
const ALLOWED_COMMON_X86_64: &[i64] = &[
    libc::SYS_open,
    libc::SYS_stat,
    libc::SYS_lstat,
    libc::SYS_access,
    libc::SYS_readlink,
    libc::SYS_poll,
    libc::SYS_time,
    libc::SYS_getrlimit,
];

/// Syscalls for an event loop, which the profiles that run a tokio runtime
/// need: the server, the feed fetcher's worker (and so its supervisor, which
/// starts it), and the web UI.
const ALLOWED_EVENT_LOOP: &[i64] = &[
    libc::SYS_epoll_create1,
    libc::SYS_epoll_ctl,
    libc::SYS_epoll_pwait,
    libc::SYS_epoll_pwait2,
    libc::SYS_eventfd2,
    libc::SYS_pipe2,
    // tokio's signal handling wakes the runtime over a socket pair it
    // makes when the runtime starts.
    libc::SYS_socketpair,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_epoll_wait,
];

/// Syscalls for making sockets of the profile's own and connecting them.
/// Which address families it may make them in is narrowed separately: see
/// [`socket_domains`].
///
/// Not granted to the server, which binds its API socket before its filter
/// goes up, and the service manager's notification socket is connected
/// before that: from then on it only accepts connections.
const ALLOWED_SOCKETS: &[i64] = &[
    libc::SYS_socket,
    libc::SYS_connect,
    libc::SYS_getsockname,
    libc::SYS_getpeername,
    libc::SYS_getsockopt,
    libc::SYS_setsockopt,
    libc::SYS_sendmmsg,
    libc::SYS_recvmmsg,
];

/// Binding a socket to an address. musl's resolver binds every UDP socket
/// it sends a query from (to port 0), so the feed fetcher's resolver may
/// bind — and so may its supervisor, as a process it starts can do
/// nothing the supervisor may not. Landlock denies the two of them TCP
/// binds where the kernel supports it (6.7+).
///
/// No profile may `listen`: the server's API socket and the web UI's
/// listener are both listening before their filters go up.
const ALLOWED_BIND: &[i64] = &[libc::SYS_bind];

/// Accepting connections on a listening socket, for the server's API
/// socket and the web UI's listener, both bound and listening before the
/// sandbox goes up. `accept` is a distinct syscall only on
/// the architectures that predate `accept4`.
const ALLOWED_ACCEPT: &[i64] = &[
    libc::SYS_accept4,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_accept,
];

/// Changing the filesystem, which only the server does: SQLite's database,
/// journals and temp files, the asset cache, the API socket's file, and
/// watching the configuration for changes.
const ALLOWED_FILE_WRITES: &[i64] = &[
    libc::SYS_pread64,
    libc::SYS_pwrite64,
    libc::SYS_preadv,
    libc::SYS_pwritev,
    libc::SYS_ftruncate,
    libc::SYS_fallocate,
    libc::SYS_fsync,
    libc::SYS_fdatasync,
    libc::SYS_flock,
    libc::SYS_mkdirat,
    libc::SYS_unlinkat,
    libc::SYS_renameat2,
    libc::SYS_linkat,
    libc::SYS_symlinkat,
    libc::SYS_fchmod,
    libc::SYS_fchmodat,
    libc::SYS_fchown,
    libc::SYS_fchownat,
    libc::SYS_utimensat,
    libc::SYS_copy_file_range,
    libc::SYS_sendfile,
    libc::SYS_inotify_init1,
    libc::SYS_inotify_add_watch,
    libc::SYS_inotify_rm_watch,
    // riscv64 has only `renameat2`.
    #[cfg(not(target_arch = "riscv64"))]
    libc::SYS_renameat,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_mkdir,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_rmdir,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_unlink,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_rename,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_link,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_symlink,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chmod,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_chown,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_inotify_init,
];

/// Waiting for and stopping child processes: the server's feed fetcher and
/// script host, the feed fetcher's worker, parser and resolver, and the web
/// UI's `kiki serve`. Landlock keeps every profile but the web UI from
/// signalling outside its own sandbox, where the kernel supports it.
const ALLOWED_CHILDREN: &[i64] = &[libc::SYS_wait4, libc::SYS_waitid, libc::SYS_kill];

/// What the feed fetcher's supervisor needs to install its children's
/// sandboxes, which they do under its filter, before their own goes up.
const ALLOWED_SANDBOXING: &[i64] = &[
    libc::SYS_seccomp,
    libc::SYS_landlock_create_ruleset,
    libc::SYS_landlock_add_rule,
    libc::SYS_landlock_restrict_self,
];

/// What the feed fetcher's supervisor needs to start its children by
/// running the kiki executable again: the `execve` itself, the `dup2` that
/// puts a child's socket on [`crate::process::CHILD_FD`] before it, and
/// what the new process calls, still under the supervisor's filter, from
/// when the dynamic loader loads it to when it installs its own filter:
/// `pread64` to read the libraries' headers, the thread pointer set up with
/// `arch_prctl` on x86_64, and the `umask` that `main` sets.
///
/// Landlock limits what the supervisor can execute to the kiki executable
/// and what it loads (see `self_exec_paths`), and every other profile's
/// allowlist leaves `execve` out, so the processes the supervisor starts
/// can execute nothing at all.
const ALLOWED_EXEC: &[i64] = &[
    libc::SYS_execve,
    libc::SYS_dup3,
    libc::SYS_pread64,
    libc::SYS_umask,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_dup2,
    #[cfg(target_arch = "x86_64")]
    libc::SYS_arch_prctl,
];

/// The `ioctl` requests every profile may make: making a descriptor
/// (non-)blocking or close-on-exec, asking how many bytes are waiting to be
/// read (as glibc's resolver does), and asking whether a descriptor is a
/// terminal, and how big (as logging does, to decide on colours; newer
/// glibc asks with `TCGETS2`).
///
/// Every other request fails with `ENOTTY`, as it would on a descriptor
/// that does not support it — among them `TIOCSTI`, which would let a
/// compromised process type commands into a terminal it inherited.
const ALLOWED_IOCTLS: &[libc::Ioctl] = &[
    libc::FIONBIO,
    libc::FIOCLEX,
    libc::FIONCLEX,
    libc::FIONREAD,
    libc::TCGETS,
    libc::TCGETS2,
    libc::TIOCGWINSZ,
];

/// The `clone` flags that create namespaces. A process that could create
/// a user namespace would gain capabilities inside it, opening up kernel
/// code that unprivileged processes cannot otherwise reach, and no Kiki
/// process creates any. `CLONE_NEWTIME` is left out: in `clone`, as
/// opposed to `clone3`, its bit is part of the exit signal.
const CLONE_NAMESPACE_FLAGS: u64 = (libc::CLONE_NEWNS
    | libc::CLONE_NEWCGROUP
    | libc::CLONE_NEWUTS
    | libc::CLONE_NEWIPC
    | libc::CLONE_NEWUSER
    | libc::CLONE_NEWPID
    | libc::CLONE_NEWNET) as u64;

/// The syscall lists a profile's allowlist is made of: everything else
/// kills the process.
fn allowed_syscalls(profile: &SandboxProfile) -> Vec<i64> {
    #[cfg(target_arch = "x86_64")]
    let common: &[&[i64]] = &[ALLOWED_COMMON, ALLOWED_COMMON_X86_64];
    #[cfg(not(target_arch = "x86_64"))]
    let common: &[&[i64]] = &[ALLOWED_COMMON];

    let extra: &[&[i64]] = match profile {
        SandboxProfile::Server { .. } => &[
            ALLOWED_EVENT_LOOP,
            ALLOWED_ACCEPT,
            ALLOWED_FILE_WRITES,
            ALLOWED_CHILDREN,
        ],
        // Nothing beyond the inherited socket pair.
        SandboxProfile::ScriptHost | SandboxProfile::FeedParser => &[],
        // What its worker, parser and resolver need between them, and
        // what it needs to start and sandbox them.
        SandboxProfile::FeedFetcher => &[
            ALLOWED_EVENT_LOOP,
            ALLOWED_SOCKETS,
            ALLOWED_BIND,
            ALLOWED_CHILDREN,
            ALLOWED_SANDBOXING,
            ALLOWED_EXEC,
        ],
        SandboxProfile::FeedWorker => &[ALLOWED_EVENT_LOOP, ALLOWED_SOCKETS],
        SandboxProfile::FeedResolver => &[ALLOWED_SOCKETS, ALLOWED_BIND],
        SandboxProfile::WebUi => &[
            ALLOWED_EVENT_LOOP,
            ALLOWED_SOCKETS,
            ALLOWED_ACCEPT,
            ALLOWED_CHILDREN,
        ],
    };

    let mut allowed: Vec<i64> = common
        .iter()
        .chain(extra)
        .flat_map(|s| s.iter())
        .copied()
        .collect();
    allowed.sort_unstable();
    allowed.dedup();
    allowed
}

/// The only address families the feed fetcher's processes may create
/// sockets in: IPv4 and IPv6, for HTTP(S) requests, and for DNS queries
/// sent straight to a name server.
///
/// Not Unix: a fetcher process that could create one could reach the
/// server's API socket, whose only access control is reachability. The
/// supervisor makes its children's channels with `socketpair`, which this
/// does not touch. So lookups that go through a local daemon — nscd,
/// sssd, or systemd-resolved's NSS module — fail, and the C library falls
/// back to the name servers in `resolv.conf` (systemd-resolved's stub
/// among them, if listed there) and to `/etc/hosts`. Nor netlink: glibc's
/// `getaddrinfo` opens one to learn which address families the host has
/// configured, and when that fails it assumes both, and resolution carries
/// on.
const FETCHER_SOCKET_FAMILIES: &[i32] = &[libc::AF_INET, libc::AF_INET6];

/// The only address family the web UI may create sockets in: Unix, to
/// reach the API. It talks to browsers over a listener it bound before the
/// sandbox went up, so a new Internet socket could only be a compromised
/// web UI reaching out.
const WEB_UI_SOCKET_FAMILIES: &[i32] = &[libc::AF_UNIX];

/// Which `socket(2)` address families a profile may use, for the profiles
/// [`ALLOWED_SOCKETS`] is granted to.
fn socket_domains(profile: &SandboxProfile) -> Option<&'static [i32]> {
    match profile {
        SandboxProfile::FeedFetcher | SandboxProfile::FeedWorker | SandboxProfile::FeedResolver => {
            Some(FETCHER_SOCKET_FAMILIES)
        }
        SandboxProfile::WebUi => Some(WEB_UI_SOCKET_FAMILIES),
        // `socket` is not on their allowlist at all.
        SandboxProfile::Server { .. } | SandboxProfile::ScriptHost | SandboxProfile::FeedParser => {
            None
        }
    }
}

/// Install the profile's seccomp filters: an allowlist that kills the
/// process on any syscall it leaves out, and, ahead of it, filters that
/// refuse some of the calls it allows with an error, according to their
/// arguments.
///
/// The kernel runs every installed filter and takes the most severe
/// verdict, so an error from one of the latter wins over the allowlist's
/// `Allow`. They refuse with an error rather than killing because the
/// call has a fallback, or because a library may reasonably probe for it:
///
/// * `clone3` fails with `ENOSYS`, as on a kernel that predates it, and
///   the C library falls back to `clone` — whose flags, unlike `clone3`'s,
///   are not behind a pointer, so the allowlist can refuse the ones that
///   create namespaces;
/// * `ioctl` fails with `ENOTTY` for any request not in
///   [`ALLOWED_IOCTLS`];
/// * `socket` fails with `EACCES` for any address family the profile may
///   not use (see [`socket_domains`]); a library that probes for a local
///   service, as glibc's resolver does for nscd, sees it as absent rather
///   than bringing the process down.
///
/// The allowlist goes on last, so that it need not allow `seccomp` itself
/// — except for the feed fetcher's supervisor, whose children install
/// filters of their own.
fn apply_seccomp(config: &SandboxConfig) -> Result<()> {
    use seccompiler::{
        apply_filter_all_threads, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp,
        SeccompCondition, SeccompFilter, SeccompRule,
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

    let condition = |arg: u8, len: SeccompCmpArgLen, op: SeccompCmpOp, value: u64| {
        SeccompCondition::new(arg, len, op, value).context("building a seccomp condition")
    };
    let rule = |conditions| SeccompRule::new(conditions).context("building a seccomp rule");

    // Compile everything before installing anything, so that a filter that
    // fails to build leaves the process as it was.
    let mut programs: Vec<(&str, BpfProgram)> = Vec::new();
    let mut compile = |name: &'static str,
                       rules: BTreeMap<i64, Vec<SeccompRule>>,
                       match_action: SeccompAction,
                       mismatch_action: SeccompAction|
     -> Result<()> {
        let filter = SeccompFilter::new(rules, mismatch_action, match_action, arch)
            .with_context(|| format!("constructing the {name} seccomp filter"))?;
        let program: BpfProgram = filter
            .try_into()
            .with_context(|| format!("compiling the {name} seccomp filter"))?;
        programs.push((name, program));
        Ok(())
    };
    // In log-only mode every refusal is logged instead.
    let refuse = |errno: i32| {
        if config.log_only {
            SeccompAction::Log
        } else {
            SeccompAction::Errno(errno as u32)
        }
    };

    // `clone3`, refused outright — in log-only mode too, as the C library
    // tries it for every thread it starts, and needs nothing in return.
    compile(
        "clone3",
        [(libc::SYS_clone3, Vec::new())].into_iter().collect(),
        SeccompAction::Errno(libc::ENOSYS as u32),
        SeccompAction::Allow,
    )?;

    // `ioctl`, refused unless its request is one of the allowed ones: a
    // rule matches when all of its conditions do.
    let ioctl_conditions = ALLOWED_IOCTLS
        .iter()
        .map(|&request| {
            condition(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                request as u32 as u64,
            )
        })
        .collect::<Result<_>>()?;
    compile(
        "ioctl",
        [(libc::SYS_ioctl, vec![rule(ioctl_conditions)?])]
            .into_iter()
            .collect(),
        refuse(libc::ENOTTY),
        SeccompAction::Allow,
    )?;

    // `socket`, refused unless its domain is one of the allowed ones.
    if let Some(families) = socket_domains(&config.profile) {
        let socket_conditions = families
            .iter()
            .map(|&f| condition(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, f as u64))
            .collect::<Result<_>>()?;
        compile(
            "socket domain",
            [(libc::SYS_socket, vec![rule(socket_conditions)?])]
                .into_iter()
                .collect(),
            refuse(libc::EACCES),
            SeccompAction::Allow,
        )?;
    }

    // The allowlist itself, with `clone` allowed only without namespace
    // flags.
    let allowed = allowed_syscalls(&config.profile);
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> =
        allowed.iter().map(|&nr| (nr, Vec::new())).collect();
    rules.insert(
        libc::SYS_clone,
        vec![rule(vec![condition(
            0,
            SeccompCmpArgLen::Qword,
            SeccompCmpOp::MaskedEq(CLONE_NAMESPACE_FLAGS),
            0,
        )?])?],
    );
    let violation = if config.log_only {
        SeccompAction::Log
    } else {
        SeccompAction::KillProcess
    };
    compile("allowlist", rules, SeccompAction::Allow, violation)?;

    for (name, program) in &programs {
        apply_filter_all_threads(program)
            .with_context(|| format!("installing the {name} seccomp filter"))?;
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
            allowed_syscalls = allowed.len(),
            "seccomp: syscall filter installed (allowlist, kill-on-violation)"
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

    fn every_profile() -> Vec<SandboxProfile> {
        vec![
            SandboxProfile::Server {
                data_dir: PathBuf::from("/var/lib/kiki"),
                socket_dir: PathBuf::from("/var/lib/kiki"),
                temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
            },
            SandboxProfile::ScriptHost,
            SandboxProfile::FeedFetcher,
            SandboxProfile::FeedWorker,
            SandboxProfile::FeedParser,
            SandboxProfile::FeedResolver,
            SandboxProfile::WebUi,
        ]
    }

    /// The script host and the feed fetcher's parser may make no socket of
    /// their own, nor attach the ones they have to a peer; and every
    /// profile that may make sockets has its address families narrowed.
    #[test]
    fn the_script_host_and_the_parser_may_not_make_sockets() {
        for profile in every_profile() {
            let allowed = allowed_syscalls(&profile);
            if matches!(
                profile,
                SandboxProfile::ScriptHost | SandboxProfile::FeedParser
            ) {
                for nr in [
                    libc::SYS_socket,
                    libc::SYS_socketpair,
                    libc::SYS_connect,
                    libc::SYS_bind,
                    libc::SYS_listen,
                    libc::SYS_accept4,
                ] {
                    assert!(!allowed.contains(&nr), "{nr} allowed");
                }
            }
            assert_eq!(
                allowed.contains(&libc::SYS_socket),
                socket_domains(&profile).is_some()
            );
        }
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
        let dns = dns_paths();
        for p in &ro {
            assert!(
                RO_TLS_PATHS
                    .iter()
                    .chain(RO_ENTROPY_PATHS)
                    .any(|allowed| Path::new(allowed) == p)
                    || env_paths.contains(p)
                    || dns.contains(p),
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
    fn the_feed_worker_may_connect_but_not_bind_or_accept() {
        let allowed = allowed_syscalls(&SandboxProfile::FeedWorker);
        assert!(!allowed.contains(&libc::SYS_bind));
        assert!(!allowed.contains(&libc::SYS_listen));
        assert!(!allowed.contains(&libc::SYS_accept4));
        assert!(allowed.contains(&libc::SYS_socket));
        assert!(allowed.contains(&libc::SYS_connect));
    }

    /// musl's resolver binds each UDP socket it queries from, and a
    /// process the supervisor starts can do nothing the supervisor
    /// may not, so neither the supervisor nor the resolver may be denied
    /// `bind`; only the worker, which installs its own filter, is.
    #[test]
    fn the_supervisor_and_the_resolver_may_bind_for_musls_resolver() {
        for profile in [SandboxProfile::FeedFetcher, SandboxProfile::FeedResolver] {
            let allowed = allowed_syscalls(&profile);
            assert!(allowed.contains(&libc::SYS_bind));
            assert!(!allowed.contains(&libc::SYS_listen));
            assert!(!allowed.contains(&libc::SYS_accept4));
        }
    }

    /// The supervisor's children run under its filter as well as their
    /// own, so a syscall one of them needs that the supervisor's
    /// allowlist leaves out would kill it. Each child's allowlist must
    /// therefore lie within the supervisor's.
    #[test]
    fn the_supervisor_allows_whatever_its_children_do() {
        let supervisor = allowed_syscalls(&SandboxProfile::FeedFetcher);
        for child in [
            SandboxProfile::FeedWorker,
            SandboxProfile::FeedParser,
            SandboxProfile::FeedResolver,
        ] {
            for nr in allowed_syscalls(&child) {
                assert!(supervisor.contains(&nr), "syscall {nr} missing");
            }
            if let Some(families) = socket_domains(&child) {
                assert!(families.iter().all(|f| FETCHER_SOCKET_FAMILIES.contains(f)));
            }
        }
    }

    /// Only the supervisor installs filters once its own is up — its
    /// children's, started under it. Every other profile installs its
    /// allowlist last, so it need not allow `seccomp` or Landlock.
    #[test]
    fn only_the_supervisor_may_install_sandboxes() {
        for profile in every_profile() {
            let allowed = allowed_syscalls(&profile);
            let supervisor = matches!(profile, SandboxProfile::FeedFetcher);
            for nr in ALLOWED_SANDBOXING {
                assert_eq!(allowed.contains(nr), supervisor, "syscall {nr}");
            }
        }
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
        let allowed = allowed_syscalls(&SandboxProfile::WebUi);
        assert!(!allowed.contains(&libc::SYS_bind));
        assert!(!allowed.contains(&libc::SYS_listen));
        assert!(allowed.contains(&libc::SYS_accept4));
        assert!(allowed.contains(&libc::SYS_socket));
        assert!(allowed.contains(&libc::SYS_connect));
        assert_eq!(WEB_UI_SOCKET_FAMILIES, [libc::AF_UNIX]);
    }

    /// The server's API socket is bound and listening before its filter
    /// goes up, so all it may do with sockets is accept connections.
    #[test]
    fn the_server_may_accept_but_not_make_sockets() {
        let allowed = allowed_syscalls(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
        });
        assert!(allowed.contains(&libc::SYS_accept4));
        for nr in [
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
        ] {
            assert!(!allowed.contains(&nr), "syscall {nr} allowed");
        }
    }

    /// No profile may listen: the server and the web UI listen before
    /// their filters go up.
    #[test]
    fn no_profile_may_listen() {
        for profile in every_profile() {
            assert!(!allowed_syscalls(&profile).contains(&libc::SYS_listen));
        }
    }

    #[test]
    fn the_feed_fetcher_may_only_create_internet_sockets() {
        assert_eq!(FETCHER_SOCKET_FAMILIES, [libc::AF_INET, libc::AF_INET6]);
    }

    #[test]
    fn the_resolver_gets_only_read_only_dns_paths() {
        let (rw, ro) = landlock_paths(&SandboxProfile::FeedResolver);
        assert!(rw.is_empty());
        for p in &ro {
            assert!(
                RO_DNS_PATHS.iter().any(|d| p.starts_with(d))
                    || symlink_targets(&dns_paths()).contains(p),
                "unexpected read-only path for the resolver: {p:?}"
            );
        }
    }

    #[test]
    fn the_server_gets_no_resolver_configuration_of_its_own() {
        let (_, ro) = landlock_paths(&SandboxProfile::Server {
            data_dir: PathBuf::from("/var/lib/kiki"),
            socket_dir: PathBuf::from("/var/lib/kiki"),
            temp_dir: PathBuf::from("/var/lib/kiki/tmp"),
        });
        for dns in RO_DNS_PATHS {
            assert!(
                !ro.contains(&PathBuf::from(dns)),
                "{dns} granted to the server"
            );
        }
    }

    /// What the denylist this allowlist replaced used to deny: the most
    /// dangerous post-exploitation primitives, which no profile may ever
    /// be allowed, however its lists grow.
    const NEVER_ALLOWED: &[i64] = &[
        libc::SYS_execve,
        libc::SYS_execveat,
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
        // Not in libc for every target.
        #[cfg(target_arch = "x86_64")]
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
        #[cfg(target_arch = "x86_64")]
        libc::SYS_uselib,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_userfaultfd,
        libc::SYS_pidfd_getfd,
        libc::SYS_process_madvise,
        libc::SYS_kcmp,
        libc::SYS_name_to_handle_at,
        libc::SYS_open_by_handle_at,
        libc::SYS_fanotify_init,
        libc::SYS_syslog,
        libc::SYS_vhangup,
        libc::SYS_sethostname,
        libc::SYS_setdomainname,
    ];

    /// The one exception is the feed fetcher's supervisor's `execve`,
    /// which is how it starts its children, and which Landlock confines to
    /// the kiki executable.
    #[test]
    fn no_profile_allows_what_the_old_denylist_denied() {
        for profile in every_profile() {
            let allowed = allowed_syscalls(&profile);
            let supervisor = matches!(profile, SandboxProfile::FeedFetcher);
            for nr in NEVER_ALLOWED {
                if supervisor && *nr == libc::SYS_execve {
                    continue;
                }
                assert!(!allowed.contains(nr), "syscall {nr} allowed");
            }
        }
    }

    /// Only the supervisor may execute anything, and the processes it
    /// starts, which install their own filters on top of its, may not.
    #[test]
    fn only_the_supervisor_may_execute() {
        for profile in every_profile() {
            let supervisor = matches!(profile, SandboxProfile::FeedFetcher);
            assert_eq!(
                allowed_syscalls(&profile).contains(&libc::SYS_execve),
                supervisor,
                "{}",
                SandboxConfig {
                    profile,
                    log_only: false
                }
                .profile_name()
            );
        }
    }

    /// The supervisor may execute the kiki executable, and whatever it
    /// has mapped: the loader and libraries, for a dynamically linked
    /// build.
    #[test]
    fn the_supervisor_may_run_this_executable_again() {
        let exe = std::env::current_exe().unwrap();
        let (executable, _) = self_exec_paths();
        assert_eq!(executable.first(), Some(&exe));
    }

    /// Every profile's filters compile, for this architecture.
    #[test]
    fn every_profiles_allowlist_compiles() {
        let arch = detect_arch().expect("a supported architecture");
        for profile in every_profile() {
            let rules = allowed_syscalls(&profile)
                .into_iter()
                .map(|nr| (nr, Vec::new()))
                .collect();
            let filter = seccompiler::SeccompFilter::new(
                rules,
                seccompiler::SeccompAction::KillProcess,
                seccompiler::SeccompAction::Allow,
                arch,
            )
            .unwrap();
            let _: seccompiler::BpfProgram = filter.try_into().unwrap();
        }
    }

    /// Forbidden `clone` flags are exactly the namespace ones, none of
    /// which a thread or a `fork` passes.
    #[test]
    fn clone_may_not_create_namespaces() {
        assert_ne!(CLONE_NAMESPACE_FLAGS & libc::CLONE_NEWUSER as u64, 0);
        assert_ne!(CLONE_NAMESPACE_FLAGS & libc::CLONE_NEWNET as u64, 0);
        let thread = libc::CLONE_VM
            | libc::CLONE_FS
            | libc::CLONE_FILES
            | libc::CLONE_SIGHAND
            | libc::CLONE_THREAD
            | libc::CLONE_SYSVSEM
            | libc::CLONE_SETTLS
            | libc::CLONE_PARENT_SETTID
            | libc::CLONE_CHILD_CLEARTID;
        let fork = libc::CLONE_CHILD_CLEARTID | libc::CLONE_CHILD_SETTID | libc::SIGCHLD;
        for flags in [thread, fork] {
            assert_eq!(flags as u64 & CLONE_NAMESPACE_FLAGS, 0);
        }
    }

    /// Every profile is kept from abstract Unix sockets outside its
    /// sandbox, and every profile but the web UI, which stops a `kiki
    /// serve` started outside its own, from signalling outside it.
    #[test]
    fn every_profile_but_the_web_ui_is_scoped_for_signals() {
        for profile in every_profile() {
            let scopes = landlock_scopes(&profile);
            assert!(scopes.contains(Scope::AbstractUnixSocket));
            assert_eq!(
                scopes.contains(Scope::Signal),
                !matches!(profile, SandboxProfile::WebUi)
            );
        }
    }
}
