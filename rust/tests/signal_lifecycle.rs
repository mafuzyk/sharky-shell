//! Process-lifecycle behaviour shared by scripts and the interactive prompt:
//! the EXIT action must run when an untrapped fatal signal ends the shell, and
//! a background job that finishes while the user idles at the prompt must be
//! reaped and announced without a typed command.
//!
//! Every format and status here was measured against bash 5.3 first with
//! `LC_ALL=C`; bash prints `[1]+  Done                       sleep 1` (the
//! status field is 27 columns wide) after `set -b` while the prompt is idle.

use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty};
use nix::sys::signal::{Signal, kill};
use nix::sys::termios::Termios;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

mod common;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

/// Runs a script file through sharky, the route a real cleanup trap takes.
fn sharky_script(source: &str) -> std::process::Output {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "sharky-lifecycle-{}-{}.sh",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, source).expect("write script");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&path)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("run sharky");
    let _ = std::fs::remove_file(&path);
    out
}

fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn exit_trap_runs_when_an_untrapped_sigterm_ends_the_shell() {
    let out = sharky_script("trap 'echo CLEAN' EXIT\nkill -TERM $$\nsleep 5\n");
    assert_eq!(out.status.code(), Some(143), "stderr={:?}", stderr(&out));
    assert_eq!(stdout(&out), "CLEAN\n");
}

#[test]
fn exit_trap_runs_when_an_untrapped_sighup_ends_the_shell() {
    let out = sharky_script("trap 'echo CLEAN' EXIT\nkill -HUP $$\nsleep 5\n");
    assert_eq!(out.status.code(), Some(129), "stderr={:?}", stderr(&out));
    assert_eq!(stdout(&out), "CLEAN\n");
}

#[test]
fn exit_trap_runs_on_a_default_sigint_in_a_script() {
    let out = sharky_script("trap 'echo CLEAN' EXIT\nkill -INT $$\nsleep 5\n");
    assert_eq!(out.status.code(), Some(130), "stderr={:?}", stderr(&out));
    assert_eq!(stdout(&out), "CLEAN\n");
}

/// The trap must run once, after the signal arrives even if the shell was
/// blocked waiting for a foreground child. Bash abandons that child and runs
/// the action before exiting.
#[test]
fn exit_trap_runs_when_the_fatal_signal_arrives_during_a_wait() {
    let dir = std::env::temp_dir().join(format!("sharky-lifecycle-wait-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let marker = dir.join("clean");
    let script = format!("trap 'echo CLEAN > {}' EXIT\nsleep 10\n", marker.display());
    let path = dir.join("wait.sh");
    std::fs::write(&path, script).expect("write script");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&path)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .spawn()
        .expect("spawn sharky");

    std::thread::sleep(Duration::from_millis(400));
    nix::sys::signal::kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM).expect("kill");

    let deadline = Instant::now() + Duration::from_secs(3);
    let status = loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => break status,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the shell ignored SIGTERM while waiting for a child");
            }
        }
    };
    std::thread::sleep(Duration::from_millis(100));
    let cleaned = marker.exists();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(status.code(), Some(143), "status={status:?}");
    assert!(cleaned, "the EXIT action did not run");
}

