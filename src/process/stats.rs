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
//! and the feed fetcher's worker is forked from its supervisor, so its
//! copy-on-write pages are the supervisor's until either writes to them.
//! Summing RSS across processes counts those pages once per process.
//! Proportional memory (PSS) instead splits each shared page evenly among
//! the processes that map it, so it adds up across processes to the memory
//! Kiki actually occupies.

use std::collections::HashMap;
use std::fs;
use std::io;

/// Which of Kiki's processes a process belongs to.
///
/// A process counts towards the role of the child of the server it
/// descends from, so the feed fetcher's worker counts towards
/// [`Role::FeedFetcher`] along with its supervisor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    /// The server itself.
    Server,
    /// The feed fetcher's supervisor and worker.
    FeedFetcher,
    /// The script host.
    ScriptHost,
    /// Any other descendant of the server.
    Other,
}

impl Role {
    /// The role's name, as used for the `process` label on metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Server => "server",
            Role::FeedFetcher => "feed_fetcher",
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
    pub resident_bytes: u64,
    /// Total proportional memory of the role's processes, in bytes: their
    /// resident memory with each shared page divided among the processes
    /// sharing it.
    ///
    /// `None` if it could not be read for one of the role's processes, as
    /// on kernels older than 4.14, which lack `/proc/<pid>/smaps_rollup`.
    pub proportional_bytes: Option<u64>,
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
    let mut pending = vec![(me, Role::Server)];
    while let Some((pid, role)) = pending.pop() {
        if let Some(stat) = stats.get(&pid) {
            let u = usage.entry(role).or_default();
            u.cpu_seconds += stat.cpu_ticks as f64 / ticks_per_second;
            u.resident_bytes += stat.rss_pages * page_size;
            // Once one process's PSS is missing, the role's total would
            // undercount, so it stays missing.
            let so_far = if u.processes == 0 {
                Some(0)
            } else {
                u.proportional_bytes
            };
            u.proportional_bytes = so_far
                .zip(proportional_bytes(pid))
                .map(|(total, pss)| total + pss);
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

/// The proportional memory of `pid`, in bytes, if it can be read.
///
/// Reading it walks the process's page tables, so it costs more than
/// reading its `stat`; [`sample`] is meant to run only every so often.
fn proportional_bytes(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .ok()
        .and_then(|s| parse_pss(&s))
}

/// The `Pss` of a `/proc/<pid>/smaps_rollup` file, in bytes.
fn parse_pss(rollup: &str) -> Option<u64> {
    rollup.lines().find_map(|line| {
        let kib = line.strip_prefix("Pss:")?.trim().strip_suffix("kB")?;
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
        assert_eq!(parse_pss(rollup), Some(451 * 1024));
        assert_eq!(parse_pss("Rss: 1632 kB\n"), None);
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
        let pss = server.proportional_bytes.expect("the server's own PSS");
        assert!(pss > 0 && pss <= server.resident_bytes);
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
