use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty};
use nix::sys::termios::Termios;
use nix::sys::wait::waitpid;
use nix::unistd::{Pid, execve};

struct Session {
    child: Pid,
    master: std::os::fd::OwnedFd,
}

impl Session {
    fn start() -> Self {
        let path = CString::new(env!("CARGO_BIN_EXE_sharky")).unwrap();
        let argv = [CString::new("sharky").unwrap()];
        let env = [
            CString::new("SHARKY_CONFIG=/nonexistent/sharky-test").unwrap(),
            CString::new("HOME=/tmp").unwrap(),
            CString::new("PATH=/usr/bin:/bin").unwrap(),
            CString::new("TERM=xterm").unwrap(),
        ];

        // SAFETY: the child only execs; every string it needs exists before the fork.
        match unsafe { forkpty(None::<&Winsize>, None::<&Termios>) }.expect("forkpty") {
            ForkptyResult::Child => {
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
            Ok(nix::sys::wait::WaitStatus::Exited(_, code)) => code,
            _ => 0,
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.child, nix::sys::signal::Signal::SIGKILL);
        let _ = waitpid(self.child, Some(nix::sys::wait::WaitPidFlag::WNOHANG));
    }
}

#[test]
fn background_job_runs_and_reports_done() {
    let mut session = Session::start();
    let prompt = session.read_until("❯", Duration::from_secs(5));
    assert!(prompt.contains('❯'), "no prompt: {prompt:?}");

    session.send("sleep 0.3 &\r");
    let seen = session.read_until("[1]", Duration::from_secs(5));
    assert!(seen.contains("[1]"), "no job id: {seen:?}");

    session.send("jobs\r");
    let seen = session.read_until("Running", Duration::from_secs(5));
    assert!(seen.contains("Running"), "job not listed as running: {seen:?}");

    std::thread::sleep(Duration::from_millis(600));
    session.send("jobs\r");
    let seen = session.read_until("Done", Duration::from_secs(5));
    assert!(seen.contains("Done"), "job not reaped: {seen:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn ctrl_z_stops_a_job_and_fg_resumes_it() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 60\r");
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x1a");

    let seen = session.read_until("Stopped", Duration::from_secs(5));
    assert!(seen.contains("Stopped"), "ctrl-z did not stop the job: {seen:?}");

    session.send("jobs\r");
    let seen = session.read_until("sleep 60", Duration::from_secs(5));
    assert!(seen.contains("Stopped"), "job table lost the job: {seen:?}");

    session.send("fg\r");
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x03");

    session.send("jobs\r");
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "missing prompt after fg: {seen:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}
