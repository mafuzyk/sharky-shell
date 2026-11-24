//! Background job management with incremental log cursors and `wait_until` readiness checks.

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{IntoRawFd, RawFd};
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::libc;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, fork, pipe2, setpgid};

use super::json::Value;
use super::protocol::ExecTarget;
use crate::shell::Shell;

const MAX_JOB_BUFFER_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub(crate) struct WaitUntilSpec {
    pub(crate) regex: Option<String>,
    pub(crate) port: Option<u16>,
    pub(crate) timeout_ms: u64,
}

pub(crate) struct ManagedJob {
    pub(crate) pid: i32,
    pub(crate) pgid: i32,
    stdin_fd: Option<RawFd>,
    stdout_fd: Option<RawFd>,
    stderr_fd: Option<RawFd>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stdout_base: usize,
    stderr_base: usize,
    running: bool,
    status: Option<i32>,
}

impl ManagedJob {
    fn set_nonblocking(fd: RawFd) {
        if let Ok(flags) = fcntl(unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }, FcntlArg::F_GETFL) {
            let mut oflags = OFlag::from_bits_truncate(flags);
            oflags.insert(OFlag::O_NONBLOCK);
            let _ = fcntl(
                unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) },
                FcntlArg::F_SETFL(oflags),
            );
        }
    }

    fn drain_fd(fd_slot: &mut Option<RawFd>, buf: &mut Vec<u8>, base: &mut usize) {
        let Some(fd) = *fd_slot else {
            return;
        };
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: `fd` is an open non-blocking read pipe owned by this ManagedJob.
            let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                buf.extend_from_slice(&chunk[..n as usize]);
                if buf.len() > MAX_JOB_BUFFER_BYTES {
                    let drop_len = buf.len() - MAX_JOB_BUFFER_BYTES;
                    buf.drain(..drop_len);
                    *base += drop_len;
                }
            } else if n == 0 {
                let _ = unsafe { libc::close(fd) };
                *fd_slot = None;
                break;
            } else {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() != std::io::ErrorKind::WouldBlock {
                    let _ = unsafe { libc::close(fd) };
                    *fd_slot = None;
                }
                break;
            }
        }
    }

    fn drain(&mut self) {
        Self::drain_fd(&mut self.stdout_fd, &mut self.stdout_buf, &mut self.stdout_base);
        Self::drain_fd(&mut self.stderr_fd, &mut self.stderr_buf, &mut self.stderr_base);
        if self.running {
            match waitpid(Pid::from_raw(self.pid), Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => {
                    self.running = false;
                    self.status = Some(code);
                    Self::drain_fd(&mut self.stdout_fd, &mut self.stdout_buf, &mut self.stdout_base);
                    Self::drain_fd(&mut self.stderr_fd, &mut self.stderr_buf, &mut self.stderr_base);
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    self.running = false;
                    self.status = Some(128 + sig as i32);
                    Self::drain_fd(&mut self.stdout_fd, &mut self.stdout_buf, &mut self.stdout_base);
                    Self::drain_fd(&mut self.stderr_fd, &mut self.stderr_buf, &mut self.stderr_base);
                }
                Ok(WaitStatus::StillAlive) | Err(_) => {}
                Ok(_) => {}
            }
        }
    }

    fn total_stdout(&self) -> usize {
        self.stdout_base + self.stdout_buf.len()
    }

    fn total_stderr(&self) -> usize {
        self.stderr_base + self.stderr_buf.len()
    }

    fn slice_from(buf: &[u8], base: usize, cursor: usize) -> String {
        let start = cursor.saturating_sub(base).min(buf.len());
        String::from_utf8_lossy(&buf[start..]).into_owned()
    }

    fn close_fds(&mut self) {
        for slot in [&mut self.stdin_fd, &mut self.stdout_fd, &mut self.stderr_fd] {
            if let Some(fd) = slot.take() {
                let _ = unsafe { libc::close(fd) };
            }
        }
    }
}

