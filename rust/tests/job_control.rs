use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Output};
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

impl Session {
    /// Discards whatever the terminal already delivered, so a later wait for a
    /// needle cannot match output produced before the command that follows.
    fn drain(&self) {
        let mut buffer = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_millis(120);
        while Instant::now() < deadline {
            let mut pollfd = libc::pollfd {
                fd: self.fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd and one valid read buffer.
            let ready = unsafe { libc::poll(&mut pollfd, 1, 20) };
            if ready > 0 {
                let count =
                    unsafe { libc::read(self.fd(), buffer.as_mut_ptr() as *mut _, buffer.len()) };
                if count <= 0 {
                    break;
                }
            }
        }
    }

    /// Runs `command` and collects everything up to a marker printed after it.
    /// The marker text cannot be confused with the echo of the typed line,
    /// which prints the format string rather than the result.
    fn capture(&mut self, command: &str) -> String {
        self.drain();
        self.send(&format!("{command}; printf '\\nCAPTURE-%s\\n' END\r"));
        let seen = self.read_until("CAPTURE-END", Duration::from_secs(8));
        assert!(
            seen.contains("CAPTURE-END"),
            "the shell never finished {command:?}: {seen:?}"
        );
        seen
    }

    /// Waits until the shell finished a job-control stop and is back at the
    /// prompt, so the following command is not typed into a blocked reader.
    fn wait_stopped(&mut self) {
        let seen = self.read_until("Stopped", Duration::from_secs(5));
        assert!(seen.contains("Stopped"), "no stop notice: {seen:?}");
    }
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
    // Keep the job alive long enough that Ctrl-Z always lands while it is
    // running, rather than racing a fixed 300 ms delay. Use $HOME so the typed
    // (and echoed) command stays short even with a long TMPDIR, then wait on
    // observable readiness -- the echoed token and a live `sleep 5`
    // descendant -- before delivering Ctrl-Z.
    session.send(&format!(
        "sh -c 'sleep 5; echo {token} > \"$HOME/resumed.marker\"'\r"
    ));
    let echoed = session.read_until(token, Duration::from_secs(5));
    assert!(
        echoed.contains(token),
        "the shell never echoed the command: {echoed:?}"
    );
    assert!(
        wait_for_descendant(session.child.as_raw(), "sleep 5").is_some(),
        "the job never started"
    );
    session.send("\x1a");
    session.wait_stopped();

    let listing = session.capture("jobs");
    assert!(
        listing.contains("Stopped") && listing.contains(token),
        "job table lost the job: {listing:?}"
    );

    session.capture("fg");

    // The resumed command ran to completion: its marker carries the token.
    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        written.contains(token),
        "fg did not resume the command to completion: marker={written:?}"
    );

