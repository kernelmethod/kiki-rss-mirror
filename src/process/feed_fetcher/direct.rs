//! A benchmark-only variant of the feed fetcher in which the supervisor
//! relays nothing, built with the `bench-direct-ipc` feature.
//!
//! The supervisor normally sits in the middle of every frame (see the
//! parent module): server ⇄ supervisor ⇄ worker, and worker ⇄ supervisor ⇄
//! parser or resolver. Here it starts the helpers first and then hands the
//! worker their sockets, along with its own socket to the server, so the
//! worker talks to all three directly and the supervisor does nothing but
//! wait for it to exit. The protocol and every other process are
//! unchanged, so comparing this build with a normal one (see
//! `tools/ipcbench`) measures what the relay costs.
//!
//! What the relay buys is lost, which is why this is not a mode to run:
//!
//! * nothing tracks which requests a dead worker had in hand, so none of
//!   them is answered with [`JobResult::WorkerExited`] and the server waits
//!   out their deadlines;
//! * a worker, parser or resolver that dies is not replaced: the
//!   supervisor stops the others and exits, and the server with it;
//! * the parser is not held to its thread count or [`PARSE_TIMEOUT`], since
//!   no one counts what it has in hand, and the resolver likewise;
//! * the worker shares a socket with the server, and the helpers with the
//!   worker, which the separation in the parent module exists to prevent.

#[cfg(doc)]
use super::PARSE_TIMEOUT;
use super::{
    decode, encode_response, read_frame_async, run_job, write_frame_async, ChildKind, Clients,
    JobResult, ParserClient, Pending, ResolverClient, Spawner, ToFetcher, MAX_FRAME_BYTES,
    WORKER_SUBCOMMAND,
};
use crate::fetcher::assets::{asset_client_builder, AssetTimeouts};
use crate::fetcher::{client_builder, Parsers, ProxiedClient};
use anyhow::{Context, Result};
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// The descriptor the worker inherits its socket to the parser on, after
/// the one to the server on [`CHILD_FD`](crate::process::CHILD_FD).
const PARSER_FD: RawFd = crate::process::CHILD_FD + 1;

/// The descriptor the worker inherits its socket to the resolver on.
const RESOLVER_FD: RawFd = crate::process::CHILD_FD + 2;

/// Where descriptors are parked while they are moved into place, clear
/// of the slots they are moved to.
const PARK_FD: RawFd = 100;

/// The supervisor's whole job in this variant: start the helpers and a
/// worker wired to them and to `server`, then wait for the worker to exit.
pub(super) fn supervise(spawner: &Spawner, server: UnixStream) -> Result<()> {
    let threads = super::parser_threads();
    let (parser, mut parser_proc) = spawner
        .spawn(ChildKind::Parser { threads })
        .context("starting the parser")?;
    let (resolver, mut resolver_proc) = spawner
        .spawn(ChildKind::Resolver)
        .context("starting the resolver")?;
    for stream in [&parser, &resolver] {
        // The worker reads these without blocking; the supervisor's
        // timeouts mean nothing to it.
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(None)?;
    }

    let mut cmd = crate::process::child_command(
        &spawner.program(),
        WORKER_SUBCOMMAND,
        super::WORKER_FD_ENV,
        ChildKind::Worker.env(),
        spawner.log_only,
        spawner.no_sandbox,
    );
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        cmd.arg0(&spawner.exe);
    }
    let fds = [
        (server.as_raw_fd(), crate::process::CHILD_FD),
        (parser.as_raw_fd(), PARSER_FD),
        (resolver.as_raw_fd(), RESOLVER_FD),
    ];
    // SAFETY: the closure runs between fork and exec, and calls nothing
    // but `fcntl` and `dup2`, which are async-signal-safe and allocate
    // nothing.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            // Park every descriptor out of the way first, so that moving
            // one into place cannot clobber another still to be moved.
            // The parked copies are close-on-exec; `dup2` clears the flag
            // on the copies that are meant to survive.
            let mut parked = [0; 3];
            for (slot, (from, _)) in parked.iter_mut().zip(fds) {
                *slot = libc::fcntl(from, libc::F_DUPFD_CLOEXEC, PARK_FD);
                if *slot < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            for (from, (_, to)) in parked.into_iter().zip(fds) {
                if libc::dup2(from, to) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut worker = cmd.spawn().context("starting a fetcher worker")?;
    info!(
        pid = worker.id(),
        "feed fetcher: worker started, wired to the server and its helpers (bench-direct-ipc)"
    );
    // The worker holds the only copies that matter now; the supervisor's
    // would keep the helpers from seeing EOF when it exits.
    drop((parser, resolver));

    let status = worker.wait().context("waiting for the worker")?;
    debug!(%status, "feed fetcher: worker exited; stopping (bench-direct-ipc)");
    for helper in [&mut parser_proc, &mut resolver_proc] {
        let _ = helper.kill();
        let _ = helper.wait();
    }
    // `server` drops here, so the server sees EOF once the worker's copy
    // has gone too.
    drop(server);
    Ok(())
}

/// Take ownership of the socket the supervisor left on `fd` for the worker.
fn take_fd(fd: RawFd, what: &str) -> Result<UnixStream> {
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
        anyhow::bail!("no socket to the {what} on fd {fd}");
    }
    // SAFETY: the supervisor put the socket on `fd` before exec, the check
    // above confirms it is open, and nothing else in this process owns it.
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

/// Make `stream` a tokio stream, and start a task that writes each frame
/// sent on the returned sender to it. The read half is returned too.
fn split(
    stream: UnixStream,
    name: &'static str,
) -> Result<(
    tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
)> {
    stream.set_nonblocking(true)?;
    let (rd, mut wr) = tokio::net::UnixStream::from_std(stream)?.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = write_frame_async(&mut wr, &frame, MAX_FRAME_BYTES).await {
                warn!(error = %e, "feed fetcher: write to the {name} failed");
                return;
            }
        }
    });
    // Buffered, so a small frame arrives in one `recv` rather than two.
    Ok((tx, tokio::io::BufReader::new(rd)))
}

