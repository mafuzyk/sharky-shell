//! Hazard regressions found by comparing the released 3.6.0 binary with bash
//! 5.3.20. Every expectation below was measured against bash first; the
//! comment on each test names the observed bash result.

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty};
use nix::sys::termios::Termios;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

mod common;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sharky-hazards-{tag}-{}-{count}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn sharky_in(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .current_dir(dir)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("failed to run sharky")
}

fn sharky(script: &str) -> Output {
    let dir = temp_dir("run");
    let out = sharky_in(&dir, script);
    std::fs::remove_dir_all(&dir).ok();
    out
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Hazard 1: command substitution must not silently discard output that is not
// valid UTF-8, and must warn about and drop NUL bytes.
//
// bash: `printf '\377' > f; x=$(cat f); printf '%s' "$x" > g` leaves one byte
// (0xFF) in g. `x=$(printf 'a\0b')` warns once and keeps "ab".
// ---------------------------------------------------------------------------

#[test]
fn command_substitution_does_not_discard_invalid_utf8() {
    let dir = temp_dir("cmdsub-utf8");
    std::fs::write(dir.join("f"), b"\xff").unwrap();
    let out = sharky_in(
        &dir,
        r#"x=$(cat f); printf '%s' "$x" > g; wc -c < g"#,
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let written = std::fs::read(dir.join("g")).unwrap();
    assert!(
        !written.is_empty(),
        "the substitution result was silently dropped: g is empty"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn command_substitution_drops_null_bytes_with_a_warning() {
    let out = sharky(r#"x=$(printf 'a\0b'); printf '[%s]' "$x""#);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "[ab]");
    assert!(
        stderr(&out).contains("null byte"),
        "no null-byte warning: {:?}",
        stderr(&out)
    );
}

#[test]
fn command_substitution_warns_once_per_substitution() {
    let out = sharky(r#"x=$(printf 'a\0b\0c'); printf '%s' "$x""#);
    assert_eq!(stdout(&out), "abc");
    let warnings = stderr(&out).lines().filter(|line| line.contains("null byte")).count();
    assert_eq!(warnings, 1, "stderr: {:?}", stderr(&out));
}

// ---------------------------------------------------------------------------
// Hazard 2: a broken pipe must end the shell quietly with bash's status.
//
// bash: `bash -c 'while true; do pwd; done' | head -1` reports PIPESTATUS 141
// and prints nothing on stderr. sharky aborted with SIGABRT and a Rust panic.
// ---------------------------------------------------------------------------

fn wait_with_deadline(mut child: std::process::Child, limit: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        match child.try_wait().expect("try_wait") {
            Some(status) => return Some(status),
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

#[test]
fn broken_stdout_pipe_ends_the_shell_with_sigpipe() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", "while true; do pwd; done"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky");

    let mut pipe = child.stdout.take().expect("stdout");
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = pipe.read(&mut byte) {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    assert!(!line.is_empty(), "the shell printed no line before the pipe closed");
    drop(pipe);

    let status = wait_with_deadline(child, Duration::from_secs(10))
        .expect("the shell kept running after the pipe closed");
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "expected death by SIGPIPE (status 141), got {status:?}"
    );
}

#[test]
fn broken_stderr_pipe_ends_the_shell_with_sigpipe() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", "set -x; while true; do pwd; done"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky");

    let mut pipe = child.stderr.take().expect("stderr");
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = pipe.read(&mut byte) {
        line.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    assert!(!line.is_empty(), "the shell traced nothing before the pipe closed");
    drop(pipe);

    let status = wait_with_deadline(child, Duration::from_secs(10))
        .expect("the shell kept running after the trace pipe closed");
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "expected death by SIGPIPE (status 141), got {status:?}"
    );
}

// ---------------------------------------------------------------------------