    // `fg` removed the job from the table and it finished normally, so `jobs`
    // now reports no job at all (neither Running nor Stopped).
    let listing = session.capture("jobs");
    assert!(
        !listing.contains("Running") && !listing.contains("Stopped"),
        "job table not empty after the resumed job finished: {listing:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

// ---- process-table helpers for the job-table regressions ----

/// One `/proc` entry, as the kernel reports it.
#[derive(Clone, Debug)]
struct Proc {
    pid: i32,
    ppid: i32,
    state: char,
    /// The kernel's short command name, which is the only identification left
    /// for a zombie (its cmdline is empty).
    name: String,
    command: String,
}

fn proc_table() -> Vec<Proc> {
    let mut entries = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return entries;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|text| text.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // The command name may contain spaces and parentheses, so split after
        // the last ')'.
        let Some((before, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let comm = before
            .split_once('(')
            .map(|(_, name)| name.to_string())
            .unwrap_or_default();
        let mut fields = rest.split_whitespace();
        let state = fields.next().and_then(|field| field.chars().next());
        let ppid = fields.next().and_then(|field| field.parse::<i32>().ok());
        let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let command = String::from_utf8_lossy(&command).replace('\0', " ");
        if let (Some(state), Some(ppid)) = (state, ppid) {
            entries.push(Proc {
                pid,
                ppid,
                state,
                name: comm,
                command: command.trim_end().to_string(),
            });
        }
    }
    entries
}

/// Every process reachable from `root` through parent links.
fn descendants(root: i32) -> Vec<Proc> {
    let table = proc_table();
    let mut found: Vec<Proc> = Vec::new();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for entry in &table {
            if entry.ppid == parent && !found.iter().any(|seen| seen.pid == entry.pid) {
                frontier.push(entry.pid);
                found.push(entry.clone());
            }
        }
    }
    found
}

fn wait_for_descendant(shell: i32, needle: &str) -> Option<Proc> {
    for _ in 0..80 {
        if let Some(entry) = descendants(shell)
            .into_iter()
            .find(|entry| entry.command.contains(needle))
        {
            return Some(entry);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn alive(pid: i32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
fn backgrounded_stopped_pipeline_reaps_every_member() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("true | sleep 0.6\r");
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x1a");
    session.wait_stopped();

    session.capture("bg");
    // The next prompt cycle reaps whatever finished while the shell was idle.
    std::thread::sleep(Duration::from_millis(800));
    session.capture(":");

    let listing = session.capture("jobs");
    assert!(
        !listing.contains("Running") && !listing.contains("Stopped"),
        "the pipeline was never reaped: {listing:?}"
    );

    let leftovers: Vec<(i32, char, String)> = descendants(session.child.as_raw())
        .into_iter()
        .filter(|entry| entry.state == 'Z')
        .map(|entry| (entry.pid, entry.state, entry.name))
        .collect();
    assert!(leftovers.is_empty(), "unreaped job members (pid, state, name): {leftovers:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn a_pipeline_that_still_has_a_member_stays_running() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 0.2 | sleep 5\r");
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x1a");
    session.wait_stopped();

    session.capture("bg");
    std::thread::sleep(Duration::from_millis(600));

    // The first member exited; the second one must keep the job in the table.
    let listing = session.capture("jobs");
    assert!(
        listing.contains("Running"),
        "the job was removed while a member was still alive: {listing:?}"
    );

    session.capture("kill %1");

    // The killed member is reaped, not left as a zombie or an orphan.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        session.capture(":");
        let leftovers = descendants(session.child.as_raw());
        let survivor = leftovers
            .iter()
            .find(|entry| entry.command.contains("sleep 5") || entry.state == 'Z');
        if survivor.is_none() || Instant::now() >= deadline {
            assert!(
                survivor.is_none(),
                "the killed pipeline member survived: {leftovers:?}"
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn stopped_subshell_stays_in_the_job_table_and_fg_resumes_it() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    let marker = session.home.join("subshell-resumed");
    let _ = std::fs::remove_file(&marker);

    // Keep the subshell alive long enough that Ctrl-Z always lands while it
    // is running, rather than racing a fixed 300 ms delay. Use $HOME so the
    // typed (and echoed) command stays short even with a long TMPDIR, then
    // wait on observable readiness -- the echoed token and a live `sleep 5`
    // descendant -- before delivering Ctrl-Z.
    session.send("(sleep 5; echo SUBSHELL-DONE > \"$HOME/subshell-resumed\")\r");
    let echoed = session.read_until("SUBSHELL-DONE", Duration::from_secs(5));
    assert!(
        echoed.contains("SUBSHELL-DONE"),
        "the shell never echoed the command: {echoed:?}"
    );
    assert!(
        wait_for_descendant(session.child.as_raw(), "sleep 5").is_some(),
        "the subshell never started"
    );
    session.send("\x1a");
    session.wait_stopped();

    let listing = session.capture("jobs");
    assert!(
        listing.contains("Stopped"),
        "the stopped subshell is missing from the job table: {listing:?}"
    );
    assert!(
        !descendants(session.child.as_raw()).is_empty(),
        "the stopped subshell was orphaned"
    );

    session.capture("fg");

    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        written.contains("SUBSHELL-DONE"),
        "the resumed subshell never finished: {written:?}"
    );
    let listing = session.capture("jobs");
    assert!(!listing.contains("Stopped"), "the finished job is still listed: {listing:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn stopped_redirected_subshell_stays_in_the_job_table() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    let marker = session.home.join("redirect-resumed");
    let _ = std::fs::remove_file(&marker);

    // Keep the subshell alive long enough that Ctrl-Z always lands while it is
    // running, rather than racing a fixed 300 ms delay. Expand $HOME in the
    // shell so the typed (and echoed) command stays short even when TMPDIR is
    // long; echoing a very long line floods the PTY and stalls the shell.
    // Then wait on observable readiness -- the echoed command and a live
    // `sleep 5` descendant -- before delivering Ctrl-Z.
    session.send(
        "(sleep 5; echo REDIRECT-DONE > \"$HOME/redirect-resumed\") > \"$HOME/redirect-output\"\r",
    );
    let echoed = session.read_until("REDIRECT-DONE", Duration::from_secs(5));
    assert!(
        echoed.contains("REDIRECT-DONE"),
        "the shell never echoed the redirected command: {echoed:?}"
    );
    assert!(
        wait_for_descendant(session.child.as_raw(), "sleep 5").is_some(),
        "the redirected subshell never started"
    );
    session.send("\x1a");
    session.wait_stopped();

    let listing = session.capture("jobs");
    assert!(
        listing.contains("Stopped"),
        "the stopped redirected command is missing from the job table: {listing:?}"
    );

    let resumed = session.capture("fg");

    let written = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(
        written.contains("REDIRECT-DONE"),
        "the resumed redirected command never finished: {written:?} after {resumed:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn stopped_subshell_read_does_not_freeze_the_shell() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("(read -r answer)\r");
    std::thread::sleep(Duration::from_millis(300));
    session.send("\x1a");
    session.wait_stopped();

    let seen = session.capture("printf 'SUB-%s\\n' OK");
    assert!(seen.contains("SUB-OK"), "the shell was frozen: {seen:?}");

    session.capture("kill -KILL %1");
    let listing = session.capture("jobs");
    assert!(!listing.contains("Stopped"), "the killed job is still listed: {listing:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn kill_cont_keeps_the_background_job() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 20 &\r");
    session.read_until("[1]", Duration::from_secs(5));
    assert!(
        wait_for_descendant(session.child.as_raw(), "sleep 20").is_some(),
        "the background job did not start"
    );

    session.capture("kill -STOP %1");
    let listing = session.capture("jobs");
    assert!(listing.contains("Stopped"), "kill -STOP did not update the job: {listing:?}");
    let stopped = descendants(session.child.as_raw())
        .into_iter()
        .find(|entry| entry.command.contains("sleep 20"));
    assert!(
        stopped.as_ref().is_some_and(|entry| entry.state == 'T'),
        "the stopped job is not our stopped child: {stopped:?}"
    );

    session.capture("kill -CONT %1");
    let listing = session.capture("jobs");
    assert!(
        listing.contains("Running"),
        "kill -CONT discarded the job: {listing:?}"
    );
    let running = descendants(session.child.as_raw())
        .into_iter()
        .find(|entry| entry.command.contains("sleep 20"));
    assert!(
        running.as_ref().is_some_and(|entry| entry.state != 'T'),
        "the resumed job did not keep running: {running:?}"
    );

    // `wait` must still reach the process, which proves it stayed a child.
    session.capture("kill %1");
    session.capture("wait %1");
    let listing = session.capture("jobs");
    assert!(!listing.contains("Running"), "the finished job is still listed: {listing:?}");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn hangup_kills_background_jobs() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 30 &\r");
    session.read_until("[1]", Duration::from_secs(5));
    let sleep = wait_for_descendant(session.child.as_raw(), "sleep 30")
        .expect("the background job did not start");

    nix::sys::signal::kill(session.child, nix::sys::signal::Signal::SIGHUP).expect("kill -HUP");

    let status = session.exit_status();
    assert_eq!(status, 129, "the shell did not die from SIGHUP");

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && alive(sleep.pid) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let survived = alive(sleep.pid);
    if survived {
        let _ = nix::sys::signal::kill(Pid::from_raw(sleep.pid), nix::sys::signal::Signal::SIGKILL);
    }
    assert!(!survived, "hangup left the background job running: {}", sleep.pid);
}

#[test]
fn normal_exit_leaves_background_jobs_running() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 30 &\r");
    session.read_until("[1]", Duration::from_secs(5));
    let sleep = wait_for_descendant(session.child.as_raw(), "sleep 30")
        .expect("the background job did not start");

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);

    // `huponexit` is off: a normal exit leaves the job alive, reparented.
    assert!(alive(sleep.pid), "a normal exit killed the background job");
    let _ = nix::sys::signal::kill(Pid::from_raw(sleep.pid), nix::sys::signal::Signal::SIGKILL);
}

// ---- `jobs` option parsing: -p, -l and invalid flags ----

fn job_pid(listing: &str) -> Option<i32> {
    for line in listing.lines() {
        let line = line.trim();
        if let Ok(pid) = line.parse::<i32>() {
            return Some(pid);
        }
        // Long listing: `[1]+ 1234 Running ...`.
        let mut fields = line.split_whitespace();
        let _id = fields.next();
        if let Some(pid) = fields.next().and_then(|field| field.parse().ok()) {
            return Some(pid);
        }
    }
    None
}

#[test]
fn jobs_dash_p_prints_one_leader_pid_per_job() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 5 &\r");
    session.read_until("[1]", Duration::from_secs(5));

    let path = session.home.join("jobs-p.txt");
    session.capture(&format!("jobs -p > {}", path.display()));
    let listing = std::fs::read_to_string(&path).unwrap_or_default();
    let pid = job_pid(&listing).unwrap_or_else(|| panic!("no pid in {listing:?}"));
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "jobs -p printed a dead pid {pid}: {listing:?}"
    );
    assert!(
        !listing.contains("Running") && !listing.contains("sleep 5"),
        "jobs -p printed the full listing: {listing:?}"
    );

    session.capture("kill %1");
    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn jobs_dash_l_adds_the_pid_column() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 5 &\r");
    session.read_until("[1]", Duration::from_secs(5));

    let path = session.home.join("jobs-l.txt");
    session.capture(&format!("jobs -l > {}", path.display()));
    let listing = std::fs::read_to_string(&path).unwrap_or_default();
    let pid = job_pid(&listing).unwrap_or_else(|| panic!("no pid in {listing:?}"));
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "jobs -l printed a dead pid {pid}: {listing:?}"
    );
    assert!(
        listing.contains("Running") && listing.contains("sleep 5"),
        "jobs -l dropped the state or the command: {listing:?}"
    );

    session.capture("kill %1");
    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn jobs_rejects_an_unknown_option_with_status_2() {
    let mut session = Session::start();
    session.read_until("❯", Duration::from_secs(5));

    session.send("sleep 5 &\r");
    session.read_until("[1]", Duration::from_secs(5));

    let listing = session.capture("jobs -z; echo status=$?");
    assert!(
        listing.contains("jobs: -z: invalid option"),
        "missing diagnostic: {listing:?}"
    );
    assert!(listing.contains("usage"), "missing usage: {listing:?}");
    assert!(listing.contains("status=2"), "wrong status: {listing:?}");

    session.capture("kill %1");
    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

// ---- exit statuses of background jobs that already left the job table ----
//
// Bash keeps a finished job answerable, so a `wait` that arrives after the Done
// notice still reports the job's real status, and repeating it reports the same
// status again (measured against bash 5.3.20). These tests pin that behavior. A
// bare `wait` forgets the statuses; bash itself leaves one answerable in a few
// corner cases, and `bare_wait_forgets_reaped_jobs` pins the simpler rule this
// shell documents instead.

fn sharky_c(script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", "/nonexistent/sharky-test")
        .output()
        .expect("failed to run sharky")
}

fn combined(out: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn wait_after_the_done_notice_reports_the_job_status() {
    let mut session = Session::start();
    let prompt = session.read_until("❯", Duration::from_secs(5));
    assert!(prompt.contains('❯'), "no prompt: {prompt:?}");

    // The `$!` ends the line so history expansion never sees a `!` followed by
    // a separator; that variant is a different bug.
    session.send("(exit 7) & p=$!\r");
    let seen = session.read_until("[1]", Duration::from_secs(5));
    assert!(seen.contains("[1]"), "no job id: {seen:?}");

    session.send("sleep 0.4\r");
    let seen = session.read_until("Exit 7", Duration::from_secs(5));
    assert!(seen.contains("Exit 7"), "no job notice: {seen:?}");

    let first = session.capture("wait \"$p\"; echo first=$?");
    assert!(
        first.contains("first=7"),
        "wait after the Done notice did not report the job status: {first:?}"
    );

    let second = session.capture("wait \"$p\"; echo second=$?");
    assert!(
        second.contains("second=7"),
        "a repeated wait lost the status: {second:?}"
    );

    let by_job = session.capture("wait %1; echo job=$?");
    assert!(
        by_job.contains("job=7"),
        "wait %1 lost the status of the reaped job: {by_job:?}"
    );

    session.capture("wait");
    let forgotten = session.capture("wait \"$p\"; echo gone=$?");
    assert!(
        forgotten.contains("gone=127"),
        "a bare wait did not forget the reaped job: {forgotten:?}"
    );

    session.send("exit\r");
    assert_eq!(session.exit_status(), 0);
}

#[test]
fn wait_reports_a_reaped_job_again() {
    let out = sharky_c("(exit 7) & p=$!; sleep 0.3; wait \"$p\"; echo a=$?; wait \"$p\"; echo b=$?");
    let (stdout, stderr) = combined(&out);
    assert!(stdout.contains("a=7"), "first wait wrong ({stdout:?}, {stderr:?})");
    assert!(
        stdout.contains("b=7"),
        "second wait lost the reaped status: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn bare_wait_forgets_reaped_jobs() {
    let out = sharky_c("(exit 7) & p=$!; sleep 0.3; wait; wait \"$p\"; echo b=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("b=127"),
        "wait after a bare wait is not a child error: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("is not a child"),
        "no not-a-child diagnostic: {stderr:?}"
    );
}

#[test]
fn wait_for_a_foreign_pid_is_not_a_child() {
    let out = sharky_c("wait 2147483646; echo st=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("st=127"),
        "wait on a foreign pid did not report 127: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("is not a child"),
        "no not-a-child diagnostic: {stderr:?}"
    );
}

// Bash parses an argument that starts with `-` as an option, so `wait -1` is an
// invalid option and not a pid: it must not touch the job table, because
// consuming the child's status there loses it for the later `wait $p`. Measured
// against bash 5.3.20.
#[test]
fn wait_with_a_negative_pid_is_an_invalid_option() {
    let out = sharky_c("wait -1; echo st=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("st=2"),
        "wait -1 did not report 2: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("wait: -1: invalid option"),
        "no invalid-option diagnostic: {stderr:?}"
    );
    assert!(stderr.contains("usage"), "no usage line: {stderr:?}");
}

#[test]
fn wait_with_a_negative_pid_keeps_the_job_status() {
    let out = sharky_c(
        "(exit 7) & p=$!; sleep 0.3; wait -1; echo first=$?; wait \"$p\"; echo job=$?",
    );
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("first=2"),
        "wait -1 did not fail: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("job=7"),
        "wait -1 stole the job status: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn wait_with_an_unsupported_option_is_an_invalid_option() {
    // Sharky implements none of bash's wait options, so every one of them has
    // to be a clean error instead of a pid lookup.
    let out = sharky_c("wait -n; echo n=$?; wait -f; echo f=$?; wait -p var; echo p=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("n=2") && stdout.contains("f=2") && stdout.contains("p=2"),
        "an unsupported wait option was not rejected: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("wait: -n: invalid option")
            && stderr.contains("wait: -f: invalid option")
            && stderr.contains("wait: -p: invalid option"),
        "missing invalid-option diagnostics: {stderr:?}"
    );
}

#[test]
fn wait_for_pid_zero_is_not_a_child() {
    let out = sharky_c("wait 0; echo st=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("st=127"),
        "wait 0 did not report 127: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stderr.contains("is not a child"),
        "no not-a-child diagnostic: {stderr:?}"
    );
}

#[test]
fn wait_for_several_pids_reports_the_last_status() {
    let out = sharky_c(
        "(exit 3) & p1=$!; (exit 5) & p2=$!; sleep 0.3; \
         wait \"$p1\" \"$p2\"; echo forward=$?; wait \"$p1\" \"$p2\"; echo again=$?",
    );
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("forward=5"),
        "multi-pid wait did not report the last status: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("again=5"),
        "repeated multi-pid wait lost the retained status: stdout={stdout:?} stderr={stderr:?}"
    );

    let out = sharky_c("(exit 3) & p1=$!; (exit 5) & p2=$!; sleep 0.3; wait \"$p2\" \"$p1\"; echo rev=$?");
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("rev=3"),
        "multi-pid wait ignored the argument order: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn kill_zero_checks_whether_the_process_exists() {
    let out = sharky_c(
        "sleep 0.2 & p=$!; kill -0 \"$p\"; echo alive=$?; kill -0 2147483646; echo missing=$?; wait \"$p\"",
    );
    let (stdout, stderr) = combined(&out);
    assert!(
        stdout.contains("alive=0"),
        "kill -0 rejected a live job: stdout={stdout:?} stderr={stderr:?}"
    );
    assert!(
        stdout.contains("missing=1"),
        "kill -0 accepted a missing pid: stdout={stdout:?} stderr={stderr:?}"
    );
}

#[test]
fn kill_then_wait_reports_the_signal_status_every_time() {
    // The kill races the child's own bookkeeping, so the status is read back
    // many times: one lost signal makes the loop produce a 0 among the 143s.
    // The job's stdio goes to /dev/null: a grandchild that survives the killed
    // job would otherwise keep the test's stdout pipe open and delay the read
    // until its sleep runs out, which is not what this test measures.
    const LOOPS: usize = 20;
    let script = format!(
        "i=0; while [ \"$i\" -lt {LOOPS} ]; do sleep 30 >/dev/null 2>&1 & p=$!; kill \"$p\"; wait \"$p\"; echo st=$?; i=$((i+1)); done"
    );

    let runs: Vec<(String, String)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..3)
            .map(|_| scope.spawn(|| combined(&sharky_c(&script))))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("worker panicked"))
            .collect()
    });

    for (stdout, stderr) in runs {
        let killed = stdout.matches("st=143").count();
        assert_eq!(
            killed, LOOPS,
            "a killed job did not report 143 every time: stdout={stdout:?} stderr={stderr:?}"
        );
    }
}

#[test]
fn wait_rejects_options_with_honest_usage() {
    let out = sharky_c("wait -n");
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("invalid option"), "{stderr}");
    assert!(stderr.contains("usage: wait [id ...]"), "{stderr}");
    assert!(!stderr.contains("-fn"), "{stderr}");
    assert!(!stderr.contains("-p"), "{stderr}");
}
