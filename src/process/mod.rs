//! Kiki's multi-process model.
//!
//! A single process needs the union of everything Kiki does: read and
//! write the SQLite database, write into the asset cache, bind a
//! listening socket, and make outbound connections to arbitrary feed
//! servers. A sandbox applied to that process can be no tighter than
//! that union, which puts a floor under how much an in-process sandbox
//! can buy — see [`crate::sandbox`].
//!
//! Splitting the work across processes lifts that floor: each process
//! gets a policy scoped to its own job, so compromising one does not
//! hand an attacker the privileges of the others.
//!
//! # Processes
//!
//! | Process | Started by | Filesystem | Network | Sandbox profile |
//! |---|---|---|---|---|
//! | server | `kiki serve` | data dir + socket dir (rw), system paths (ro) | outbound + listening | [`SandboxProfile::Server`] |
//! | script host | the server, at startup | none | none | [`SandboxProfile::ScriptHost`] |
//!
//! [`SandboxProfile::Server`]: crate::sandbox::SandboxProfile::Server
//! [`SandboxProfile::ScriptHost`]: crate::sandbox::SandboxProfile::ScriptHost
//!
//! # Spawning order
//!
//! Every sandbox profile denies `execve`, so a process cannot spawn
//! children once its own sandbox is installed. The server therefore
//! spawns the script host *first*, in [`crate::cli::serve`], and only
//! then restricts itself. The consequence is deliberate: a script host
//! that dies cannot be replaced, and scripting stays disabled until the
//! server is restarted. Since the host's own error handling keeps script
//! failures — compile errors, runtime errors, timeouts — inside the
//! child, the ways it can actually die are an OOM kill, a panic, or a
//! seccomp violation, and refusing to hand a fresh VM to whatever caused
//! the last one to die is the safer default.
//!
//! Script *reloads* do not need a respawn: the host rebuilds its VM in
//! place when the server sends it a new set of sources.
//!
//! # Transport
//!
//! Parent and child talk over an anonymous `SOCK_STREAM` socket pair
//! created before the fork, which the child inherits on
//! [`script_host::HOST_FD`]. Messages are length-prefixed JSON frames
//! (see [`ipc`]). The child has no way to open another socket, so its
//! parent is the only peer it can ever reach.

pub mod ipc;

#[cfg(all(unix, feature = "lua"))]
pub mod script_host;

/// A shared handle to the script host, or `None` when Lua runs in the
/// server process.
///
/// Aliased so that call sites which only pass the handle along do not
/// need to be `cfg`-gated: on platforms or builds without an isolated
/// host the alias degrades to a unit that is always `None`.
#[cfg(all(unix, feature = "lua"))]
pub type ScriptHostHandle = Option<std::sync::Arc<script_host::ScriptHost>>;

/// See the `unix` + `lua` variant of this alias.
#[cfg(not(all(unix, feature = "lua")))]
pub type ScriptHostHandle = Option<()>;