pub(crate) struct JobManager {
    jobs: HashMap<String, ManagedJob>,
    next_id: u64,
}

impl JobManager {
    pub(crate) fn new() -> Self {
        Self {
            jobs: HashMap::new(),
            next_id: 1,
        }
    }

    pub(crate) fn start(
        &mut self,
        shell: &mut Shell,
        target: &ExecTarget,
        cwd: Option<&str>,
        env: &[(String, Option<String>)],
        wait_until: Option<&WaitUntilSpec>,
    ) -> Result<Value, String> {
        let (in_r, in_w) = pipe2(OFlag::O_CLOEXEC).map_err(|e| format!("pipe failed: {e}"))?;
        let (out_r, out_w) = pipe2(OFlag::O_CLOEXEC).map_err(|e| format!("pipe failed: {e}"))?;
        let (err_r, err_w) = pipe2(OFlag::O_CLOEXEC).map_err(|e| format!("pipe failed: {e}"))?;

        let mask = crate::signal::block_all();
        // SAFETY: forked child executes shell code and calls `_exit` without unwinding.
        match unsafe { fork() } {
            Ok(ForkResult::Parent { child }) => {
                crate::signal::restore_mask(mask);
                let pid = child.as_raw();
                let _ = setpgid(child, child);
                drop(in_r);
                drop(out_w);
                drop(err_w);
                let stdin_fd = in_w.into_raw_fd();
                let stdout_fd = out_r.into_raw_fd();
                let stderr_fd = err_r.into_raw_fd();
                ManagedJob::set_nonblocking(stdout_fd);
                ManagedJob::set_nonblocking(stderr_fd);

                let job_id = format!("job-{}", self.next_id);
                self.next_id += 1;
                let mut job = ManagedJob {
                    pid,
                    pgid: pid,
                    stdin_fd: Some(stdin_fd),
                    stdout_fd: Some(stdout_fd),
                    stderr_fd: Some(stderr_fd),
                    stdout_buf: Vec::new(),
                    stderr_buf: Vec::new(),
                    stdout_base: 0,
                    stderr_base: 0,
                    running: true,
                    status: None,
                };

                let mut matched = false;
                let mut timed_out = false;
                if let Some(spec) = wait_until {
                    let compiled = match &spec.regex {
                        Some(pat) => Some(
                            regex_lite::Regex::new(pat)
                                .map_err(|e| format!("invalid wait_until.regex: {e}"))?,
                        ),
                        None => None,
                    };
                    let deadline = Instant::now() + Duration::from_millis(spec.timeout_ms);
                    loop {
                        job.drain();
                        let out_text = String::from_utf8_lossy(&job.stdout_buf);
                        let err_text = String::from_utf8_lossy(&job.stderr_buf);
                        let regex_ok = compiled
                            .as_ref()
                            .is_some_and(|re| re.is_match(&out_text) || re.is_match(&err_text));
                        let port_ok = spec.port.is_some_and(|port| {
                            let addr = SocketAddr::from(([127, 0, 0, 1], port));
                            TcpStream::connect_timeout(&addr, Duration::from_millis(20)).is_ok()
                        });
                        if (spec.regex.is_some() && regex_ok) || (spec.port.is_some() && port_ok) {
                            matched = true;
                            break;
                        }
                        if !job.running {
                            break;
                        }
                        if Instant::now() >= deadline {
                            timed_out = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(15));
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                    job.drain();
                }

                let stdout = ManagedJob::slice_from(&job.stdout_buf, job.stdout_base, 0);
                let stderr = ManagedJob::slice_from(&job.stderr_buf, job.stderr_base, 0);
                let stdout_cursor = job.total_stdout() as i64;
                let stderr_cursor = job.total_stderr() as i64;
                let running = job.running;
                let status = job.status;
                self.jobs.insert(job_id.clone(), job);

                let mut fields = vec![
                    (String::from("job_id"), Value::String(job_id)),
                    (String::from("pid"), Value::Int(i64::from(pid))),
                    (String::from("pgid"), Value::Int(i64::from(pid))),
                    (String::from("running"), Value::Bool(running)),
                    (String::from("stdout"), Value::String(stdout)),
                    (String::from("stderr"), Value::String(stderr)),
                    (String::from("stdout_cursor"), Value::Int(stdout_cursor)),
                    (String::from("stderr_cursor"), Value::Int(stderr_cursor)),
                ];
                if wait_until.is_some() {
                    fields.push((String::from("matched"), Value::Bool(matched)));
                    fields.push((String::from("timed_out"), Value::Bool(timed_out)));
                }
                if let Some(code) = status {
                    fields.push((String::from("status"), Value::Int(i64::from(code))));
                }
                Ok(Value::Object(fields))
            }
            Ok(ForkResult::Child) => {
                let _ = setpgid(Pid::from_raw(0), Pid::from_raw(0));
                crate::agent::child_guard();
                shell.reset_child_traps();
                crate::signal::restore_mask(mask);
                drop(in_w);
                drop(out_r);
                drop(err_r);
                let in_raw = in_r.into_raw_fd();
                let out_raw = out_w.into_raw_fd();
                let err_raw = err_w.into_raw_fd();
                // SAFETY: redirect standard descriptors cleanly in child.
                unsafe {
                    libc::dup2(in_raw, 0);
                    libc::dup2(out_raw, 1);
                    libc::dup2(err_raw, 2);
                    if in_raw > 2 {
                        libc::close(in_raw);
                    }
                    if out_raw > 2 {
                        libc::close(out_raw);
                    }
                    if err_raw > 2 {
                        libc::close(err_raw);
                    }
                }
                if let Some(dir) = cwd {
                    let _ = shell.enter_task_directory(dir);
                }
                for (name, val) in env {
                    shell.set_task_env(name, val.clone());
                }
                let status = match target {
                    ExecTarget::Command(cmd) => match shell.session_eval(cmd) {
                        Ok(code) => code,
                        Err(err) => {
                            let msg = format!("sharky: {}\n", err.message);
                            let _ = crate::term::write_stderr(&msg);
                            2
                        }
                    },
                    ExecTarget::Argv(argv) => shell.session_exec_argv(argv),
                };
                unsafe { libc::_exit(status) }
            }
            Err(err) => {
                crate::signal::restore_mask(mask);
                Err(format!("fork failed: {err}"))
            }
        }
    }

    pub(crate) fn poll(
        &mut self,
        job_id: &str,
        stdout_cursor: usize,
        stderr_cursor: usize,
        wait_ms: u64,
    ) -> Result<Value, String> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| format!("unknown job_id: {job_id}"))?;
        let deadline = Instant::now() + Duration::from_millis(wait_ms);
        loop {
            job.drain();
            let has_delta =
                job.total_stdout() > stdout_cursor || job.total_stderr() > stderr_cursor;
            if has_delta || !job.running || wait_ms == 0 || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }

