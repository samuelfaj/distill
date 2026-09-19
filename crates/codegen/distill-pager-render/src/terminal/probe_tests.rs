use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::{
    DRAIN_TAIL_BYTES, DrainEnd, DrainStats, drain_tty_until, file_status_flags, write_all_until,
};

const TERMINATOR: &[u8] = b"END";

fn ends_with_terminator(tail: &[u8]) -> Option<usize> {
    tail.ends_with(TERMINATOR).then_some(TERMINATOR.len())
}

/// The peer is returned so the caller keeps it alive: a dropped peer turns silence into a hang-up.
fn preloaded_pair(payload: &[u8]) -> (UnixStream, UnixStream) {
    let (reader, mut writer) = UnixStream::pair().expect("socket pair");
    writer.write_all(payload).expect("preload payload");
    (reader, writer)
}

fn far_deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn stops_at_the_terminator() {
    let (reader, _peer) = preloaded_pair(b"abcEND");

    let stats = drain_tty_until(reader.as_raw_fd(), far_deadline(), ends_with_terminator);

    assert_eq!(
        DrainStats {
            bytes: 6,
            end: DrainEnd::Terminated,
            tail_len: 3,
        },
        stats
    );
}

/// Bytes behind the terminator belong to whoever reads the tty next.
#[test]
fn leaves_bytes_after_the_terminator_unread() {
    let prefix = b"\x1b[100;5:3u";
    let suffix = b"typed-later";
    let mut payload = prefix.to_vec();
    payload.extend_from_slice(TERMINATOR);
    payload.extend_from_slice(suffix);
    let (mut reader, _peer) = preloaded_pair(&payload);

    let stats = drain_tty_until(reader.as_raw_fd(), far_deadline(), ends_with_terminator);

    assert_eq!(
        DrainStats {
            bytes: prefix.len() + TERMINATOR.len(),
            end: DrainEnd::Terminated,
            tail_len: TERMINATOR.len(),
        },
        stats
    );
    // A bounded read: over-consumption must fail the test, not hang it
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut unread = vec![0u8; suffix.len()];
    reader.read_exact(&mut unread).expect("suffix still queued");
    assert_eq!(suffix.as_slice(), unread.as_slice());
}

#[test]
fn deadline_is_never_early() {
    let (reader, _peer) = UnixStream::pair().expect("socket pair");
    let timeout = Duration::from_millis(100);
    let started = Instant::now();

    let stats = drain_tty_until(reader.as_raw_fd(), started + timeout, ends_with_terminator);

    assert_eq!(
        DrainStats {
            bytes: 0,
            end: DrainEnd::Deadline,
            tail_len: 0,
        },
        stats
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= timeout, "returned early: {elapsed:?}");
    assert!(
        elapsed < timeout + Duration::from_millis(500),
        "deadline overshoot: {elapsed:?}"
    );
}

/// Far more than `MAX_PROBE_RESPONSE` may precede the terminator.
#[test]
fn residue_has_no_byte_cap() {
    let mut payload = vec![b'x'; 600];
    payload.extend_from_slice(TERMINATOR);
    let (reader, _peer) = preloaded_pair(&payload);

    let stats = drain_tty_until(reader.as_raw_fd(), far_deadline(), ends_with_terminator);

    assert_eq!(
        DrainStats {
            bytes: 603,
            end: DrainEnd::Terminated,
            tail_len: 3,
        },
        stats
    );
}

/// One byte more than the tail can never match, which is why the tail must cover the longest accepted DA1 reply.
#[test]
fn terminator_must_fit_the_tail() {
    let fits = vec![b'F'; DRAIN_TAIL_BYTES];
    let (reader, _peer) = preloaded_pair(&fits);
    let stats = drain_tty_until(reader.as_raw_fd(), far_deadline(), |tail| {
        (tail == fits.as_slice()).then_some(fits.len())
    });
    assert_eq!(
        DrainStats {
            bytes: DRAIN_TAIL_BYTES,
            end: DrainEnd::Terminated,
            tail_len: DRAIN_TAIL_BYTES,
        },
        stats
    );

    let too_long = vec![b'L'; DRAIN_TAIL_BYTES + 1];
    let (reader, _peer) = preloaded_pair(&too_long);
    let deadline = Instant::now() + Duration::from_millis(100);
    let stats = drain_tty_until(reader.as_raw_fd(), deadline, |tail| {
        (tail == too_long.as_slice()).then_some(too_long.len())
    });
    assert_eq!(
        DrainStats {
            bytes: DRAIN_TAIL_BYTES + 1,
            end: DrainEnd::Deadline,
            tail_len: 0,
        },
        stats
    );
}

#[test]
fn write_all_until_delivers_and_restores_flags() {
    let (writer, mut reader) = UnixStream::pair().expect("socket pair");
    let fd = writer.as_raw_fd();
    let flags_before = file_status_flags(fd).expect("F_GETFL");

    assert!(write_all_until(fd, b"\x1b[c", far_deadline()));

    assert_eq!(flags_before, file_status_flags(fd).expect("F_GETFL"));
    reader
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut arrived = [0u8; 3];
    reader.read_exact(&mut arrived).expect("query arrived");
    assert_eq!(b"\x1b[c", &arrived);
}

/// A terminal that stopped reading leaves the output buffer full.
#[test]
fn write_all_until_gives_up_on_a_full_buffer() {
    let (mut writer, _unread_peer) = UnixStream::pair().expect("socket pair");
    writer.set_nonblocking(true).expect("nonblocking fill");
    let chunk = [b'x'; 4096];
    loop {
        match writer.write(&chunk) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill failed: {error}"),
        }
    }
    writer.set_nonblocking(false).expect("blocking again");
    let fd = writer.as_raw_fd();
    let flags_before = file_status_flags(fd).expect("F_GETFL");
    let timeout = Duration::from_millis(100);
    let started = Instant::now();

    let delivered = write_all_until(fd, b"\x1b[c", started + timeout);

    let elapsed = started.elapsed();
    assert!(!delivered, "a full output buffer must not report success");
    assert!(elapsed >= timeout, "returned early: {elapsed:?}");
    assert!(
        elapsed < timeout + Duration::from_millis(500),
        "deadline overshoot: {elapsed:?}"
    );
    assert_eq!(flags_before, file_status_flags(fd).expect("F_GETFL"));
}