/// The worker in this variant: as [`super::serve`], but with requests from
/// the server, parse tasks to the parser and lookups to the resolver each
/// on a socket of their own.
pub(super) async fn serve(server: UnixStream) -> Result<()> {
    let parser_stream = take_fd(PARSER_FD, "parser")?;
    let resolver_stream = take_fd(RESOLVER_FD, "resolver")?;

    let (tx, mut rd) = split(server, "server")?;
    let (parser_tx, mut parser_rd) = split(parser_stream, "parser")?;
    let (resolver_tx, mut resolver_rd) = split(resolver_stream, "resolver")?;

    let resolver = ResolverClient {
        frames: resolver_tx,
        pending: Arc::new(Pending::new()),
    };
    let parser = ParserClient {
        frames: parser_tx,
        pending: Arc::new(Pending::new()),
        suspects: Arc::new(tokio::sync::Mutex::new(())),
    };

    // The helpers' answers come straight back, each on its own socket.
    let parser_pending = Arc::clone(&parser.pending);
    tokio::spawn(async move {
        while let Ok(frame) = read_frame_async(&mut parser_rd, MAX_FRAME_BYTES).await {
            match decode(&frame) {
                Ok(ToFetcher::Parsed { id, reply }) => parser_pending.complete(id, reply),
                _ => {
                    warn!("feed fetcher: the parser sent a malformed frame");
                    break;
                }
            }
        }
        parser_pending.close();
    });
    let resolver_pending = Arc::clone(&resolver.pending);
    tokio::spawn(async move {
        while let Ok(frame) = read_frame_async(&mut resolver_rd, MAX_FRAME_BYTES).await {
            match decode(&frame) {
                Ok(ToFetcher::Resolved { id, result }) => resolver_pending.complete(id, result),
                _ => {
                    warn!("feed fetcher: the resolver sent a malformed frame");
                    break;
                }
            }
        }
        resolver_pending.close();
    });

    let feeds_resolver = resolver.clone();
    let assets_resolver = resolver.clone();
    let clients = Clients {
        parsers: Parsers::Pool(parser.clone()),
        feeds: ProxiedClient::new(move || {
            client_builder().dns_resolver(Arc::new(feeds_resolver.clone()))
        })
        .context("building the HTTP client")?,
        assets: ProxiedClient::new(move || {
            asset_client_builder(AssetTimeouts::DEFAULT)
                .dns_resolver(Arc::new(assets_resolver.clone()))
        })
        .context("building the HTTP client for assets")?,
    };

    loop {
        let frame = match read_frame_async(&mut rd, MAX_FRAME_BYTES).await {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading a request"),
        };
        let request = match decode(&frame) {
            Ok(ToFetcher::Request(r)) => r,
            Ok(_) => anyhow::bail!("the server sent a helper's answer"),
            Err(e) => return Err(e).context("decoding a request"),
        };
        let clients = clients.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let id = request.id;
            let result = match tokio::spawn(run_job(clients, request.job)).await {
                Ok(r) => r,
                Err(e) => JobResult::Failed {
                    message: format!("fetch task failed: {e}"),
                },
            };
            let _ = tx.send(encode_response(id, result));
        });
    }
    Ok(())
}
