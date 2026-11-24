//! End-to-end tests for the `sharky agent --socket PATH` transport.
//!
//! The socket carries exactly the same JSONL contract as `--stdio`. These
//! cases drive the real binary over a Unix stream socket and assert the extra
//! lifecycle obligations: one client only, mode 0600, path cleanup on every
//! exit path, and child-group cleanup when the client disappears.

mod common;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use common::agent::json;

const NO_CONFIG: &str = "/nonexistent/sharky-agent-socket-test";

// ------------------------------------------------------------------- helpers

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-agent-socket-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn agent() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
    command
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn get<'a>(value: &'a json::Value, key: &str) -> &'a json::Value {
    value
        .get(key)
        .unwrap_or_else(|| panic!("missing {key} in {value:?}"))
}

fn text(value: &json::Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("not a string: {value:?}"))
}

fn int(value: &json::Value) -> i64 {
    value
        .as_i64()
        .unwrap_or_else(|| panic!("not an integer: {value:?}"))
}

/// One client attached to a live socket session.
struct SocketSession {
    child: Child,
    stream: UnixStream,
    buffer: Vec<u8>,
}

impl SocketSession {
    /// Spawns the agent and connects within two seconds.
    fn connect(path: &Path) -> SocketSession {
        let mut child = agent()
            .arg("agent")
            .arg("--socket")
            .arg(path)
            .arg("--no-config")
            .spawn()
            .expect("spawn sharky agent --socket");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match UnixStream::connect(path) {
                Ok(stream) => {
                    return SocketSession {
                        child,
                        stream,
                        buffer: Vec::new(),
                    };
                }
                Err(error) => {
                    if let Some(status) = child.try_wait().expect("try_wait on agent") {
                        let mut errors = String::new();
                        if let Some(mut handle) = child.stderr.take() {
                            let _ = handle.read_to_string(&mut errors);
                        }
                        panic!(
                            "agent exited before accepting a client: {status}, stderr={errors}, last connect error={error}"
                        );
                    }
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("no socket listener at {} within 2 s: {error}", path.display());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn send(&mut self, line: &str) {
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        self.stream.write_all(&bytes).expect("write request");
        self.stream.flush().expect("flush request");
    }

    fn recv(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                return String::from_utf8(line[..line.len() - 1].to_vec())
                    .expect("the protocol stream must be UTF-8");
            }
            let wait_ms = match deadline.checked_duration_since(Instant::now()) {
                Some(remaining) => remaining.as_millis().max(1) as i32,
                None => panic!("timed out waiting for a response line"),
            };
            let mut descriptor = nix::libc::pollfd {
                fd: self.stream.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid descriptor with a millisecond timeout.
            let ready = unsafe { nix::libc::poll(&mut descriptor, 1, wait_ms) };
            assert!(ready >= 0, "poll on the agent socket failed");
            assert!(ready > 0, "agent closed the socket before answering");
            let mut chunk = [0u8; 8192];
            // SAFETY: the buffer is valid for writes of its own length.
            let count = unsafe {
                nix::libc::read(
                    self.stream.as_raw_fd(),
                    chunk.as_mut_ptr() as *mut _,
                    chunk.len(),
                )
            };
            assert!(count > 0, "agent closed the socket before answering");
            self.buffer.extend_from_slice(&chunk[..count as usize]);
        }
    }

    fn json(&mut self) -> json::Value {
        let line = self.recv();
        json::parse_line(line.as_bytes())
            .unwrap_or_else(|error| panic!("response is not JSON: {line:?}: {error}"))
    }

    fn ready(&mut self) -> json::Value {
        let response = self.json();
        assert_eq!(text(get(&response, "type")), "ready", "{response:?}");
        response
    }

    fn wait_exit(&mut self, ms: u64) -> Option<ExitStatus> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait on agent") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Closes the client and waits for the agent to leave, returning its
    /// exit status and whatever it wrote to the real stderr.
    fn finish(mut self) -> (ExitStatus, String) {
        self.stream.shutdown(std::net::Shutdown::Both).expect("shutdown client");
        let status = self
            .wait_exit(5000)
            .expect("agent did not exit after client disconnect");
        let mut errors = String::new();
        if let Some(mut handle) = self.child.stderr.take() {
            let _ = handle.read_to_string(&mut errors);
        }
        (status, errors)
    }

    fn pid(&self) -> i32 {
        i32::try_from(self.child.id()).expect("agent pid fits i32")
    }
}

impl Drop for SocketSession {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `kill(pid, 0)` plus the zombie state, so a reaped-but-not-yet-collected
/// child counts as gone.
fn process_gone(pid: i32) -> bool {
    // SAFETY: signal 0 only performs the existence check.
    if unsafe { nix::libc::kill(pid, 0) } != 0 {
        return true;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => matches!(stat.split_whitespace().nth(2), Some("Z") | None),
        Err(_) => true,
    }
}

fn gone_within(pid: i32, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    process_gone(pid)
}

fn stderr_of(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// --------------------------------------------------------------------- tests

/// Extra `/dev/null` descriptors beyond isolated stdio in a live agent at the
/// ready frame. fd 0, 1 and 2 are `dup2` targets and must also point at null;
/// the shared setup owns exactly one additional null fd for those `dup2` calls
/// and future captures. A second one is a transport-startup leak.
fn extra_null_fds(pid: i32) -> Vec<i32> {
    for fd in 0..=2 {
        let target = std::fs::read_link(format!("/proc/{pid}/fd/{fd}"))
            .unwrap_or_else(|error| panic!("fd {fd} is not readable: {error}"));
        assert_eq!(target, Path::new("/dev/null"), "fd {fd} is not isolated");
    }
    let mut nulls = Vec::new();
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap_or_else(|error| panic!("cannot list /proc/{pid}/fd: {error}"));
    for entry in entries.flatten() {
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd < 3 {
            continue;
        }
        if std::fs::read_link(entry.path()).is_ok_and(|target| target == Path::new("/dev/null")) {
            nulls.push(fd);
        }
    }
    nulls.sort_unstable();
    nulls
}

#[test]
fn ready_frame_leaves_exactly_one_low_null_descriptor() {
    let dir = scratch("fd-count");
    let path = dir.join("sock");
    let mut client = SocketSession::connect(&path);
    client.ready();

    let nulls = extra_null_fds(client.pid());
    assert_eq!(
        nulls.len(),
        1,
        "socket startup must leave exactly one extra /dev/null descriptor: {nulls:?}"
    );

    let (status, errors) = client.finish();
    assert!(status.success(), "{status:?}");
    assert!(errors.is_empty(), "{errors}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn socket_session_serves_ready_exec_and_close_then_unlinks_the_path() {
    let dir = scratch("roundtrip");
    let socket = dir.join("s.sock");
    let mut session = SocketSession::connect(&socket);

    let ready = session.ready();
    assert_eq!(
        get(&ready, "schema_version"),
        &json::Value::Int(1),
        "{ready:?}"
    );

    session.send(
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"printf socket-ok"}"#,
    );
    let result = session.json();
    assert_eq!(text(get(&result, "type")), "result", "{result:?}");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(int(get(&result, "status")), 0);
    assert_eq!(text(get(&result, "stdout")), "socket-ok");
    assert_eq!(text(get(&result, "stdout_encoding")), "utf8");

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let ack = session.json();
    assert_eq!(text(get(&ack, "type")), "ack", "{ack:?}");
    assert_eq!(text(get(&ack, "op")), "close");
    assert_eq!(text(get(&ack, "outcome")), "closed");
    let closed = session.json();
    assert_eq!(text(get(&closed, "type")), "closed", "{closed:?}");
    assert_eq!(int(get(&closed, "status")), 0);

    let (status, errors) = session.finish();
    assert_eq!(status.code(), Some(0));
    assert!(errors.is_empty(), "unexpected agent stderr: {errors}");
    assert!(!socket.exists(), "socket path survived session close");
}

#[test]
fn socket_is_private_mode_0600() {
    let dir = scratch("mode");
    let socket = dir.join("s.sock");
    let session = SocketSession::connect(&socket);
    let mode = std::fs::metadata(&socket)
        .expect("socket metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "socket must be private");
    let _ = session.finish();
}

#[test]
fn a_stale_path_fails_without_deleting_it() {
    let dir = scratch("stale");
    let socket = dir.join("s.sock");
    std::fs::write(&socket, b"not a socket").expect("create stale path");

    let output = agent()
        .arg("agent")
        .arg("--socket")
        .arg(&socket)
        .arg("--no-config")
        .output()
        .expect("run sharky agent --socket with a stale path");

    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    assert!(
        stderr_of(&output).contains("socket"),
        "missing diagnostic: {}",
        stderr_of(&output)
    );
    assert_eq!(
        std::fs::read(&socket).expect("stale path was removed"),
        b"not a socket"
    );
}

#[test]
fn a_second_client_cannot_attach_after_the_first_one() {
    let dir = scratch("single-client");
    let socket = dir.join("s.sock");
    let mut session = SocketSession::connect(&socket);
    session.ready();

    assert!(
        UnixStream::connect(&socket).is_err(),
        "a second client must not be able to attach"
    );

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    assert_eq!(text(get(&session.json(), "type")), "ack");
    assert_eq!(text(get(&session.json(), "type")), "closed");
    let (status, _) = session.finish();
    assert_eq!(status.code(), Some(0));
    assert!(!socket.exists(), "socket path survived close");
}

#[test]
fn client_disconnect_cleans_up_children_and_the_socket_path() {
    let dir = scratch("disconnect");
    let socket = dir.join("s.sock");
    let mut session = SocketSession::connect(&socket);
    session.ready();

    session.send(
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"sleep 30 & echo $!"}"#,
    );
    let result = session.json();
    assert_eq!(text(get(&result, "type")), "result", "{result:?}");
    let child_pid = text(get(&result, "stdout"))
        .trim()
        .parse::<i32>()
        .expect("background child pid");
    let agent_pid = session.pid();
    let (status, errors) = session.finish();
    assert_eq!(status.code(), Some(0), "stderr={errors}");
    assert!(!socket.exists(), "socket path survived disconnect");
    assert!(
        gone_within(child_pid, 3000),
        "background child {child_pid} survived client disconnect"
    );
    assert!(
        gone_within(agent_pid, 1000),
        "agent {agent_pid} left a zombie behind"
    );
}

#[test]
fn a_signal_to_the_server_unlinks_the_path_and_kills_running_children() {
    let dir = scratch("signal");
    let socket = dir.join("s.sock");
    let mut session = SocketSession::connect(&socket);
    session.ready();

    session.send(
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"sh -c 'trap \"\" TERM; sleep 30 & echo $!'"}"#,
    );
    let result = session.json();
    assert_eq!(text(get(&result, "type")), "result", "{result:?}");
    let child_pid = text(get(&result, "stdout"))
        .trim()
        .parse::<i32>()
        .expect("background child pid");
    let agent_pid = session.pid();

    // SAFETY: signal 0/SIGTERM only targets the agent process this test owns.
    assert_eq!(unsafe { nix::libc::kill(agent_pid, nix::libc::SIGTERM) }, 0);

    let status = session
        .wait_exit(5000)
        .expect("agent did not exit after SIGTERM");
    assert!(status.code().is_some(), "unexpected signal status: {status}");
    assert!(!socket.exists(), "socket path survived a signal");
    assert!(
        gone_within(child_pid, 3000),
        "background child {child_pid} survived a server signal"
    );
    assert!(
        gone_within(agent_pid, 1000),
        "agent {agent_pid} left a zombie behind after a signal"
    );
}

#[test]
fn transport_flags_are_mutually_exclusive_and_required() {
    let dir = scratch("usage");
    let socket = dir.join("s.sock");

    let missing = agent().arg("agent").output().expect("agent without transport");
    assert_eq!(missing.status.code(), Some(2));
    let message = stderr_of(&missing);
    assert!(message.contains("--socket"), "unclear usage error: {message}");

    let both = agent()
        .arg("agent")
        .arg("--stdio")
        .arg("--socket")
        .arg(&socket)
        .output()
        .expect("agent with both transports");
    assert_eq!(both.status.code(), Some(2));
    let message = stderr_of(&both);
    assert!(
        message.contains("mutually exclusive"),
        "unclear usage error: {message}"
    );
}

#[test]
fn an_overlong_socket_path_fails_without_touching_the_filesystem() {
    let dir = scratch("longpath");
    let socket = dir.join("x".repeat(120));
    let output = agent()
        .arg("agent")
        .arg("--socket")
        .arg(&socket)
        .output()
        .expect("agent with an overlong socket path");
    assert_eq!(output.status.code(), Some(2), "{}", stderr_of(&output));
    let message = stderr_of(&output);
    assert!(
        message.contains("too long") || message.contains("socket path"),
        "unclear diagnostic: {message}"
    );
    assert!(!socket.exists(), "overlong path should not be created");
}

#[test]
fn help_advertises_the_socket_transport() {
    let output = agent()
        .arg("help")
        .arg("agent")
        .output()
        .expect("help agent");
    assert_eq!(output.status.code(), Some(0));
    let text = String::from_utf8(output.stdout).expect("UTF-8 help");
    for fragment in ["--socket", "single client", "not a sandbox"] {
        assert!(text.contains(fragment), "help agent is missing {fragment:?}");
    }
}
