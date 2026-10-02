//! CPU and memory used by each of Kiki's processes.
//!
//! Most of a feed refresh happens outside the server — fetching and
//! parsing in the feed fetcher, plugins in the script host — so the
//! server's own usage says little about where Kiki spends its resources.
//! [`sample`] reads `/proc` for the server and every process descended from
//! it, and totals them up by the job each one does (see [`Role`]).
//!
//! Memory is reported two ways. Resident memory (RSS) counts every page a
//! process has mapped, including pages it shares with others — and Kiki's
//! processes share a lot: each child is the same executable as the server,
//! so they all map the same pages of its code and of the libraries it
//! loads.
//! Summing RSS across processes counts those pages once per process.
//! Proportional memory (PSS) instead splits each shared page evenly among
//! the processes that map it, so it adds up across processes to the memory
//! Kiki actually occupies.
//!
//! Besides the memory itself, each role reports the peak resident memory its
//! processes have reached and how much of their memory is swapped out, since
//! a snapshot taken every so often misses the bursts of a feed refresh and
//! RSS and PSS leave out whatever the kernel has swapped.
//!
//! PSS comes from `/proc/<pid>/smaps_rollup`, which, unlike `stat`, the
//! kernel lets a process read only if it could ptrace the target. Under
//! Landlock that takes the target to be in the reader's Landlock domain or
//! one nested inside it, which is why the server starts its children only
//! after installing its own Landlock rules (see
//! [`crate::sandbox::restrict_filesystem`]). The feed fetcher's worker,
//! parser and resolver, started by the supervisor, are in domains nested
//! inside the supervisor's, and so inside the server's too.

use std::collections::HashMap;
use std::fs;
use std::io;

/// Which of Kiki's processes a process belongs to.
///
/// A process counts towards the role of the child of the server it
/// descends from, so the feed fetcher's worker, parser and resolver count
/// towards [`Role::FeedFetcher`] along with their supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The server itself.
    Server,
    /// The feed fetcher's supervisor, worker, parser and resolver.
    FeedFetcher,
    /// The `kiki web` process that started the server, if any.
    ///
    /// It is the server's parent, outside the server's Landlock domain, so
    /// its PSS cannot be read (see the module documentation), but its RSS
    /// and CPU time can.
    Web,
    /// The script host.
    ScriptHost,
    /// Any other descendant of the server.
    Other,
}

impl Role {
    /// Every role, in the order [`sample`] reports them.
    pub const ALL: [Role; 5] = [
        Role::Server,
        Role::FeedFetcher,
        Role::ScriptHost,
        Role::Web,
        Role::Other,
    ];

    /// The role's name, as used for the `process` label on metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::FeedFetcher => "feed_fetcher",
            Role::Web => "web",
            Role::ScriptHost => "script_host",
            Role::Other => "other",
        }
    }

    /// The role of a child of the server whose first argument is `arg`.
    fn of_child(arg: &[u8]) -> Self {
        if arg == super::feed_fetcher::SUBCOMMAND.as_bytes() {
            return Role::FeedFetcher;
        }
        #[cfg(feature = "lua")]
        if arg == super::script_host::SUBCOMMAND.as_bytes() {
            return Role::ScriptHost;
        }
        Role::Other
    }
}

/// Resources used by the processes of one [`Role`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Usage {
    /// User and system CPU time, in seconds, used by the role's processes
    /// that are still running.
    pub cpu_seconds: f64,
    /// Total resident memory of the role's processes, in bytes.
    ///
    /// This is the kernel's running count, which may lag the actual figure
    /// slightly, so it is not exactly comparable with `proportional_bytes`.
    pub resident_bytes: u64,
    /// Total proportional memory of the role's processes, in bytes: their
    /// resident memory with each shared page divided among the processes
    /// sharing it.
    ///
    /// `None` if it could not be read for one of the role's processes, as
    /// on kernels older than 4.14, which lack `/proc/<pid>/smaps_rollup`.
    pub proportional_bytes: Option<u64>,
    /// Total swapped-out memory of the role's processes, in bytes (the
    /// `SwapPss` of `smaps_rollup`, so shared swapped pages are split like
    /// PSS). Neither `resident_bytes` nor `proportional_bytes` includes it.
    ///
    /// `None` if it could not be read for one of the role's processes.
    pub swap_bytes: Option<u64>,
    /// Sum of the peak resident memory (`VmHWM`) each of the role's
    /// *running* processes has reached. A process that is replaced starts
    /// again from zero, and the peaks of processes that have exited are
    /// lost.
    ///
    /// `None` if it could not be read for one of the role's processes.
    pub peak_resident_bytes: Option<u64>,
    /// How many processes the role has.
    pub processes: u64,
}

