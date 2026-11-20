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
