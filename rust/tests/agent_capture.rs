#[path = "../src/agent/capture.rs"]
#[allow(dead_code)] // this target exercises the capture surface only
mod capture;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use capture::{Capture, CaptureContent, StdioGuard};

fn null_file() -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .expect("open /dev/null")
}

/// A second descriptor for `fd`'s open file description, so a test can point it
/// somewhere else and observe the change without touching the test harness.
fn shadow(fd: RawFd) -> File {
    let copy = unsafe { nix::libc::dup(fd) };
    assert!(copy >= 0, "dup({fd}) failed");
    unsafe { File::from_raw_fd(copy) }
}

/// A regular file with a name, standing in for a destination the user chose
/// (`exec >log`). A [`Capture`] cannot play that role: it is unlinked by design
/// and the replay deliberately skips destinations nobody can reach.
fn named_file(name: &str) -> (PathBuf, File) {
    let path = std::env::temp_dir().join(format!(
        "sharky-agent-capture-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("create the named destination");
    (path, file)
}

#[test]
fn temp_file_is_unlinked_0600_and_read_write() {
    let file = capture::capture_file("test").expect("create temp file");
    let meta = file.metadata().expect("fstat temp file");

    assert_eq!(meta.mode() & 0o777, 0o600, "mode must be 0600");
    assert_eq!(meta.nlink(), 0, "name must be unlinked after open");

    let mut file = &file;
    file.write_all(b"hello").expect("write through fd");
    file.seek(SeekFrom::Start(0)).expect("rewind");
    let mut got = String::new();
    file.read_to_string(&mut got).expect("read through fd");
    assert_eq!(got, "hello");
}

#[test]
fn capture_descriptors_stay_above_the_stdio_range() {
    // A request can leave fd 0/1/2 closed (`exec 1>&-`), and the next `open`
    // would then hand out that number. A capture living on fd 1 makes
    // `dup2(1, 1)` a no-op and lets the restore repoint the file the response
    // is read from, so every capture descriptor lives above the stdio range.
    let out = Capture::create("out").expect("create capture");
    let payload = Capture::create("in").expect("create capture");
    assert!(out.fd() >= 10, "stdout capture landed on fd {}", out.fd());
    assert!(
        payload.fd() >= 10,
        "payload capture landed on fd {}",
        payload.fd()
    );
}

#[test]
fn an_unlink_failure_is_an_error_and_keeps_the_name_visible() {
    let dir = std::env::temp_dir().join(format!(
        "sharky-unlink-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");

    let result = capture::create_unlinked(&dir, "test", |_path| {
        Err(std::io::Error::from_raw_os_error(nix::libc::EACCES))
    });

    let error = result.expect_err("a capture that cannot be unlinked must be an error");
    assert_eq!(error.raw_os_error(), Some(nix::libc::EACCES), "{error}");
    let names: Vec<_> = std::fs::read_dir(&dir)
        .expect("read the scratch directory")
        .map(|entry| entry.expect("directory entry").file_name())
        .collect();
    assert_eq!(
        names.len(),
        1,
        "the rejected name must not be reused or hidden: {names:?}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn read_back_truncates_but_reports_real_size() {
    let out = Capture::create("out").expect("create capture");
    out.fill(&[b'x'; 100]).expect("fill");

    let got = out.read_back(10).expect("read back");
    assert_eq!(got.byte_len(), 10, "cap must bound what is returned");
    assert_eq!(got.total, 100, "fstat reports the real size");
    assert!(got.truncated, "100-byte file read with a 10-byte cap");
    assert!(matches!(&got.content, CaptureContent::Text(text) if text.len() == 10));
}

#[test]
fn read_back_does_not_flag_a_file_that_fits() {
    let out = Capture::create("out").expect("create capture");
    out.fill(b"hello").expect("fill");

    let got = out.read_back(5).expect("exact cap");
    assert!(!got.truncated, "content fits exactly");
    assert_eq!(got.total, 5);

    let got = out.read_back(64).expect("generous cap");
    assert!(!got.truncated);
    assert!(matches!(&got.content, CaptureContent::Text(text) if text == "hello"));
}

#[test]
fn valid_utf8_is_text() {
    let out = Capture::create("out").expect("create capture");
    out.fill("olá\n".as_bytes()).expect("fill");
    let got = out.read_back(64).expect("read back");
    match got.content {
        CaptureContent::Text(text) => assert_eq!(text, "olá\n"),
        CaptureContent::Binary(bytes) => panic!("expected text, got {bytes:?}"),
    }
}

#[test]
fn invalid_utf8_is_binary() {
    let out = Capture::create("out").expect("create capture");
    let raw = [0x00u8, 0xff, 0xfe, 0x41];
    out.fill(&raw).expect("fill");
    let got = out.read_back(64).expect("read back");
    match got.content {
        CaptureContent::Binary(bytes) => assert_eq!(bytes, raw),
        CaptureContent::Text(text) => panic!("expected binary, got {text:?}"),
    }
}

#[test]
fn fill_keeps_the_read_offset_at_the_start() {
    let payload = Capture::create("in").expect("create capture");
    payload.fill(b"payload\n").expect("fill");

    // The driver dup2()s a payload file onto fd 0; reading it must start at the
    // beginning, so `fill` writes without moving the shared file offset.
    let null = null_file();
    let target = shadow(null.as_raw_fd());
    capture::dup2_onto(payload.fd(), target.as_raw_fd()).expect("dup2 payload");

    let mut reader = &target;
    let mut got = String::new();
    reader.read_to_string(&mut got).expect("read payload");
    assert_eq!(got, "payload\n");
}

#[test]
fn guard_swaps_and_restores_on_drop() {
    let out = Capture::create("out").expect("create capture");
    let err = Capture::create("err").expect("create capture");
    let null = null_file();
    let first = shadow(null.as_raw_fd());
    let second = shadow(null.as_raw_fd());

    {
        let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
        guard
            .swap(first.as_raw_fd(), out.fd())
            .expect("swap first");
        guard
            .swap(second.as_raw_fd(), err.fd())
            .expect("swap second");
        assert_eq!(
            capture::identity(first.as_raw_fd()).unwrap(),
            out.identity()
        );
        assert_eq!(
            capture::identity(second.as_raw_fd()).unwrap(),
            err.identity()
        );
    }

    assert_eq!(
        capture::identity(first.as_raw_fd()).unwrap(),
        capture::identity(null.as_raw_fd()).unwrap(),
        "drop must restore the first descriptor"
    );
    assert_eq!(
        capture::identity(second.as_raw_fd()).unwrap(),
        capture::identity(null.as_raw_fd()).unwrap(),
        "drop must restore the second descriptor"
    );
}

#[test]
fn guard_leaves_a_user_redirect_in_place() {
    let out = Capture::create("out").expect("create capture");
    let user = Capture::create("user").expect("create capture");
    let null = null_file();
    let target = shadow(null.as_raw_fd());

    {
        let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
        guard.swap(target.as_raw_fd(), out.fd()).expect("swap");
        capture::dup2_onto(user.fd(), target.as_raw_fd()).expect("user redirect");
    }

    assert_eq!(
        capture::identity(target.as_raw_fd()).unwrap(),
        user.identity(),
        "restore must not clobber a redirect the identity check does not own"
    );
}

#[test]
fn guard_restores_the_destination_it_found_and_replays_the_output() {
    let out = Capture::create("out").expect("create capture");
    let (path, user) = named_file("destination");
    let null = null_file();
    // What fd 1 points at when the request starts: a destination the user chose
    // in an earlier request.
    let target = shadow(user.as_raw_fd());
    let user_identity = capture::identity(user.as_raw_fd()).unwrap();

    {
        let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
        guard
            .swap_output(target.as_raw_fd(), out.fd())
            .expect("swap");
        assert_eq!(
            capture::identity(target.as_raw_fd()).unwrap(),
            out.identity(),
            "the request runs with the capture on the descriptor"
        );
        // The request writes to fd 1, which is the capture right now.
        let mut file = &target;
        file.write_all(b"second\n").expect("write to the capture");
    }

    assert_eq!(
        capture::identity(target.as_raw_fd()).unwrap(),
        user_identity,
        "the descriptor must go back to the file it pointed at, not to /dev/null"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the saved destination"),
        "second\n",
        "the request's output must reach the destination the user chose"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn guard_replays_only_into_the_saved_destination() {
    let out = Capture::create("out").expect("create capture");
    let other = Capture::create("other").expect("create capture");
    let (path, user) = named_file("previous-destination");
    let null = null_file();
    let target = shadow(user.as_raw_fd());

    {
        let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
        guard
            .swap_output(target.as_raw_fd(), out.fd())
            .expect("swap");
        let mut file = &target;
        file.write_all(b"before\n").expect("write to the capture");
        // `exec >other` inside the request: the redirect persists, so the
        // restore leaves the descriptor where the request put it.
        capture::dup2_onto(other.fd(), target.as_raw_fd()).expect("user redirect");
    }

    assert_eq!(
        capture::identity(target.as_raw_fd()).unwrap(),
        other.identity(),
        "the request's own redirect must survive the restore"
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read the saved destination"),
        "before\n",
        "output from before `exec >other` belongs to the previous destination"
    );
    std::fs::remove_file(&path).ok();
}

#[test]
fn guard_does_not_replay_into_an_unreachable_destination() {
    let out = Capture::create("out").expect("create capture");
    // The shape a stale capture has after `exec 2>&1`: a regular file with no
    // name left, so no reader can ever see what reaches it.
    let stale = Capture::create("stale").expect("create capture");
    let null = null_file();
    let target = shadow(stale.fd());

    {
        let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
        guard
            .swap_output(target.as_raw_fd(), out.fd())
            .expect("swap");
        let mut file = &target;
        file.write_all(b"bytes\n").expect("write to the capture");
    }

    assert_eq!(
        capture::identity(target.as_raw_fd()).unwrap(),
        stale.identity(),
        "the user's redirect still persists"
    );
    let got = stale.read_back(64).expect("read the stale destination");
    assert!(
        matches!(&got.content, CaptureContent::Text(text) if text.is_empty()),
        "an unreachable destination must not grow with every request"
    );
}

/// Set in the child process of
/// [`capture_and_restore_survive_a_closed_stdout_descriptor`].
const CLOSED_STDIO_ENV: &str = "SHARKY_CAPTURE_CLOSED_STDIO_CHILD";
const CLOSED_STDIO_TEST: &str = "capture_and_restore_survive_a_closed_stdout_descriptor";

#[test]
fn capture_and_restore_survive_a_closed_stdout_descriptor() {
    // fd 1 is process-global: closing it here would break the output of every
    // other test in this binary, and forking a multithreaded harness is racy.
    // The assertions run in a dedicated process that re-runs just this test.
    if std::env::var_os(CLOSED_STDIO_ENV).is_none() {
        let exe = std::env::current_exe().expect("path of the running test binary");
        let output = std::process::Command::new(exe)
            .args(["--exact", CLOSED_STDIO_TEST, "--nocapture"])
            .env(CLOSED_STDIO_ENV, "1")
            .output()
            .expect("spawn the closed-stdio child");
        assert!(
            output.status.success(),
            "the closed-stdio child failed: {:?}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    // The session's null file is opened first, so closing fd 1 does not make
    // the next open take that number.
    let null = null_file();
    // SAFETY: this process owns fd 1; the test reopens it before returning.
    assert_eq!(unsafe { nix::libc::close(1) }, 0, "close fd 1");

    // A capture must never take the free fd 1: `dup2(1, 1)` would be a no-op
    // and the identity-checked restore would repoint the file being read.
    let out = Capture::create("out").expect("create a capture with fd 1 closed");
    assert!(out.fd() >= 10, "capture landed on fd {}", out.fd());

    let mut guard = StdioGuard::new(null.as_raw_fd()).expect("guard");
    guard
        .swap_output(1, out.fd())
        .expect("swap the capture onto fd 1");
    // SAFETY: fd 1 is the request's capture for these three bytes.
    let wrote = unsafe { nix::libc::write(1, b"hi\n".as_ptr() as *const _, 3) };
    assert_eq!(wrote, 3, "write to the request's fd 1");
    guard.restore().expect("restore fd 1");

    // The descriptor was closed when the request started, and "closed stays
    // closed" is the same rule as a user redirect: the restore must not open
    // it on the session's null file.
    // SAFETY: a plain fcntl probe on this process's own descriptor.
    let flags = unsafe { nix::libc::fcntl(1, nix::libc::F_GETFD) };
    assert!(
        flags < 0 && std::io::Error::last_os_error().raw_os_error() == Some(nix::libc::EBADF),
        "the restore must leave a descriptor the request closed closed"
    );

    let got = out.read_back(64).expect("read the request's capture");
    match got.content {
        CaptureContent::Text(text) => assert_eq!(text, "hi\n", "the capture must survive the swap"),
        CaptureContent::Binary(bytes) => panic!("expected text, got {bytes:?}"),
    }

    // Leave fd 1 open on /dev/null so the harness can report this test; the
    // `File` must not be dropped at the end of the function.
    let reopened = OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .expect("reopen stdout on /dev/null");
    assert_eq!(reopened.as_raw_fd(), 1, "the lowest free descriptor is fd 1");
    std::mem::forget(reopened);
}