#[test]
fn the_exit_action_of_a_fatal_signal_runs_exactly_once() {
    let dir = std::env::temp_dir().join(format!("sharky-lifecycle-once-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let marker = dir.join("count");
    let script = format!(
        "trap 'printf x >> {}' EXIT\nkill -TERM $$\nsleep 5\n",
        marker.display()
    );
    let out = sharky_script(&script);
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(143));
    assert_eq!(written, "x", "the EXIT action ran more than once");
}

#[test]
fn a_normal_exit_still_runs_the_exit_action_once() {
    let out = sharky_script("trap 'echo CLEAN' EXIT\nexit 7\n");
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(stdout(&out), "CLEAN\n");
}

// ---------------------------------------------------------------------------
// Interactive lifecycle
// ---------------------------------------------------------------------------

struct Session {
    child: Pid,
    master: Option<std::os::fd::OwnedFd>,
    home: PathBuf,
}

impl Session {
    fn start() -> Self {
        static HOME_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "sharky-lifecycle-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            HOME_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let home = std::env::temp_dir().join(&unique);
        let xdg = home.join("xdg");
        let _ = std::fs::create_dir_all(&xdg);

        let path = CString::new(env!("CARGO_BIN_EXE_sharky")).unwrap();
        let argv = [CString::new("sharky").unwrap()];
        let env = [
            CString::new("SHARKY_CONFIG=/nonexistent/sharky-test").unwrap(),
            CString::new(format!("HOME={}", home.display())).unwrap(),
            CString::new(format!("XDG_DATA_HOME={}", xdg.display())).unwrap(),
            CString::new(format!("PATH={}", common::tool_path().to_string_lossy())).unwrap(),
            CString::new("TERM=xterm").unwrap(),
            CString::new("LC_ALL=C").unwrap(),
        ];

        // SAFETY: the child only execs; every string it needs exists beforehand.
        match unsafe { forkpty(None::<&Winsize>, None::<&Termios>) }.expect("forkpty") {
            ForkptyResult::Child => {
                for fd in 3..256 {
                    let _ = unsafe { libc::close(fd) };
                }
                let _ = execve(&path, &argv, &env);
                unsafe { libc::_exit(1) }
            }
            ForkptyResult::Parent { child, master } => Session {
                child,
                master: Some(master),
                home: home.clone(),
            },
        }
    }

    fn fd(&self) -> RawFd {
        self.master
            .as_ref()
            .expect("pty master is open")
            .as_raw_fd()
    }

    fn send(&mut self, text: &str) {
        let master = self.master.as_ref().expect("pty master is open");
        let mut writer = std::fs::File::from(master.try_clone().unwrap());
        let _ = writer.write_all(text.as_bytes());
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
                let count =
                    unsafe { libc::read(self.fd(), buffer.as_mut_ptr() as *mut _, buffer.len()) };
                if count > 0 {
                    collected.push_str(&String::from_utf8_lossy(&buffer[..count as usize]));
                } else {
                    break;
                }
            }
        }
        collected
    }

    fn wait_with_deadline(&self, timeout: Duration) -> Option<i32> {
        let deadline = Instant::now() + timeout;
        loop {
            match waitpid(self.child, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(WaitStatus::StillAlive) => return None,
                Ok(WaitStatus::Exited(_, code)) => return Some(code),
                Ok(WaitStatus::Signaled(_, signal, _)) => return Some(128 + signal as i32),
                // An interrupted poll (a SIGCHLD for another child of this test
                // process) is not a verdict: keep waiting until the deadline.
                Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => return None,
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.master = None;
        let _ = kill(self.child, Signal::SIGKILL);
        let _ = waitpid(self.child, None);
        std::fs::remove_dir_all(&self.home).ok();
    }
}

/// Installs the EXIT action on a fresh prompt and waits until the shell has
/// actually run the command. The typed line itself contains the trap source,
/// so the marker is a printf token the input never shows.
fn prepare_exit_trap(session: &mut Session, marker: &std::path::Path) {
    session.send(&format!(
        "trap 'echo CLEAN > {}' EXIT; printf 'READY%s\n' -\r",
        marker.display()
    ));
    let seen = session.read_until("READY-", Duration::from_secs(5));
    assert!(
        seen.contains("READY-"),
        "startup command did not run: {seen:?}"
    );
    session.read_until("❯", Duration::from_secs(5));
}

#[test]
fn exit_trap_runs_when_an_idle_prompt_receives_sigterm() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    let marker = session.home.join("clean");
    prepare_exit_trap(&mut session, &marker);

    kill(session.child, Signal::SIGTERM).expect("kill -TERM");
    let status = session.wait_with_deadline(Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(100));
    let cleaned = marker.exists();
    let home = session.home.clone();
    std::fs::remove_dir_all(&home).ok();
    assert_eq!(status, Some(143), "the shell did not die with 128+SIGTERM");
    assert!(
        cleaned,
        "the EXIT action did not run for SIGTERM at the prompt"
    );
}