/// The resources used by the calling process and its descendants, by role.
///
/// Roles with no running process are left out. Processes that exit while
/// they are being read are skipped.
///
/// # Errors
///
/// Returns an error if `/proc` cannot be listed, or the calling process's
/// own entry cannot be read.
pub fn sample() -> io::Result<HashMap<Role, Usage>> {
    // SAFETY: `sysconf` only reads system configuration.
    let (ticks_per_second, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_CLK_TCK),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    let ticks_per_second = if ticks_per_second > 0 {
        ticks_per_second as f64
    } else {
        100.0
    };
    let page_size = u64::try_from(page_size).unwrap_or(4096);

    let mut stats = HashMap::new();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in fs::read_dir("/proc")? {
        let Some(pid) = entry
            .ok()
            .and_then(|e| e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()))
        else {
            continue;
        };
        let Some(stat) = fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| Stat::parse(&s))
        else {
            continue;
        };
        children.entry(stat.ppid).or_default().push(pid);
        stats.insert(pid, stat);
    }

    let me = std::process::id();
    if !stats.contains_key(&me) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no readable /proc entry for this process ({me})"),
        ));
    }

    let mut usage: HashMap<Role, Usage> = HashMap::new();
    // `kiki web` starts the server, so it is the server's parent. It isn't
    // one of the server's descendants, so it is looked for separately.
    if let Some(parent) = stats.get(&me).map(|s| s.ppid) {
        if let Some(parent_stat) = stats.get(&parent).filter(|_| is_web(parent)) {
            let u = usage.entry(Role::Web).or_default();
            u.cpu_seconds = parent_stat.cpu_ticks as f64 / ticks_per_second;
            u.resident_bytes = parent_stat.rss_pages * page_size;
            let rollup = rollup(parent);
            accumulate(&mut u.proportional_bytes, true, rollup.and_then(|r| r.pss));
            accumulate(&mut u.swap_bytes, true, rollup.and_then(|r| r.swap_pss));
            accumulate(
                &mut u.peak_resident_bytes,
                true,
                peak_resident_bytes(parent),
            );
            u.processes = 1;
        }
    }
    let mut pending = vec![(me, Role::Server)];
    while let Some((pid, role)) = pending.pop() {
        if let Some(stat) = stats.get(&pid) {
            let u = usage.entry(role).or_default();
            u.cpu_seconds += stat.cpu_ticks as f64 / ticks_per_second;
            u.resident_bytes += stat.rss_pages * page_size;
            let first = u.processes == 0;
            let rollup = rollup(pid);
            accumulate(&mut u.proportional_bytes, first, rollup.and_then(|r| r.pss));
            accumulate(&mut u.swap_bytes, first, rollup.and_then(|r| r.swap_pss));
            accumulate(&mut u.peak_resident_bytes, first, peak_resident_bytes(pid));
            u.processes += 1;
        }
        for &child in children.get(&pid).into_iter().flatten() {
            let child_role = if role == Role::Server {
                child_role(child)
            } else {
                role
            };
            pending.push((child, child_role));
        }
    }
    Ok(usage)
}

/// The role of `pid`, a direct child of the server, going by the
/// subcommand it was started with.
fn child_role(pid: u32) -> Role {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|cmdline| cmdline.split(|&b| b == 0).nth(1).map(Role::of_child))
        .unwrap_or(Role::Other)
}

/// Add one process's `value` to a role's running `total`.
///
/// Once one process's value is missing, the role's total would undercount,
/// so it stays missing. `first` says `total` has nothing added to it yet.
fn accumulate(total: &mut Option<u64>, first: bool, value: Option<u64>) {
    let so_far = if first { Some(0) } else { *total };
    *total = so_far.zip(value).map(|(total, v)| total + v);
}

/// Whether `pid` is a `kiki web` process.
fn is_web(pid: u32) -> bool {
    fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .and_then(|cmdline| cmdline.split(|&b| b == 0).nth(1).map(|a| a == b"web"))
        .unwrap_or(false)
}

/// The figures of `/proc/<pid>/smaps_rollup` that [`sample`] uses, in bytes.
///
/// Reading it walks the process's page tables, so it costs more than
/// reading its `stat`; [`sample`] is meant to run only every so often.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Rollup {
    pss: Option<u64>,
    swap_pss: Option<u64>,
}

/// Read `pid`'s `smaps_rollup`, if the kernel allows it.
fn rollup(pid: u32) -> Option<Rollup> {
    fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .map(|s| parse_rollup(&s))
}

fn parse_rollup(rollup: &str) -> Rollup {
    Rollup {
        pss: kib_field(rollup, "Pss:"),
        swap_pss: kib_field(rollup, "SwapPss:"),
    }
}

