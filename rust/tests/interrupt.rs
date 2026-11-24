//! Interactive Ctrl-C and fatal-signal handling.
//!
//! These tests drive a real terminal: the shell ignores a signal for its own
//! process group, so the only faithful check is a signal sent by the terminal
//! driver to the foreground process group, which is what a PTY does.

use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty, openpty};
use nix::sys::termios::Termios;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

mod common;

struct Session {
    child: Pid,
    master: std::os::fd::OwnedFd,
    home: PathBuf,
}

impl Session {
    fn start() -> Self {
        static HOME_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "sharky-int-{}-{}-{}",
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
        let search_path = CString::new(format!("PATH={}", common::tool_path().to_string_lossy()))
            .expect("PATH must not contain NUL bytes");
        let home_env = CString::new(format!("HOME={}", home.display()))
            .expect("HOME must not contain NUL bytes");
        let xdg_env = CString::new(format!("XDG_DATA_HOME={}", xdg.display()))
            .expect("XDG_DATA_HOME must not contain NUL bytes");
        let env = [
            CString::new("SHARKY_CONFIG=/nonexistent/sharky-test").unwrap(),
            home_env,
            xdg_env,
            search_path,
            CString::new("TERM=xterm").unwrap(),
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
            ForkptyResult::Parent { child, master } => Session { child, master, home },
        }
    }

    fn fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }

    fn send(&mut self, text: &str) {
        let mut writer = std::fs::File::from(self.master.try_clone().unwrap());
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
                }
            }
        }

        collected
    }

    fn exit_status(self) -> i32 {
        match waitpid(self.child, None) {
            Ok(WaitStatus::Exited(_, code)) => code,
            Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
            other => panic!("shell did not exit normally: {other:?}"),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.child, nix::sys::signal::Signal::SIGKILL);
        let _ = waitpid(self.child, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// Sends Ctrl-C three times, spaced out, so a shell that ignores one of the
/// deliveries still sees the others. A single byte can be lost when the shell
/// is busy; three attempts over a second are not.
fn ctrl_c(session: &mut Session) {
    for _ in 0..3 {
        session.send("\x03");
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn ctrl_c_interrupts_a_builtin_only_loop() {
    let mut session = Session::start();
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    session.send("while :; do :; done\r");
    std::thread::sleep(Duration::from_millis(400));
    ctrl_c(&mut session);

    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(
        seen.contains('❯'),
        "the shell never returned to the prompt after Ctrl-C: {seen:?}"
    );

    session.send("printf 'INT-%s\\n' $?\r");
    let seen = session.read_until("INT-130", Duration::from_secs(5));
    assert!(
        seen.contains("INT-130"),
        "interrupted loop did not report status 130: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn ctrl_c_breaks_a_builtin_for_loop() {
    let mut session = Session::start();
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    session.send("for ((i=0; ; i++)); do :; done\r");
    std::thread::sleep(Duration::from_millis(400));
    ctrl_c(&mut session);

    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(
        seen.contains('❯'),
        "the shell never returned to the prompt after Ctrl-C: {seen:?}"
    );

    session.send("printf 'FOR-%s\\n' $?\r");
    let seen = session.read_until("FOR-130", Duration::from_secs(5));
    assert!(
        seen.contains("FOR-130"),
        "interrupted loop did not report status 130: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn ctrl_c_unblocks_the_read_builtin() {
    let mut session = Session::start();
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    session.send("read -r answer\r");
    std::thread::sleep(Duration::from_millis(400));
    ctrl_c(&mut session);

    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(
        seen.contains('❯'),
        "the shell stayed blocked in read after Ctrl-C: {seen:?}"
    );

    session.send("printf 'READ-%s\\n' $?\r");
    let seen = session.read_until("READ-130", Duration::from_secs(5));
    assert!(
        seen.contains("READ-130"),
        "interrupted read did not report status 130: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn ctrl_c_aborts_a_loop_waiting_on_an_external_command() {
    let mut session = Session::start();
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    session.send("while :; do sleep 0.05; done\r");
    std::thread::sleep(Duration::from_millis(400));
    ctrl_c(&mut session);

    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(
        seen.contains('❯'),
        "the shell never returned to the prompt after Ctrl-C: {seen:?}"
    );

    session.send("printf 'EXT-%s\\n' $?\r");
    let seen = session.read_until("EXT-130", Duration::from_secs(5));
    assert!(
        seen.contains("EXT-130"),
        "interrupted loop did not report status 130: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

/// A signal handler that writes to a terminal nobody drains must not block: the
/// shell would hang instead of dying. The terminal output queue is filled until
/// a write blocks, then SIGTERM is delivered.
#[test]
fn sigterm_does_not_hang_on_a_full_terminal() {
    let stdin_pair = openpty(None::<&Winsize>, None::<&Termios>).expect("pty in");
    let stdout_pair = openpty(None::<&Winsize>, None::<&Termios>).expect("pty out");
    let (stdin_master, stdin_slave) = (stdin_pair.master, stdin_pair.slave);
    let (stdout_master, stdout_slave) = (stdout_pair.master, stdout_pair.slave);

    let path = CString::new(env!("CARGO_BIN_EXE_sharky")).unwrap();
    let argv = [CString::new("sharky").unwrap()];

    // SAFETY: the child only duplicates descriptors and execs.
    let child = unsafe { libc::fork() };
    assert!(child >= 0, "fork failed");
    if child == 0 {
        unsafe {
            libc::dup2(stdin_slave.as_raw_fd(), 0);
            libc::dup2(stdout_slave.as_raw_fd(), 1);
            libc::dup2(stdin_slave.as_raw_fd(), 2);
            for fd in 3..256 {
                libc::close(fd);
            }
        }
        let env = [
            CString::new("PATH=/run/current-system/sw/bin:/usr/bin:/bin").unwrap(),
            CString::new("TERM=xterm").unwrap(),
            CString::new("SHARKY_CONFIG=/nonexistent/sharky-test").unwrap(),
            CString::new("SHARKY_HISTORY=/dev/null").unwrap(),
            CString::new("HOME=/tmp").unwrap(),
        ];
        let _ = execve(&path, &argv, &env);
        unsafe { libc::_exit(1) }
    }
    drop(stdin_slave);
    drop(stdout_slave);

    // Flood the editor with newlines; every redraw goes to the undrained
    // stdout pty, so its output queue fills and further writes block.
    let mut writer = std::fs::File::from(stdin_master);
    let _ = writer.write_all("\r".repeat(400).as_bytes());
    std::thread::sleep(Duration::from_millis(700));

    nix::sys::signal::kill(Pid::from_raw(child), nix::sys::signal::Signal::SIGTERM).expect("kill");

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut status = None;
    while Instant::now() < deadline {
        match waitpid(Pid::from_raw(child), Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(50)),
            Ok(seen) => {
                status = Some(seen);
                break;
            }
            Err(_) => break,
        }
    }

    // Always reap, so a failing assertion does not leave a stray shell behind.
    let _ = nix::sys::signal::kill(Pid::from_raw(child), nix::sys::signal::Signal::SIGKILL);
    let _ = waitpid(Pid::from_raw(child), None);

    let status = status.expect("shell hung in the SIGTERM handler with a full terminal");
    assert_eq!(
        status,
        WaitStatus::Exited(Pid::from_raw(child), 128 + nix::sys::signal::Signal::SIGTERM as i32),
        "unexpected death: {status:?}"
    );
    drop(stdout_master);
}