/// Closing the terminal sends SIGHUP and takes standard input with it; the
/// shell must still run its cleanup and report the hangup's status. A real
/// terminal close delivers that signal once: closing the master queues
/// SIGHUP for the foreground process group. A pty master inherited by another
/// test process's child can delay that delivery, so an explicit hangup is kept
/// as a fallback, but it is only sent when the shell has not reacted to the
/// kernel's own hangup. Sending it eagerly races a second SIGHUP into the EXIT
/// action, which aborts the cleanup here and in bash 5.3 alike.
#[test]
fn exit_trap_runs_when_the_terminal_is_closed() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    let marker = session.home.join("clean");
    prepare_exit_trap(&mut session, &marker);

    // Dropping the master is a hangup: no terminal is left to read or write,
    // and the kernel queues SIGHUP for the shell by itself.
    drop(session.master.take());
    let mut status = None;
    let grace = Instant::now() + Duration::from_millis(300);
    while status.is_none() && Instant::now() < grace {
        status = session.wait_with_deadline(Duration::from_millis(20));
    }
    if status.is_none() {
        // The kernel's hangup was delayed: deliver one explicitly while the
        // shell is still alive, exactly as a closed terminal would.
        kill(session.child, Signal::SIGHUP).expect("kill -HUP");
        status = session.wait_with_deadline(Duration::from_secs(5));
    }

    std::thread::sleep(Duration::from_millis(100));
    let cleaned = marker.exists();
    let home = session.home.clone();
    std::fs::remove_dir_all(&home).ok();
    assert_eq!(status, Some(129), "the shell did not die with 128+SIGHUP");
    assert!(cleaned, "the EXIT action did not run for a terminal hangup");
}

#[test]
fn a_finished_background_job_is_announced_while_the_prompt_is_idle() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    session.send("sleep 1 &\r");
    session.read_until("[1]", Duration::from_secs(5));

    // No command is typed: the shell itself must notice and announce.
    let seen = session.read_until("[1]+  Done", Duration::from_secs(5));
    assert!(
        seen.contains("[1]+  Done"),
        "no asynchronous job notice while idle: {seen:?}"
    );
    assert!(
        seen.contains("sleep 1"),
        "the notice did not name the job: {seen:?}"
    );
    assert!(
        seen.contains('❯'),
        "the prompt was not redrawn after the notice: {seen:?}"
    );

    session.send("jobs\r");
    let listing = session.read_until("❯", Duration::from_secs(5));
    assert!(
        !listing.contains("Running"),
        "the finished job was not reaped: {listing:?}"
    );
    session.send("exit\r");
    let status = session.wait_with_deadline(Duration::from_secs(5));
    std::fs::remove_dir_all(&session.home).ok();
    assert_eq!(status, Some(0));
}

#[test]
fn the_job_notice_keeps_a_partially_typed_line_intact() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    session.send("sleep 1 &\r");
    session.read_until("[1]", Duration::from_secs(5));
    let marker = session.home.join("partial");
    // Type a line but do not submit it, so the notice arrives while the line
    // is being edited.
    session.send(&format!("echo EDITED > {}", marker.display()));
    std::thread::sleep(Duration::from_millis(150));

    let seen = session.read_until("[1]+  Done", Duration::from_secs(5));
    assert!(
        seen.contains("[1]+  Done"),
        "no asynchronous notice with a typed line: {seen:?}"
    );
    // The line is redrawn below the notice, so it appears again after it.
    let after_notice = seen.split("[1]+  Done").nth(1).unwrap_or("");
    assert!(
        after_notice.contains("echo EDITED") || seen.matches("echo EDITED").count() >= 2,
        "the typed line was not redrawn after the notice: {seen:?}"
    );

    session.send("\r");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut written = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            written = text;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        written.trim(),
        "EDITED",
        "the typed line was corrupted by the notice: {seen:?}"
    );
    session.send("exit\r");
    let status = session.wait_with_deadline(Duration::from_secs(5));
    std::fs::remove_dir_all(&session.home).ok();
    assert_eq!(status, Some(0));
}

#[test]
fn a_background_job_that_exits_nonzero_is_announced_with_its_status() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    session.send("(exit 3) &\r");
    session.read_until("[1]", Duration::from_secs(5));

    let seen = session.read_until("Exit 3", Duration::from_secs(5));
    assert!(
        seen.contains("[1]+  Exit 3"),
        "bash reports a nonzero exit as `Exit N`: {seen:?}"
    );
    session.send("exit\r");
    let status = session.wait_with_deadline(Duration::from_secs(5));
    std::fs::remove_dir_all(&session.home).ok();
    assert_eq!(status, Some(0));
}

#[test]
fn a_background_job_killed_by_a_signal_is_announced_by_signal() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));
    session.send("sleep 5 &\r");
    session.read_until("[1]", Duration::from_secs(5));
    session.send("kill -TERM %1\r");

    let seen = session.read_until("Terminated", Duration::from_secs(5));
    assert!(
        seen.contains("[1]+  Terminated"),
        "bash names the fatal signal: {seen:?}"
    );
    session.send("exit\r");
    let status = session.wait_with_deadline(Duration::from_secs(5));
    std::fs::remove_dir_all(&session.home).ok();
    assert_eq!(status, Some(0));
}
