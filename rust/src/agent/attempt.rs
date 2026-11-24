//! One task attempt: spawn, child preparation, status record, result assembly.
//!
//! This is the half of `op:"parallel"` that a second caller shares
//! (`docs/workflow-executor-design.md` §4.1): every caller interprets a task the
//! same way, so the fork/isolate/evaluate sequence and the reaped-attempt
//! result exist exactly once, here. `parallel.rs` keeps what is its own — the
//! window loop, its slots, its policy, fail-fast and the aggregate.
//!
//! The isolation model is the parallel design's D2 (§3): an attempt is a `fork`
//! of the session process that runs the command on its own copy-on-write
//! `Shell`, writes its output to its own captures and reports the detail an
//! exit status cannot carry (cwd, signal, env delta) through one small status
//! record.
//!
//! Descriptor discipline (parallel design §6): captures are created in the
//! parent, above fd 10 and `O_CLOEXEC`, one set per attempt. The task child
//! keeps only its own three (stdout, stderr, status record) and closes every
//! descriptor the caller hands it before it evaluates anything.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::Instant;

use nix::libc;
use nix::sys::resource::{Resource, setrlimit};
use nix::sys::signal::SigSet;
use nix::sys::wait::WaitStatus;
use nix::unistd::{ForkResult, Pid, fork, getegid, geteuid, getpgrp, getpid, setpgid, setsid};

use super::capture::{self, Capture, CaptureContent};
use super::json::{
    ErrorCode, Limits, TaskError, TaskResult, TaskState, Termination, Value,
};
use super::projection::{build_projection_summary, read_capture_for_projection};
use super::protocol::{SandboxSpec, TaskRequest};
use super::store::StoredStream;
use super::supervisor;
use crate::shell::Shell;

/// Status of a timeout-ended task (parallel design §2.4).
pub(crate) const TIMEOUT_STATUS: i64 = 124;

/// Status of a task that never ran, and of a queued task a deadline dropped.
const SPAWN_ERROR_STATUS: i64 = 126;
pub(crate) const CANCELLED_STATUS: i64 = 130;

/// Status of a task whose outcome was lost (parallel design §3.7). Non-zero on
/// purpose: a group that could not read a task's status must never look
/// successful.
const UNKNOWN_STATUS: i64 = 1;

/// Bound on one task status record. It is independent of
/// `limits.max_line_bytes`, which bounds protocol lines: the record carries the
/// env delta a task asked for, and that delta can be far larger than a line.
/// A record that would exceed this bound drops the delta and writes
/// `env_delta_omitted` instead, so the response never loses the field silently.
const RECORD_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// The reason a requested env delta is missing from a task response.
const ENV_DELTA_TOO_LARGE: &str = "record_too_large";

/// The reason reported when a task could not get a process group of its own.
const GROUP_FAILURE_MESSAGE: &str = "could not create process group";

// --------------------------------------------------------------- one attempt

/// One attempt's own captures. Created by the parent when the attempt starts,
/// read back and dropped when the attempt is reaped (parallel design §6).
pub(crate) struct Captures {
    pub(crate) stdout: Capture,
    pub(crate) stderr: Capture,
    record: Capture,
}

impl Captures {
    /// The descriptors a child of a sibling attempt must not inherit, in the
    /// order the caller hands them to [`spawn_attempt`].
    pub(crate) fn fds(&self) -> [RawFd; 3] {
        [self.stdout.fd(), self.stderr.fd(), self.record.fd()]
    }
}

/// A live attempt: the leader whose process group supervision records, its
/// captures, and the parent's spawn instant.
///
/// The duration is measured here, spawn to reap, on the parent's monotonic
/// clock: the child measures only its own `session_eval`, which ends before the
/// task's EXIT action runs, so its number would hide the trap — `trap 'sleep 1'
/// EXIT` would report ~0 ms for a task that really ran a second. The parent
/// sees the whole life of the task, trap included.
pub(crate) struct Attempt {
    pub(crate) leader: i32,
    pub(crate) captures: Captures,
    pty_master: Option<OwnedFd>,
    started: Instant,
}

