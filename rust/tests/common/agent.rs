//! Pipe-based client for `sharky agent --stdio`.
//!
//! One [`Session`] owns a spawned agent and its two pipes. Every test obeys the
//! deadlock discipline of design §10: write one request, read one line, never
//! read more lines than responses expected. The line reader polls the child's
//! stdout, so a missing response is a timeout instead of a hung test.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

// The protocol's own parser doubles as the test's response reader; the module
// is included verbatim so the tests exercise the shipped implementation.
#[path = "../../src/agent/json.rs"]
#[allow(dead_code)]
pub mod json;

pub const NO_CONFIG: &str = "/nonexistent/sharky-agent-session-test";

pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    buffer: Vec<u8>,
}

impl Session {
    /// Spawns `sharky agent --stdio` with `extra` arguments appended.
    pub fn spawn(extra: &[&str]) -> Session {
        Session::spawn_with_env(extra, &[])
    }

    /// [`Session::spawn`] with extra environment for the agent process. A
    /// capture is created in the parent from the child's own `TMPDIR`, so this
    /// is the only way to make a parent-side capture fail without mutating the
    /// test process's environment.
    pub fn spawn_with_env(extra: &[&str], env: &[(&str, &str)]) -> Session {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
        command
            .arg("agent")
            .arg("--stdio")
            .args(extra)
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("PATH", super::tool_path())
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn sharky agent --stdio");
        let stdin = child.stdin.take().expect("agent stdin");
        let stdout = child.stdout.take().expect("agent stdout");
        Session {
            child,
            stdin: Some(stdin),
            stdout,
            buffer: Vec::new(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Sends one line (a newline is appended).
    pub fn send(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.send_raw(&bytes);
    }

    /// Sends arbitrary bytes, including a frame with no trailing newline.
    pub fn send_raw(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("agent stdin is still open");
        stdin.write_all(bytes).expect("write request");
        stdin.flush().expect("flush request");
    }

    /// Reads one response line, blocking until it arrives or the pipe closes.
    pub fn recv(&mut self) -> String {
        self.line(None)
            .expect("agent closed the response stream before answering")
    }

    /// Reads one response line, or `None` when it does not arrive in `ms`.
    pub fn recv_within(&mut self, ms: i64) -> Option<String> {
        self.line(Some(ms))
    }

    pub fn json(&mut self) -> json::Value {
        let line = self.recv();
        self.parse(line)
    }

    pub fn json_within(&mut self, ms: i64) -> Option<json::Value> {
        let line = self.recv_within(ms)?;
        Some(self.parse(line))
    }

    /// Sends one `op:"parallel"` frame and reads its single response, asserting
    /// the envelope. A group is answered by exactly one `type:"parallel"` line,
    /// so anything else is a protocol failure.
    pub fn parallel(&mut self, line: &str) -> json::Value {
        self.send(line);
        let response = self.json();
        assert_eq!(
            response.get("type").and_then(json::Value::as_str),
            Some("parallel"),
            "expected one parallel response: {response:?}"
        );
        response
    }

    fn parse(&self, line: String) -> json::Value {
        json::parse_line(line.as_bytes())
            .unwrap_or_else(|error| panic!("response is not JSON: {line:?}: {error}"))
    }

    fn line(&mut self, timeout: Option<i64>) -> Option<String> {
        let deadline = timeout.map(|ms| Instant::now() + Duration::from_millis(ms.max(0) as u64));
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                let text = String::from_utf8(line[..line.len() - 1].to_vec())
                    .expect("the protocol stream must stay UTF-8");
                return Some(text);
            }
            let wait_ms = match deadline {
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return None;
                    }
                    (deadline - now).as_millis().max(1) as i32
                }
                None => -1,
            };
            let mut descriptor = nix::libc::pollfd {
                fd: self.stdout.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid descriptor with a millisecond timeout.
            let ready = unsafe { nix::libc::poll(&mut descriptor, 1, wait_ms) };
            assert!(ready >= 0, "poll on the agent stdout failed");
            if ready == 0 {
                return None;
            }
            let mut chunk = [0u8; 8192];
            // SAFETY: the buffer is valid for writes of its own length.
            let count = unsafe {
                nix::libc::read(
                    self.stdout.as_raw_fd(),
                    chunk.as_mut_ptr() as *mut _,
                    chunk.len(),
                )
            };
            if count <= 0 {
                return None;
            }
            self.buffer.extend_from_slice(&chunk[..count as usize]);
        }
    }

    /// Closes the write end, which is the transport EOF the driver observes.
    pub fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Waits up to `ms` for the agent process to exit, keeping stdin open.
    pub fn wait_within(&mut self, ms: u64) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait on agent") {
                return Some(status);
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let remaining = (deadline - now).as_millis().min(20) as i64;
            let _ = self.line(Some(remaining));
        }
    }

    /// Drains whatever is left, waits for the process, and reports the exit
    /// status plus every line and byte the agent wrote after the test's last
    /// read. `stderr` is the agent process's own stderr, which must stay empty
    /// once isolation is in place.
    pub fn finish(&mut self) -> (ExitStatus, Vec<String>, String) {
        self.close_stdin();
        let mut leftovers = Vec::new();
        while let Some(line) = self.line(None) {
            leftovers.push(line);
        }
        let status = self.child.wait().expect("wait for the agent");
        let mut errors = String::new();
        if let Some(mut handle) = self.child.stderr.take() {
            let _ = handle.read_to_string(&mut errors);
        }
        (status, leftovers, errors)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // A failed assertion must not leave an agent behind. Closing stdin is
        // the documented shutdown; kill only if it is still running.
        self.stdin.take();
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
