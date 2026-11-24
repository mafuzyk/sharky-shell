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
// Hazard 3: background job notifications are interactive-only.
//
// bash: `x=$(true &); printf "[%s]\n" "$x"` prints `[]`, and `true & printf hi`
// prints only `hi`.
// ---------------------------------------------------------------------------

#[test]
fn background_notification_does_not_leak_into_command_substitution() {
    let out = sharky(r#"x=$(true &); printf '[%s]\n' "$x""#);
    assert_eq!(stdout(&out), "[]\n");
}

#[test]
fn background_notification_does_not_leak_into_script_output() {
    let out = sharky("true & printf hi");
    assert_eq!(stdout(&out), "hi");
}

// ---------------------------------------------------------------------------
// Hazard 4: a missing command path exits 127; an existing non-executable file
// still exits 126.
//
// bash: `./missing` -> 127, `/abs/missing` -> 127, `./noexec` -> 126.
// ---------------------------------------------------------------------------

#[test]
fn missing_relative_command_exits_127() {
    let dir = temp_dir("exit127");
    let out = sharky_in(&dir, "./sharky-hazard-missing");
    assert_eq!(status(&out), 127, "stderr: {}", stderr(&out));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_absolute_command_exits_127() {
    let dir = temp_dir("exit127abs");
    let script = format!("{}/sharky-hazard-missing", dir.display());
    let out = sharky_in(&dir, &script);
    assert_eq!(status(&out), 127, "stderr: {}", stderr(&out));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn non_executable_file_still_exits_126() {
    let dir = temp_dir("exit126");
    std::fs::write(dir.join("noexec"), "#!/bin/sh\necho hi\n").unwrap();
    let out = sharky_in(&dir, "./noexec");
    assert_eq!(status(&out), 126, "stderr: {}", stderr(&out));
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------
// Hazard 5: `wait` on a stopped job returns bash's stop status and keeps the
// job in the table, so fg/bg/kill can still reach it.
//
// bash in a pty: `sleep 100 & kill -STOP %1; wait %1; echo "st=$?"` prints
// st=147 (128+SIGSTOP) and `jobs` still lists the stopped job. `wait` with no
// arguments warns about the stopped job and returns 0.
// ---------------------------------------------------------------------------

struct Session {
    child: Pid,
    master: std::os::fd::OwnedFd,
}

impl Session {
    fn start() -> Self {
        let path = std::ffi::CString::new(env!("CARGO_BIN_EXE_sharky")).unwrap();
        let argv = [std::ffi::CString::new("sharky").unwrap()];
        let search = std::ffi::CString::new(format!(
            "PATH={}",
            common::tool_path().to_string_lossy()
        ))
        .expect("PATH must not contain NUL bytes");
        let env = [
            std::ffi::CString::new(format!("SHARKY_CONFIG={NO_CONFIG}")).unwrap(),
            std::ffi::CString::new("HOME=/tmp").unwrap(),
            search,
            std::ffi::CString::new("TERM=xterm").unwrap(),
        ];

        // SAFETY: the child only execs; every string it needs exists before the fork.
        match unsafe { forkpty(None::<&Winsize>, None::<&Termios>) }.expect("forkpty") {
            ForkptyResult::Child => {
                for fd in 3..256 {
                    let _ = unsafe { libc::close(fd) };
                }
                let _ = execve(&path, &argv, &env);
                unsafe { libc::_exit(1) }
            }
            ForkptyResult::Parent { child, master } => Session { child, master },
        }
    }

    fn fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    fn send(&mut self, text: &str) {
        let mut writer = std::fs::File::from(self.master.try_clone().expect("clone pty"));
        writer.write_all(text.as_bytes()).expect("write pty");
    }

    fn read_until(&self, needle: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        let mut collected = String::new();
        let mut buffer = [0u8; 4096];

        while Instant::now() < deadline {
            if collected.contains(needle) {
                return collected;
            }
            let mut pollfd = libc::pollfd {
                fd: self.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd and one valid read buffer.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
            if ready > 0 {
                let count = unsafe { libc::read(self.fd(), buffer.as_mut_ptr() as *mut _, buffer.len()) };
                if count > 0 {
                    collected.push_str(&String::from_utf8_lossy(&buffer[..count as usize]));
                }
            }
        }
        collected
    }

    fn wait_for_exit(self, timeout: Duration) -> i32 {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match waitpid(self.child, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(20)),
                Ok(WaitStatus::Exited(_, code)) => return code,
                Ok(WaitStatus::Signaled(_, signal, _)) => return 128 + signal as i32,
                Ok(_) => continue,
                Err(_) => return -1,
            }
        }
        -1
    }

    /// Sends one line and waits for `needle`, which must be a string the
    /// command *prints* rather than one the terminal echoes back. The prompt
    /// is then read so the next line is never typed ahead of the shell.
    fn run(&mut self, line: &str, needle: &str) -> String {
        self.send(&format!("{line}\r"));
        let seen = self.read_until(needle, Duration::from_secs(15));
        let _ = self.read_until("❯", Duration::from_secs(2));
        seen
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Only processes this session started: the shell's own job children
        // (they run in their own process group) and then the shell itself.
        for child in job_children(self.child) {
            let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
        }
        let _ = nix::sys::signal::killpg(self.child, nix::sys::signal::Signal::SIGKILL);
        let _ = waitpid(self.child, Some(WaitPidFlag::WNOHANG));
    }
}

/// Direct children of `parent`, read from /proc so a hung session can clean up
/// the background job it started without touching any other process.
fn job_children(parent: Pid) -> Vec<Pid> {
    let path = format!("/proc/{}/task/{}/children", parent.as_raw(), parent.as_raw());
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|field| field.parse::<i32>().ok())
        .map(Pid::from_raw)
        .collect()
}

fn start_session() -> Session {
    let session = Session::start();
    let prompt = session.read_until("❯", Duration::from_secs(10));
    assert!(prompt.contains('❯'), "no prompt: {prompt:?}");
    session
}

#[test]
fn wait_on_a_stopped_job_reports_the_stop_status_and_keeps_the_job() {
    let mut session = start_session();

    let seen = session.run("sleep 100 &", "[1] ");
    assert!(seen.contains("[1] "), "no job id: {seen:?}");
    session.run("kill -STOP %1", "Stopped");

    let seen = session.run("wait %1; echo \"st=$?\"", "st=147");
    assert!(
        seen.contains("st=147"),
        "wait did not report 128+SIGSTOP: {seen:?}"
    );

    let seen = session.run("jobs", "Stopped");
    assert!(
        seen.contains("Stopped"),
        "the stopped job left the table: {seen:?}"
    );

    // The job must still be reachable by job spec.
    let seen = session.run("kill -KILL %1; echo \"killed=$?\"", "killed=0");
    assert!(
        seen.contains("killed=0"),
        "kill %1 could not reach the stopped job: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.wait_for_exit(Duration::from_secs(10)), 0);
}

#[test]
fn wait_without_arguments_warns_about_a_stopped_job_and_returns_zero() {
    let mut session = start_session();

    session.run("sleep 100 &", "[1] ");
    session.run("kill -STOP %1", "Stopped");

    let seen = session.run("wait; echo \"st=$?\"", "st=0");
    assert!(
        seen.contains("st=0"),
        "wait returned a failure status: {seen:?}"
    );

    let seen = session.run("jobs", "Stopped");
    assert!(
        seen.contains("Stopped"),
        "the stopped job left the table: {seen:?}"
    );

    let seen = session.run("kill -KILL %1; echo \"killed=$?\"", "killed=0");
    assert!(
        seen.contains("killed=0"),
        "kill %1 could not reach the stopped job: {seen:?}"
    );
    session.send("exit\r");
    assert_eq!(session.wait_for_exit(Duration::from_secs(10)), 0);
}

// ---------------------------------------------------------------------------
// Hazard 6: the here-document temp file must never open a predictable path.
//
// An attacker who pre-creates a symlink at the old predictable name had its
// target truncated and overwritten with the here-document payload.
// ---------------------------------------------------------------------------

#[test]
fn heredoc_does_not_follow_a_precreated_symlink() {
    let dir = temp_dir("heredoc");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let victim = dir.join("victim");
    std::fs::write(&victim, "original\n").unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .env("TMPDIR", &tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky");

    // The first here-document in a fresh process used the counter value 0.
    let trap = tmp.join(format!("sharky-here-{}-0", child.id()));
    std::os::unix::fs::symlink(&victim, &trap).unwrap();

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"cat <<EOF\npayload\nEOF\n")
        .expect("write script");
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("run sharky");
    assert_eq!(stdout(&out), "payload\n", "the here-document broke: {}", stderr(&out));
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "original\n",
        "the here-document overwrote the symlink target"
    );
    std::fs::remove_dir_all(&dir).ok();
}
