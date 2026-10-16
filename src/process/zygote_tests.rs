//! Tests of the zygote's parts that run without forking: forking from the
//! test harness, which runs many threads, is not safe. The zygote itself
//! is tested through the `kiki` binary, in `tests/sandbox.rs`.

use super::*;

/// The server may have the supervisor and the script host started, and
/// the supervisor its worker, parser and resolver; nothing may have
/// anything else started.
#[test]
fn each_connection_may_start_only_its_own_children() {
    let roles = [
        Role::FeedFetcher,
        Role::FeedWorker,
        Role::FeedParser { threads: 2 },
        Role::FeedResolver,
        Role::ScriptHost,
    ];
    for role in roles {
        assert_eq!(
            role.may_be_started_by(None),
            matches!(role, Role::FeedFetcher | Role::ScriptHost),
            "{role:?} by the server"
        );
        assert_eq!(
            role.may_be_started_by(Some(Role::FeedFetcher)),
            matches!(
                role,
                Role::FeedWorker | Role::FeedParser { .. } | Role::FeedResolver
            ),
            "{role:?} by the supervisor"
        );
        for requester in roles.into_iter().filter(|r| *r != Role::FeedFetcher) {
            assert!(
                !role.may_be_started_by(Some(requester)),
                "{role:?} by {requester:?}"
            );
        }
    }
}

/// Only the supervisor is given a connection to the zygote of its own, and
/// only the parser and the resolver lose the environment.
#[test]
fn roles_are_set_up_as_their_subcommands_are() {
    assert!(Role::FeedFetcher.starts_children());
    assert!(!Role::ScriptHost.starts_children());
    assert!(!Role::FeedWorker.starts_children());
    assert_eq!(Role::FeedWorker.kept_environment(), None);
    assert_eq!(Role::ScriptHost.kept_environment(), None);
    assert_eq!(
        Role::FeedParser { threads: 1 }.kept_environment(),
        Some(feed_fetcher::PARSER_ENV)
    );
    assert_eq!(
        Role::FeedResolver.kept_environment(),
        Some(feed_fetcher::RESOLVER_ENV)
    );
    assert_eq!(
        Role::FeedParser { threads: 1 }.subcommand(),
        feed_fetcher::PARSER_SUBCOMMAND
    );
}

/// A descriptor sent over a socket arrives as a working descriptor of the
/// same socket, close-on-exec, and the frames around it are unaffected.
#[test]
fn descriptors_pass_over_a_socket() {
    let (a, b) = UnixStream::pair().unwrap();
    let (mut ours, mut theirs) = UnixStream::pair().unwrap();
    write_frame(&mut ours, b"before").unwrap();
    send_fd(&ours, a.as_raw_fd()).unwrap();
    write_frame(&mut ours, b"after").unwrap();
    drop(a);

    assert_eq!(read_frame(&mut theirs).unwrap(), b"before");
    let fd = recv_fd(&theirs).unwrap();
    // SAFETY: `fcntl(F_GETFD)` only inspects the descriptor table entry.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    assert_ne!(flags & libc::FD_CLOEXEC, 0);
    assert_eq!(read_frame(&mut theirs).unwrap(), b"after");

    let mut received = UnixStream::from(fd);
    use std::io::{Read, Write};
    received.write_all(b"x").unwrap();
    let mut b = b;
    let mut byte = [0u8; 1];
    b.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"x");
}

/// A reply that should carry a descriptor but does not is an error, not a
/// hang or a descriptor made up.
#[test]
fn a_reply_without_a_descriptor_is_an_error() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    use std::io::Write;
    (&ours).write_all(&[0]).unwrap();
    assert!(recv_fd(&theirs).is_err());
    drop(ours);
    assert_eq!(
        recv_fd(&theirs).unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

/// The protocol's messages survive the wire.
#[test]
fn requests_and_replies_round_trip() {
    let request = Request::Spawn {
        role: Role::FeedParser { threads: 3 },
        log_only: true,
        no_sandbox: false,
    };
    match decode::<Request>(&encode(&request).unwrap()).unwrap() {
        Request::Spawn {
            role,
            log_only,
            no_sandbox,
        } => {
            assert_eq!(role, Role::FeedParser { threads: 3 });
            assert!(log_only);
            assert!(!no_sandbox);
        }
        other => panic!("{other:?}"),
    }
    match decode::<Reply>(&encode(&Reply::Ended { status: 9 }).unwrap()).unwrap() {
        Reply::Ended { status } => assert_eq!(status, 9),
        other => panic!("{other:?}"),
    }
}