        let stdout = ManagedJob::slice_from(&job.stdout_buf, job.stdout_base, stdout_cursor);
        let stderr = ManagedJob::slice_from(&job.stderr_buf, job.stderr_base, stderr_cursor);
        let mut fields = vec![
            (String::from("job_id"), Value::String(job_id.to_string())),
            (String::from("pid"), Value::Int(i64::from(job.pid))),
            (String::from("running"), Value::Bool(job.running)),
            (String::from("stdout"), Value::String(stdout)),
            (String::from("stderr"), Value::String(stderr)),
            (
                String::from("stdout_cursor"),
                Value::Int(job.total_stdout() as i64),
            ),
            (
                String::from("stderr_cursor"),
                Value::Int(job.total_stderr() as i64),
            ),
        ];
        if let Some(code) = job.status {
            fields.push((String::from("status"), Value::Int(i64::from(code))));
        }
        Ok(Value::Object(fields))
    }

    pub(crate) fn send(
        &mut self,
        job_id: &str,
        data: Option<&str>,
        close_stdin: bool,
    ) -> Result<Value, String> {
        let job = self
            .jobs
            .get_mut(job_id)
            .ok_or_else(|| format!("unknown job_id: {job_id}"))?;
        let mut bytes_written = 0usize;
        if let Some(text) = data {
            let Some(fd) = job.stdin_fd else {
                return Err(format!("stdin already closed for job {job_id}"));
            };
            let bytes = text.as_bytes();
            let mut offset = 0;
            while offset < bytes.len() {
                // SAFETY: `fd` is open stdin pipe for the managed job.
                let n = unsafe {
                    libc::write(
                        fd,
                        bytes[offset..].as_ptr().cast(),
                        bytes.len() - offset,
                    )
                };
                if n > 0 {
                    offset += n as usize;
                    bytes_written += n as usize;
                } else {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(format!("write to stdin failed: {err}"));
                }
            }
        }
        if close_stdin && let Some(fd) = job.stdin_fd.take() {
            let _ = unsafe { libc::close(fd) };
        }
        job.drain();
        Ok(Value::Object(vec![
            (String::from("job_id"), Value::String(job_id.to_string())),
            (
                String::from("bytes_written"),
                Value::Int(bytes_written as i64),
            ),
            (String::from("stdin_closed"), Value::Bool(job.stdin_fd.is_none())),
            (String::from("running"), Value::Bool(job.running)),
        ]))
    }

    pub(crate) fn stop(&mut self, job_id: &str, sig_name: Option<&str>) -> Result<Value, String> {
        let sig = match sig_name {
            None | Some("TERM") | Some("SIGTERM") => Signal::SIGTERM,
            Some("KILL") | Some("SIGKILL") => Signal::SIGKILL,
            Some("INT") | Some("SIGINT") => Signal::SIGINT,
            Some("HUP") | Some("SIGHUP") => Signal::SIGHUP,
            Some(other) => return Err(format!("unsupported signal: {other}")),
        };
        let mut job = self
            .jobs
            .remove(job_id)
            .ok_or_else(|| format!("unknown job_id: {job_id}"))?;
        job.drain();
        if job.running {
            let _ = killpg(Pid::from_raw(job.pgid), sig);
            let deadline = Instant::now() + Duration::from_millis(400);
            while job.running && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                job.drain();
            }
            if job.running && sig != Signal::SIGKILL {
                let _ = killpg(Pid::from_raw(job.pgid), Signal::SIGKILL);
                let _ = waitpid(Pid::from_raw(job.pid), None);
                job.running = false;
                job.status.get_or_insert(137);
                job.drain();
            }
        }
        job.close_fds();
        let stdout = ManagedJob::slice_from(&job.stdout_buf, job.stdout_base, 0);
        let stderr = ManagedJob::slice_from(&job.stderr_buf, job.stderr_base, 0);
        let mut fields = vec![
            (String::from("job_id"), Value::String(job_id.to_string())),
            (String::from("pid"), Value::Int(i64::from(job.pid))),
            (String::from("running"), Value::Bool(false)),
            (String::from("stdout"), Value::String(stdout)),
            (String::from("stderr"), Value::String(stderr)),
        ];
        if let Some(code) = job.status {
            fields.push((String::from("status"), Value::Int(i64::from(code))));
        }
        Ok(Value::Object(fields))
    }
}

impl Drop for JobManager {
    fn drop(&mut self) {
        for (_, mut job) in self.jobs.drain() {
            if job.running {
                let _ = killpg(Pid::from_raw(job.pgid), Signal::SIGKILL);
                let _ = waitpid(Pid::from_raw(job.pid), Some(WaitPidFlag::WNOHANG));
            }
            job.close_fds();
        }
    }
}