/// What finishing one attempt yields: the task's result plus whether the shared
/// response budget forced a cut in one of its streams. The caller owns the
/// group-level truncation flag, the policy and fail-fast (parallel design
/// §6/§7).
pub(crate) struct AttemptOutcome {
    pub(crate) result: TaskResult,
    pub(crate) truncated: bool,
    pub(crate) raw_stdout: StoredStream,
    pub(crate) raw_stderr: StoredStream,
}

impl Attempt {
    /// Descriptors owned by this live attempt that sibling children must close.
    pub(crate) fn fds(&self) -> Vec<RawFd> {
        let mut fds = self.captures.fds().to_vec();
        if let Some(master) = &self.pty_master {
            fds.push(master.as_raw_fd());
        }
        fds
    }

    /// Non-blocking PTY master descriptor while a PTY task is alive.
    pub(crate) fn pty_fd(&self) -> Option<RawFd> {
        self.pty_master.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Drains any pending bytes from the non-blocking PTY master into `captures.stdout`.
    pub(crate) fn drain_pty(&mut self) {
        let Some(master) = &self.pty_master else {
            return;
        };
        let mut buf = [0u8; 8192];
        loop {
            // SAFETY: `master` is a valid open fd in this process and `buf` is writable.
            let n = unsafe {
                libc::read(
                    master.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                )
            };
            if n > 0 {
                let _ = self.captures.stdout.append(&buf[..n as usize]);
                continue;
            }
            if n == 0 {
                self.pty_master = None;
                break;
            }
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if err.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            self.pty_master = None;
            break;
        }
    }

    /// Reads a reaped attempt's captures and records its result, charging
    /// `budget`, the caller's remaining response bytes. `stop` is the
    /// supervisor's verdict for a task it terminated; it wins over the wait
    /// status, exactly as `exec` reports cancel/timeout over the command's own
    /// exit (parallel design §5). The budget is shared by the caller across
    /// attempts, so the read order here is the charge order everywhere.
    pub(crate) fn finish(
        mut self,
        task: &TaskRequest,
        session_cwd: &str,
        status: Option<WaitStatus>,
        stop: Option<supervisor::TaskStop>,
        limits: &Limits,
        budget: &mut usize,
    ) -> Result<AttemptOutcome, String> {
        self.drain_pty();
        drop(self.pty_master.take());
        let mut truncated = false;
        let (stdout, out_proj, raw_stdout) = read_capture_for_projection(
            &self.captures.stdout,
            task.output.as_ref(),
            limits.max_capture_bytes,
            budget,
            &mut truncated,
        )?;
        let (stderr, err_proj, raw_stderr) = read_capture_for_projection(
            &self.captures.stderr,
            task.output.as_ref(),
            limits.max_capture_bytes,
            budget,
            &mut truncated,
        )?;
        let projection = build_projection_summary(out_proj, err_proj);
        let record = read_record(&self.captures.record);

        let elapsed_us = self
            .started
            .elapsed()
            .as_micros()
            .min(i64::MAX as u128) as i64;

        let (task_status, termination, signal, state, error) = if let Some(stop) = stop {
            match stop.outcome {
                supervisor::Outcome::Timeout => (
                    TIMEOUT_STATUS,
                    Termination::Timeout,
                    None,
                    TaskState::Timeout,
                    None,
                ),
                supervisor::Outcome::Cancelled => (
                    143,
                    Termination::Cancelled,
                    None,
                    TaskState::Cancelled,
                    None,
                ),
            }
        } else if let Some(WaitStatus::Signaled(_, number, _)) = status {
            let number = number as i32;
            (
                128 + i64::from(number),
                Termination::Signaled,
                Some(signal_name(number)),
                TaskState::Completed,
                None,
            )
        } else if let Some(record) = &record {
            if record.termination == Some(Termination::SpawnError) {
                (
                    SPAWN_ERROR_STATUS,
                    Termination::SpawnError,
                    None,
                    TaskState::SpawnError,
                    Some(TaskError {
                        code: ErrorCode::SpawnFailed,
                        message: record
                            .message
                            .clone()
                            .unwrap_or_else(|| "the task could not be started".to_string()),
                    }),
                )
            } else {
                (
                    record.status,
                    record.termination.unwrap_or(Termination::Exited),
                    record.signal.clone(),
                    TaskState::Completed,
                    None,
                )
            }
        } else if let Some(WaitStatus::Exited(_, code)) = status {
            (
                i64::from(code),
                Termination::Exited,
                None,
                TaskState::Completed,
                None,
            )
        } else {
            // The status was consumed elsewhere or the child vanished: report
            // it as unknown, never as a silent success (parallel design §3.7).
            // The non-zero status is what keeps the aggregate honest.
            (
                UNKNOWN_STATUS,
                Termination::Exited,
                None,
                TaskState::Completed,
                Some(TaskError {
                    code: ErrorCode::Internal,
                    message: String::from("the task status was consumed elsewhere"),
                }),
            )
        };

        let mut result = TaskResult::new(
            task.id.clone(),
            record
                .as_ref()
                .and_then(|record| record.cwd.clone())
                .unwrap_or_else(|| effective_cwd(task, session_cwd)),
        );
        result.state = state;
        result.termination = termination;
        result.status = task_status;
        result.signal = signal;
        result.reason = stop.and_then(|stop| stop.reason);
        result.error = error;
        result.duration_us = elapsed_us;
        result.duration_ms = elapsed_us / 1000;
        result.stdout = stdout;
        result.stderr = stderr;
        result.projection = projection;
        if task.env_delta {
            result.env_delta_omitted = record
                .as_ref()
                .and_then(|record| record.env_delta_omitted.clone());
            result.env_delta = record.and_then(|record| record.env_delta);
        }
        result.pgid = Some(i64::from(self.leader));
        Ok(AttemptOutcome {
            result,
            truncated,
            raw_stdout,
            raw_stderr,
        })
    }
}

// ------------------------------------------------------------------- spawning

/// Forks one task child. `Ok` returns the attempt in the parent; the child
/// never returns.
///
/// `fork` is called with the supervisor `Mutex` **not** held: the caller is
/// outside every `with_state` closure, so a child can never inherit a locked
/// mutex.
pub(crate) fn spawn_attempt(
    shell: &mut Shell,
    task: &TaskRequest,
    null_fd: RawFd,
    close_fds: &[RawFd],
) -> Result<Attempt, String> {
    let started = Instant::now();
    let stdout = Capture::create("agent-task-out")
        .map_err(|error| format!("cannot create the task stdout capture: {error}"))?;
    let stderr = Capture::create("agent-task-err")
        .map_err(|error| format!("cannot create the task stderr capture: {error}"))?;
    let record = Capture::create("agent-task-status")
        .map_err(|error| format!("cannot create the task status capture: {error}"))?;
    let payload = match &task.stdin {
        None => None,
        Some(bytes) => {
            let capture = Capture::create("agent-task-in")
                .map_err(|error| format!("cannot create the task stdin payload: {error}"))?;
            capture
                .fill(bytes)
                .map_err(|error| format!("cannot fill the task stdin payload: {error}"))?;
            Some(capture)
        }
    };

    let (pty_master, pty_slave) = if task.tty {
        let pty = nix::pty::openpty(
            Some(&nix::pty::Winsize {
                ws_row: 24,
                ws_col: 80,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .map_err(|error| format!("cannot open pty: {error}"))?;
        let mut term = nix::sys::termios::tcgetattr(&pty.slave)
            .map_err(|error| format!("cannot read pty termios: {error}"))?;
        nix::sys::termios::cfmakeraw(&mut term);
        nix::sys::termios::tcsetattr(&pty.slave, nix::sys::termios::SetArg::TCSANOW, &term)
            .map_err(|error| format!("cannot configure pty raw mode: {error}"))?;

        let master_fd = capture::duplicate_above_min(pty.master.as_raw_fd(), capture::FD_FLOOR)
            .map_err(|error| format!("cannot lift pty master fd: {error}"))?;
        // SAFETY: `master_fd` owns an open non-stdio descriptor returned by fcntl.
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };
        set_nonblock(master.as_raw_fd())
            .map_err(|error| format!("cannot configure pty nonblocking mode: {error}"))?;

        let slave_fd = capture::duplicate_above_min(pty.slave.as_raw_fd(), capture::FD_FLOOR)
            .map_err(|error| format!("cannot lift pty slave fd: {error}"))?;
        // SAFETY: `slave_fd` owns an open non-stdio descriptor returned by fcntl.
        let slave = unsafe { OwnedFd::from_raw_fd(slave_fd) };
        drop(pty);
        (Some(master), Some(slave))
    } else {
        (None, None)
    };

    // A buffered byte of the parent must not surface inside a task's capture.
    capture::flush_stdio();
    let mask = crate::signal::block_all();
    // SAFETY: the parent restores the mask in both returning branches; the
    // child restores it before it runs any shell code and never unwinds.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => {
            crate::signal::restore_mask(mask);
            drop(pty_slave);
            if !task.tty {
                let _parent_group = setpgid(child, child);
            }
            Ok(Attempt {
                leader: child.as_raw(),
                captures: Captures {
                    stdout,
                    stderr,
                    record,
                },
                pty_master,
                started,
            })
        }
        Ok(ForkResult::Child) => {
            let pty_master_fd = pty_master.as_ref().map(AsRawFd::as_raw_fd);
            let pty_slave_fd = pty_slave.as_ref().map(AsRawFd::as_raw_fd);
            run_child(Child {
                shell,
                task,
                null_fd,
                stdout: stdout.fd(),
                stderr: stderr.fd(),
                record: record.fd(),
                payload: payload.as_ref().map(Capture::fd),
                pty_master: pty_master_fd,
                pty_slave: pty_slave_fd,
                close_fds,
                mask,
            })
        }
        Err(error) => {
            crate::signal::restore_mask(mask);
            Err(format!("fork: {error}"))
        }
    }
}

fn set_nonblock(fd: RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is open in this process.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fd` is open in this process and `flags | O_NONBLOCK` is valid.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// What a forked task child needs. A struct so the child entry point stays one
/// function with one documented order (parallel design §11, third risk).
struct Child<'a> {
    shell: &'a mut Shell,
    task: &'a TaskRequest,
    null_fd: RawFd,
    stdout: RawFd,
    stderr: RawFd,
    record: RawFd,
    payload: Option<RawFd>,
    pty_master: Option<RawFd>,
    pty_slave: Option<RawFd>,
    /// Descriptors of the session and of the other running tasks.
    close_fds: &'a [RawFd],
    mask: SigSet,
}

/// The task child: isolate, prepare, evaluate, report, `_exit`.
fn run_child(child: Child<'_>) -> ! {
    let Child {
        shell,
        task,
        null_fd,
        stdout,
        stderr,
        record,
        payload,
        pty_master,
        pty_slave,
        close_fds,
        mask,
    } = child;

    if let Some(fd) = pty_master {
        close_fd(fd);
    }

    if pty_slave.is_some() {
        let _ = setsid();
    } else {
        let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
    }
    // No protocol pumping, no control descriptors, no self-pipe: the existing
    // subshell guarantee.
    supervisor::child_guard();
    // Session flags, `job_pgid`, trap reset and the EXIT action snapshot.
    shell.enter_task_child();
    // The group is final after `enter_task_child`'s own attempt: confirm it now.
    let ungrouped = group_failure();
    crate::signal::restore_mask(mask);
    crate::signal::clear_interrupt();

    // The task's own streams: fd 1/2 are its captures (or PTY slave on fd 1 when
    // `tty` is requested) and fd 0 is the payload file, PTY slave, or `/dev/null`.
    let isolated = if let Some(slave) = pty_slave {
        // SAFETY: child owns `slave` and called `setsid()` above.
        unsafe {
            libc::ioctl(slave, libc::TIOCSCTTY as _, 0);
        }
        capture::dup2_onto(slave, 1).and_then(|()| {
            capture::dup2_onto(stderr, 2)?;
            match payload {
                Some(fd) => capture::dup2_onto(fd, 0),
                None => capture::dup2_onto(slave, 0),
            }
        })
    } else {
        capture::dup2_onto(stdout, 1).and_then(|()| {
            capture::dup2_onto(stderr, 2)?;
            match payload {
                Some(fd) => capture::dup2_onto(fd, 0),
                None => capture::dup2_onto(null_fd, 0),
            }
        })
    };
    if let Err(error) = isolated {
        child_fail(
            record,
            &format!("cannot isolate the task descriptors: {error}"),
        );
    }
    // Close the original high copies of stdout/stderr now that fd 1 and 2 carry
    // them, so the child does not expose duplicate capture descriptors.
    if stdout != 1 {
        close_fd(stdout);
    }
    if stderr != 2 {
        close_fd(stderr);
    }
    if let Some(slave) = pty_slave
        && slave > 2
    {
        close_fd(slave);
    }
    // Nothing of the session or of another task may stay reachable from inside
    // the task: a leaked descriptor would keep an unlinked capture alive and
    // could keep another task's result file open (parallel design §6).
    for fd in close_fds {
        close_fd(*fd);
    }
    if let Some(fd) = payload {
        close_fd(fd);
    }
    // Move the status record out of the low descriptor range (to >= 900) so
    // user redirections on low fds cannot clobber it, and keep its identity so
    // even an explicit redirect onto that high fd cannot divert internal JSON.
    let record = match capture::duplicate_above_min(record, 900) {
        Ok(high) => {
            close_fd(record);
            high
        }
        Err(_) => record,
    };
    let record_identity = capture::identity(record).ok();
    // The group verdict now that stderr is the task's own capture.
    if let Some(reason) = ungrouped {
        child_fail(record, reason);
    }

    // `cwd` and `env` are applied here, in the child, on top of the snapshot,
    // so a task that fails halfway still cannot touch the session (parallel
    // design §2.1).
    if let Some(cwd) = &task.cwd
        && let Err(error) = shell.enter_task_directory(cwd)
    {
        child_fail(
            record,
            &format!("cannot change directory to {cwd}: {error}"),
        );
    }
    for (name, value) in &task.env {
        shell.set_task_env(name, value.clone());
    }

    if let Some(spec) = &task.sandbox {
        apply_sandbox(spec, record);
    }

    // The action the task inherited; `finish_task` runs the one the task
    // itself set, and an unchanged action is the session's and must not fire
    // once per task (parallel design §1.10/§7).
    let inherited = shell.snapshot_exit_trap();
    let before = task.env_delta.then(|| shell.snapshot_env());

    let mut status = match &task.target {
        super::protocol::ExecTarget::Command(command) => {
            // An empty or whitespace-only command is a no-op with status 0, the same
            // answer `exec` gives it. It must not be evaluated: `session_eval` ends with
            // `run_program`'s status and `run_program` of an empty program reports the
            // inherited `$?`, so a task with `"   "` after an `exec false` would report
            // 1 for doing nothing (parallel design §2.1).
            if command.trim().is_empty() {
                0
            } else {
                let outcome = shell.session_eval(command);
                shell.close_substitutions_from(0);
                match &outcome {
                    Ok(status) => *status,
                    Err(error) => {
                        // Parse errors are the task's status 2, with bash's message shape,
                        // and nothing of the command ran (parallel design §7).
                        let line = format!("sharky: {}\n", error.message);
                        let _ = crate::term::write_stderr(&line);
                        2
                    }
                }
            }
        }
        super::protocol::ExecTarget::Argv(argv) => {
            let status = shell.session_exec_argv(argv);
            shell.close_substitutions_from(0);
            status
        }
    };
    // `exit` inside a task ends the task, never the session (parallel design
    // §7).
    if let Some(code) = shell.exit_requested.take() {
        status = code;
    }
    status = shell.finish_task(status, inherited.as_deref());

    let cwd = shell.cwd.to_string_lossy().into_owned();
    let delta = before.map(|before| shell.env_delta(&before));
    if record_identity.is_none_or(|expected| capture::identity(record).ok() == Some(expected)) {
        StatusRecord {
            status: i64::from(status),
            termination: "exited",
            cwd,
            env_delta: delta,
            message: None,
        }
        .write(record);
    }
    capture::flush_stdio();
    // SAFETY: a forked child has nothing to unwind; the parent reaps it.
    unsafe { libc::_exit(status & 0xff) }
}

/// Whether a forked task child failed to become the leader of its own process
/// group, and why. The check is the child's own `getpgrp()`, not the result of
/// its `setpgid`: that call can lose the race with the parent's copy and still
/// leave the child in the right group, so what matters is the group it is
/// actually in. A child that is not leading its own group must not run the task
/// — a cancel or a deadline signals the recorded pgid, and the task's tree
/// would survive it.
fn group_failure() -> Option<&'static str> {
    // Test-only: the failure is otherwise only reachable through a real
    // `setpgid` error, which no test can provoke. Debug builds only, and never
    // in a release binary.
    #[cfg(debug_assertions)]
    if std::env::var_os("SHARKY_PARALLEL_TEST_GROUP_FAILURE").is_some() {
        return Some(GROUP_FAILURE_MESSAGE);
    }
    if task_group_confirmed(getpgrp().as_raw(), getpid().as_raw()) {
        None
    } else {
        Some(GROUP_FAILURE_MESSAGE)
    }
}

/// The confirmation itself: a task shell leads its own group when its group id
/// is its pid.
fn task_group_confirmed(pgrp: i32, pid: i32) -> bool {
    pgrp == pid
}

/// Reports a preparation failure from inside the child and leaves with 126, so
/// the parent can answer `spawn_error` with the reason.
fn child_fail(record: RawFd, message: &str) -> ! {
    let line = format!("sharky: task: {message}\n");
    let _ = crate::term::write_stderr(&line);
    let cwd = std::env::current_dir()
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .unwrap_or_default();
    StatusRecord {
        status: SPAWN_ERROR_STATUS,
        termination: "spawn_error",
        cwd,
        env_delta: None,
        message: Some(message.to_string()),
    }
    .write(record);
    capture::flush_stdio();
    // SAFETY: the forked child leaves without unwinding.
    unsafe { libc::_exit(SPAWN_ERROR_STATUS as i32) }
}

// -------------------------------------------------------------- status record

/// The line a task child writes to its status capture.
///
/// It is an optimization, never a requirement: a killed child leaves an empty
/// or truncated file, and the parent then derives what it can from the wait
/// status (parallel design §3.6).
struct StatusRecord {
    status: i64,
    termination: &'static str,
    cwd: String,
    env_delta: Option<Vec<(String, Option<String>)>>,
    message: Option<String>,
}

impl StatusRecord {
    /// One `write(2)` of one JSON line; the parent parses it with the protocol
    /// parser, so no format exists twice.
    ///
    /// The record has its own bound ([`RECORD_LIMIT_BYTES`]), larger than a
    /// protocol line because the env delta can be megabytes. Over that bound the
    /// delta is dropped — the only part with no natural size — and the line
    /// says so, so the parent never omits a requested delta silently while
    /// keeping the rest of the result real.
    fn write(&self, fd: RawFd) {
        let line = self.encode(false);
        if line.len() <= RECORD_LIMIT_BYTES {
            let _ = capture::write_safely(fd, line.as_bytes());
            return;
        }
        // `cwd` and `message` are small (PATH_MAX and one sentence), so a
        // record without the delta cannot reach the bound; when there was no
        // delta to drop this is the same line again.
        let line = self.encode(true);
        let _ = capture::write_safely(fd, line.as_bytes());
    }

    fn encode(&self, omit_env_delta: bool) -> String {
        let mut entries: Vec<(String, Value)> = vec![
            (String::from("status"), Value::Int(self.status)),
            (
                String::from("termination"),
                Value::String(self.termination.to_string()),
            ),
            (String::from("cwd"), Value::String(self.cwd.clone())),
        ];
        if let Some(delta) = &self.env_delta
            && !omit_env_delta
        {
            entries.push((
                String::from("env_delta"),
                Value::Object(
                    delta
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.clone(),
                                match value {
                                    Some(value) => Value::String(value.clone()),
                                    None => Value::Null,
                                },
                            )
                        })
                        .collect(),
                ),
            ));
        }
        if omit_env_delta && self.env_delta.is_some() {
            entries.push((
                String::from("env_delta_omitted"),
                Value::String(ENV_DELTA_TOO_LARGE.to_string()),
            ));
        }
        if let Some(message) = &self.message {
            entries.push((String::from("message"), Value::String(message.clone())));
        }
        let mut line = Value::Object(entries).encode();
        line.push('\n');
        line
    }
}

/// What the parent could recover from a task's status capture.
struct TaskRecord {
    status: i64,
    termination: Option<Termination>,
    signal: Option<String>,
    cwd: Option<String>,
    env_delta: Option<Vec<(String, Option<String>)>>,
    env_delta_omitted: Option<String>,
    message: Option<String>,
}

/// Reads a task's status record with its own bound, not the protocol's
/// `max_line_bytes`: the record may be larger than a line because it can carry
/// an env delta, and a record cut at the line limit would lose the status of a
/// task whose delta was big. `None` means the child left nothing parseable (it
/// was killed, say), and the caller then derives what it can from the wait
/// status (parallel design §3.6).
fn read_record(capture: &Capture) -> Option<TaskRecord> {
    let captured = capture.read_back(RECORD_LIMIT_BYTES).ok()?;
    let bytes = match captured.content {
        CaptureContent::Text(text) => text.into_bytes(),
        CaptureContent::Binary(bytes) => bytes,
    };
    let value = super::json::parse_line(&bytes).ok()?;
    let status = value.get("status")?.as_i64()?;
    let termination = match value.get("termination").and_then(Value::as_str) {
        Some("exited") => Some(Termination::Exited),
        Some("signaled") => Some(Termination::Signaled),
        Some("timeout") => Some(Termination::Timeout),
        Some("cancelled") => Some(Termination::Cancelled),
        Some("spawn_error") => Some(Termination::SpawnError),
        _ => None,
    };
    let env_delta = value
        .get("env_delta")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .map(|(name, value)| (name.clone(), value.as_str().map(str::to_string)))
                .collect()
        });
    Some(TaskRecord {
        status,
        termination,
        signal: value
            .get("signal")
            .and_then(Value::as_str)
            .map(str::to_string),
        cwd: value.get("cwd").and_then(Value::as_str).map(str::to_string),
        env_delta,
        env_delta_omitted: value
            .get("env_delta_omitted")
            .and_then(Value::as_str)
            .map(str::to_string),
        message: value
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

// -------------------------------------------------------------------- helpers

/// A task whose parent-side spawn failed: it fails the group under `fail-fast`
/// like any other non-zero outcome (parallel design §7), but keeps its own error
/// object.
pub(crate) fn spawn_error(task: &TaskRequest, session_cwd: &str, message: String) -> TaskResult {
    let mut result = TaskResult::new(task.id.clone(), effective_cwd(task, session_cwd));
    result.state = TaskState::SpawnError;
    result.termination = Termination::SpawnError;
    result.status = SPAWN_ERROR_STATUS;
    result.error = Some(TaskError {
        code: ErrorCode::SpawnFailed,
        message,
    });
    result
}

/// A task with no result at all: an internal failure, reported with the
/// non-zero status that makes the group fail rather than look successful.
pub(crate) fn unknown_result(id: String, session_cwd: &str) -> TaskResult {
    let mut result = TaskResult::new(id, session_cwd.to_string());
    result.status = UNKNOWN_STATUS;
    result.error = Some(TaskError {
        code: ErrorCode::Internal,
        message: String::from("the task produced no result"),
    });
    result
}

pub(crate) fn effective_cwd(task: &TaskRequest, session_cwd: &str) -> String {
    task.cwd.clone().unwrap_or_else(|| session_cwd.to_string())
}

fn signal_name(number: i32) -> String {
    crate::signal::name(number)
        .map(str::to_string)
        .unwrap_or_else(|| number.to_string())
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: the child owns `fd` and closes it exactly once.
        unsafe {
            libc::close(fd);
        }
    }
}

fn apply_sandbox(spec: &SandboxSpec, record: RawFd) {
    // SAFETY: PR_SET_NO_NEW_PRIVS only restricts this process and descendants.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        let err = std::io::Error::last_os_error();
        child_fail(record, &format!("sandbox prctl(NO_NEW_PRIVS) failed: {err}"));
    }

    if spec.no_network || spec.read_only_root {
        let uid = geteuid().as_raw();
        let gid = getegid().as_raw();
        let mut flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS;
        if spec.no_network {
            flags |= libc::CLONE_NEWNET;
        }
        // SAFETY: single-threaded forked child entering new namespaces.
        if unsafe { libc::unshare(flags) } < 0 {
            let err = std::io::Error::last_os_error();
            child_fail(record, &format!("sandbox namespace setup failed: {err}"));
        }
        if let Err(err) = std::fs::write("/proc/self/setgroups", b"deny\n")
            .and_then(|()| std::fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n")))
            .and_then(|()| std::fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n")))
        {
            child_fail(record, &format!("sandbox uid/gid mapping failed: {err}"));
        }
        if spec.read_only_root {
            let root = c"/";
            // SAFETY: NUL-terminated static C strings in private mount namespace.
            let ok = unsafe {
                libc::mount(
                    std::ptr::null(),
                    root.as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) == 0
                    && libc::mount(
                        root.as_ptr(),
                        root.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REC,
                        std::ptr::null(),
                    ) == 0
                    && libc::mount(
                        std::ptr::null(),
                        root.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_REC,
                        std::ptr::null(),
                    ) == 0
            };
            if !ok {
                let err = std::io::Error::last_os_error();
                child_fail(record, &format!("sandbox read_only_root failed: {err}"));
            }
        }
    }

    if let Some(bytes) = spec.max_file_bytes
        && let Err(err) = setrlimit(Resource::RLIMIT_FSIZE, bytes, bytes)
    {
        child_fail(record, &format!("sandbox RLIMIT_FSIZE failed: {err}"));
    }
    if let Some(procs) = spec.max_procs
        && let Err(err) = setrlimit(Resource::RLIMIT_NPROC, procs, procs)
    {
        child_fail(record, &format!("sandbox RLIMIT_NPROC failed: {err}"));
    }
    if let Some(secs) = spec.cpu_seconds
        && let Err(err) = setrlimit(Resource::RLIMIT_CPU, secs, secs)
    {
        child_fail(record, &format!("sandbox RLIMIT_CPU failed: {err}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A task leads its own group exactly when its group id is its pid; the
    /// child refuses the task otherwise, because a cancel would signal a group
    /// it is not in.
    #[test]
    fn a_task_group_is_confirmed_by_its_own_pid() {
        assert!(task_group_confirmed(42, 42));
        assert!(!task_group_confirmed(41, 42));
        assert!(!task_group_confirmed(42, 41));
    }
}
