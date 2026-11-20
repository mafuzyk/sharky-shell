use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::libc;
use nix::pty::{ForkptyResult, Winsize, forkpty};
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
        // Every session gets its own HOME/XDG_DATA_HOME so parallel tests never
        // share a history (or z) file under a common /tmp path.
        static HOME_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = format!(
            "sharky-jc-{}-{}-{}",
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
                // A terminal login starts with only the standard descriptors;
                // fds inherited from the test harness would otherwise occupy
                // the numbers a redirect can reuse.
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
            // Report a signal death the way a shell does, instead of collapsing
            // it to 0 and letting `assert_eq!(..., 0)` pass on a crashed shell.
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
fn scoped_stdin_redirect_keeps_the_child_foreground() {
    let (foreground, background) = foreground_marker("stdin", "f < /dev/null");
    assert!(
        foreground && !background,
        "a scoped stdin redirect lost the foreground group (foreground={foreground}, background={background})"
    );
}

// The shell's terminal must not be a descriptor number a redirect can take:
// `exec 3> file` keeps fd 3 for the rest of the session.
#[test]
fn persistent_numbered_redirect_keeps_the_child_foreground() {
    let target = std::env::temp_dir().join(format!("sharky-exec3-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);
    let action = format!("exec 3> {}; f < /dev/null", target.display());
    let (foreground, background) = foreground_marker("exec3", &action);
    let _ = std::fs::remove_file(&target);

    assert!(
        foreground && !background,
        "a persistent fd 3 redirect lost the foreground group (foreground={foreground}, background={background})"
    );
}

#[test]
fn scoped_numbered_redirect_keeps_the_child_foreground() {
    let target = std::env::temp_dir().join(format!("sharky-fd3-{}", std::process::id()));
    let _ = std::fs::remove_file(&target);
    let action = format!("f 3> {}", target.display());
    let (foreground, background) = foreground_marker("fd3", &action);
    let _ = std::fs::remove_file(&target);

    assert!(
        foreground && !background,
        "a scoped fd 3 redirect lost the foreground group (foreground={foreground}, background={background})"
    );
}

// Runs `action` with a function that records whether the child it starts is in
// the terminal's foreground process group.
fn foreground_marker(name: &str, action: &str) -> (bool, bool) {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);

    let base = std::env::temp_dir().join(format!(
        "sharky-fg-{}-{name}-{count}",
        std::process::id()
    ));
    let foreground = base.with_extension("foreground");
    let background = base.with_extension("background");
    let _ = std::fs::remove_file(&foreground);
    let _ = std::fs::remove_file(&background);

    let mut session = Session::start();
    let prompt = session.read_until("❯", Duration::from_secs(5));
    assert!(prompt.contains('❯'), "no prompt: {prompt:?}");

    let sh = common::resolve("sh");
    session.send(&format!(
        "f() {{ {} -c 'read -r st < /proc/$$/stat; set -- $st; [ \"$5\" = \"$8\" ] && : > {} || : > {}'; }}\r",
        sh.display(),
        foreground.display(),
        background.display()
    ));
    session.read_until("❯", Duration::from_secs(5));

    session.send(&format!("{action}\r"));
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt after {action:?}: {seen:?}");

    let had_foreground = foreground.exists();
    let had_background = background.exists();
    let _ = std::fs::remove_file(&foreground);
    let _ = std::fs::remove_file(&background);

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);

    (had_foreground, had_background)
}

#[test]
fn ctrl_z_stops_a_job_and_fg_resumes_it() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    let token = "SHARKY_JOBCTRL_RESUME";
    let marker = session.home.join("resumed.marker");
    let _ = std::fs::remove_file(&marker);

    // The only side effect happens after the sleep, i.e. after the job has
    // been resumed by `fg`. Its presence proves the job actually ran again.
    session.send(&format!(
        "sh -c 'sleep 1; echo {token} > {}'\r",
        marker.display()
    ));
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x1a");

    let seen = session.read_until("Stopped", Duration::from_secs(5));
    assert!(seen.contains("Stopped"), "ctrl-z did not stop the job: {seen:?}");

    session.send("jobs\r");
    // The leftover from the status line has no "Stopped", so this only returns
    // once the fresh `jobs` listing (carrying the token) has been read.
    let seen = session.read_until("Stopped", Duration::from_secs(5));
    assert!(
        seen.contains("Stopped") && seen.contains(token),
        "job table lost the job: {seen:?}"
    );

    session.send("fg\r");
    // Give the resumed command time to finish its sleep and write the marker.
    std::thread::sleep(Duration::from_millis(1500));

    session.send("jobs\r");
    let seen = session.read_until("❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "missing prompt after fg: {seen:?}");

    // The resumed command ran to completion: its marker carries the token.
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        written.contains(token),
        "fg did not resume the command to completion: marker={written:?}"
    );

    // `fg` removed the job from the table and it finished normally, so `jobs`
    // now reports no job at all (neither Running nor Stopped).
    assert!(
        !seen.contains("Running") && !seen.contains("Stopped"),
        "job table not empty after the resumed job finished: {seen:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}