/// The peak resident memory (`VmHWM`) of `pid`, in bytes, if it can be read.
fn peak_resident_bytes(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| kib_field(&s, "VmHWM:"))
}

/// The value, in bytes, of the line starting with `name` in a `/proc` file
/// that reports sizes in kB.
fn kib_field(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let kib = line.strip_prefix(name)?.trim().strip_suffix("kB")?;
        kib.trim().parse::<u64>().ok().map(|kib| kib * 1024)
    })
}

/// The fields of `/proc/<pid>/stat` that [`sample`] uses.
#[derive(Debug, PartialEq)]
struct Stat {
    ppid: u32,
    /// `utime` + `stime`, in clock ticks.
    cpu_ticks: u64,
    rss_pages: u64,
}

impl Stat {
    /// Parse a `/proc/<pid>/stat` line (see proc_pid_stat(5)).
    fn parse(line: &str) -> Option<Self> {
        // The command name, in parentheses, may itself contain spaces and
        // parentheses, so the fields are counted from the last `)`.
        let (_, rest) = line.rsplit_once(')')?;
        // `rest` starts at field 3 (state).
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let field = |n: usize| fields.get(n - 3)?.parse::<u64>().ok();
        Some(Stat {
            ppid: u32::try_from(field(4)?).ok()?,
            cpu_ticks: field(14)? + field(15)?,
            rss_pages: field(24)?,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn stat_lines_are_parsed_from_the_end_of_the_command_name() {
        let line = "4242 (kiki (web) x) S 4200 4242 4200 0 -1 4194560 1234 0 0 0 \
                    150 25 0 0 20 0 9 0 12345 104857600 2560 18446744073709551615 \
                    1 1 0 0 0 0 0 4096 17664 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(
            Stat::parse(line),
            Some(Stat {
                ppid: 4200,
                cpu_ticks: 175,
                rss_pages: 2560,
            })
        );
        assert_eq!(Stat::parse("4242 (kiki) S 1"), None);
    }

    #[test]
    fn pss_is_read_from_smaps_rollup() {
        let rollup = "55ccc0599000-7ffc222ad000 ---p 00000000 00:00 0    [rollup]\n\
                      Rss:                1632 kB\n\
                      Pss:                 451 kB\n\
                      Pss_Dirty:           108 kB\n\
                      Pss_Anon:            108 kB\n";
        let parsed = parse_rollup(rollup);
        assert_eq!(parsed.pss, Some(451 * 1024));
        assert_eq!(parsed.swap_pss, None);
        assert_eq!(parse_rollup("Rss: 1632 kB\n").pss, None);
        assert_eq!(
            parse_rollup("Pss: 8 kB\nSwapPss: 3 kB\n").swap_pss,
            Some(3 * 1024)
        );
    }

    #[test]
    fn peak_resident_memory_is_read_from_status() {
        let status = "Name:\tkiki\nVmHWM:\t   2048 kB\nVmRSS:\t   1024 kB\n";
        assert_eq!(kib_field(status, "VmHWM:"), Some(2048 * 1024));
    }

    #[test]
    fn one_missing_value_makes_the_total_missing() {
        let mut total = None;
        accumulate(&mut total, true, Some(5));
        accumulate(&mut total, false, Some(7));
        assert_eq!(total, Some(12));
        accumulate(&mut total, false, None);
        assert_eq!(total, None);
        accumulate(&mut total, false, Some(1));
        assert_eq!(total, None);
    }

    #[test]
    fn subcommands_name_the_role_of_the_servers_children() {
        assert_eq!(Role::of_child(b"__feed-fetcher"), Role::FeedFetcher);
        #[cfg(feature = "lua")]
        assert_eq!(Role::of_child(b"__script-host"), Role::ScriptHost);
        assert_eq!(Role::of_child(b"serve"), Role::Other);
    }

    #[test]
    fn the_calling_process_is_the_server() {
        let usage = sample().unwrap();
        let server = usage.get(&Role::Server).expect("the server's own usage");
        assert!(server.processes >= 1);
        assert!(server.resident_bytes > 0);
        // PSS isn't compared with RSS: `stat`'s RSS comes from per-CPU
        // counters that can lag the page tables `smaps_rollup` walks, by
        // megabytes on a machine with many CPUs.
        assert!(server.proportional_bytes.expect("the server's own PSS") > 0);
    }

    #[test]
    fn children_are_counted_by_role() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let usage = sample();
        child.kill().unwrap();
        child.wait().unwrap();
        // `sleep`'s first argument is `30`, which names no role.
        assert!(usage.unwrap().get(&Role::Other).unwrap().processes >= 1);
    }
}
