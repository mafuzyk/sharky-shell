//! Agent session runtime: `sharky agent --stdio` or `--socket PATH`.
//!
//! One process, one [`Shell`], one JSONL connection. Both transports feed the
//! same session driver: `--stdio` duplicates the pipe endpoints to high
//! descriptors, while `--socket` accepts exactly one client and duplicates its
//! connected socket twice. The driver decodes a line ([`protocol`]), evaluates
//! shell code in process (`Shell::session_eval`), and writes exactly one
//! response line per accepted request.
//!
//! Descriptor discipline (design §4/§5), all of which happens before any shell
//! code runs:
//!
//! * fd 0, 1 and 2 are re-pointed at `/dev/null`; the control channel lives on
//!   descriptors duplicated to 900+ with `F_DUPFD_CLOEXEC`, so no command,
//!   `exec`, subprocess or pipeline can reach it through stdio.
//! * Per request, fd 0 is the payload file (or `/dev/null`), and fd 1/2 are
//!   unlinked temp files created above fd 10, so a descriptor the request left
//!   closed can never be mistaken for a capture. Before swapping, the guard
//!   `dup`s whatever fd 0/1/2 point at; afterwards it puts those saved
//!   descriptors back when the target still carries the capture, and leaves a
//!   descriptor the request re-pointed or closed alone. That is what keeps
//!   `exec >log` alive across requests: the next request captures into fd 1 for
//!   the response and replays the bytes into the saved log, as bash would.
//! * The control descriptors are re-checked after every request; a command that
//!   re-pointed one of them ends the session instead of answering on a
//!   descriptor it no longer owns.
//!
//! Responses are written with one raw `write` per line, so "flushed per line"
//! is structural: there is no user-space buffer to lose. `SIGPIPE` is blocked
//! around the write, so a dead client surfaces as `EPIPE` and takes the cleanup
//! path instead of killing the agent before it can reap anything.
//!
//! Request-scoped process groups, cancellation and deadlines live in
//! [`supervisor`]: every foreground/job group is recorded there, `cancel` and
//! `timeout_ms` signal the group of the in-flight request (never the agent's
//! own group), and the session wait hook pumps that machinery while a child is
//! alive. `ready.protocol.ops` and `ready.protocol.features` advertise exactly
//! what `tests/agent_session.rs` proves.

mod attempt;
mod code;
pub(crate) mod guide;
mod jobs;
pub(crate) mod mcp;
mod parallel;
mod policy;
mod projection;
mod protocol;
mod store;
mod supervisor;
mod workflow;

// The capture surface keeps identity/byte_len accessors that only the capture
// unit test reads today; the driver uses the create/fill/read_back/guard path.
#[allow(dead_code)]
pub(crate) mod capture;
// The encoder-side wire types (request encoders, the response vocabulary and the
// error codes reserved for the supervisor unit) are the frozen contract
// exercised by `tests/agent_protocol.rs`; the driver consumes the decode side.
#[allow(dead_code)]
pub(crate) mod json;

// The interpreter calls these from `shell.rs` and `exec.rs`. Each one is a
// no-op outside a session, which is what keeps the non-agent paths
// byte-identical.
pub(crate) use supervisor::{
    active, begin_request, cancel_op, child_guard, clear_foreground, cleanup, end_request,
    next_frame, note_foreground, note_group, pending_outcome, request_group, service,
    take_queued_cancelled, wait_poll, wait_poll_fd,
};

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use capture::{Capture, CaptureContent, StdioGuard};
use json::{
    Ack, Captured, Closed, ErrorCode, ErrorResponse, ExecResult, FsDelta, JobSummary, Limits,
    ProtocolInfo, Ready, Response, ShellOptions, StateResult, StateSection, StreamChunk,
    StreamData, Termination, Value,
};
use protocol::DecodedRequest;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// First descriptor number the control channel may use (`F_DUPFD` picks the
/// first free one at or above this).
const CONTROL_FD_MIN: RawFd = 900;

/// Read size for the control channel and the payload files.
const READ_CHUNK: usize = 64 * 1024;

/// How much of the control-channel buffer is compacted at once.
const COMPACT_THRESHOLD: usize = 64 * 1024;

// ------------------------------------------------------------------------ CLI

/// Where the control channel comes from. Exactly one is selected.
enum Transport {
    Stdio,
    Socket(PathBuf),
    Mcp,
}

const STDIO_TRANSPORT: &str = "stdio";
const SOCKET_TRANSPORT: &str = "socket";
const MCP_TRANSPORT: &str = "mcp";

impl Transport {
    fn name(&self) -> &'static str {
        match self {
            Transport::Stdio => STDIO_TRANSPORT,
            Transport::Socket(_) => SOCKET_TRANSPORT,
            Transport::Mcp => MCP_TRANSPORT,
        }
    }
}

/// Names of the transports `parse_options` can select, for capability output.
pub(crate) fn transport_names() -> &'static [&'static str] {
    &[STDIO_TRANSPORT, SOCKET_TRANSPORT, MCP_TRANSPORT]
}

#[derive(Clone)]
pub(crate) struct SessionOptions {
    pub(crate) no_config: bool,
    pub(crate) redact_env: bool,
    pub(crate) limits: Limits,
    pub(crate) state_file: Option<PathBuf>,
    pub(crate) policy: Option<Arc<policy::Policy>>,
    pub(crate) multi_client: bool,
}

/// Dispatches `sharky agent ...` before the normal flag parser. `None` means
/// the invocation is not an agent request and `cli::run` continues.
pub(crate) fn try_cli(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("agent") {
        return None;
    }
    if args
        .get(1)
        .map(String::as_str)
        .is_some_and(|arg| matches!(arg, "guide" | "--guide"))
    {
        return Some(guide::run(&args[2..]));
    }
    // `--help`/`-h` short-circuit before the required `--stdio` check: it is
    // the same text as `sharky help agent`, on stdout with exit 0. Any other
    // unknown flag is still a usage error from `parse_options` (exit 2).
    if args[1..]
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        if let Some(text) = crate::help::topic("agent") {
            crate::help::print_text(&text);
        }
        return Some(0);
    }
    Some(match parse_options(&args[1..]) {
        Ok((Transport::Stdio, options)) => run_stdio(options),
        Ok((Transport::Socket(path), options)) => run_socket(options, path),
        Ok((Transport::Mcp, options)) => mcp::serve(options),
        Err(message) => {
            eprintln!("sharky: agent: {message}");
            2
        }
    })
}

fn parse_options(args: &[String]) -> Result<(Transport, SessionOptions), String> {
    let mut options = SessionOptions {
        no_config: false,
        redact_env: false,
        limits: Limits::default(),
        state_file: None,
        policy: None,
        multi_client: false,
    };
    let mut stdio = false;
    let mut socket: Option<PathBuf> = None;
    let mut mcp = false;
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "--stdio" => stdio = true,
            "--mcp" => mcp = true,
            "--socket" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument}: option requires a path"))?;
                if socket.is_some() {
                    return Err(String::from("--socket may be given only once"));
                }
                socket = Some(PathBuf::from(value));
                index += 1;
            }
            "--state-file" | "--resume" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument}: option requires a path"))?;
                if value.is_empty() {
                    return Err(format!("{argument}: path must not be empty"));
                }
                if options.state_file.is_some() {
                    return Err(String::from("--state-file/--resume may be given only once"));
                }
                options.state_file = Some(PathBuf::from(value));
                index += 1;
            }
            "--no-config" => options.no_config = true,
            "--redact-env" => options.redact_env = true,
            "--multi-client" => options.multi_client = true,
            "--policy" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| String::from("--policy: option requires a path"))?;
                if value.is_empty() {
                    return Err(String::from("--policy: path must not be empty"));
                }
                if options.policy.is_some() {
                    return Err(String::from("--policy may be given only once"));
                }
                let loaded = policy::Policy::load(Path::new(value))?;
                options.policy = Some(Arc::new(loaded));
                index += 1;
            }
            "-l" | "--login" => {
                return Err(String::from(
                    "login mode is not available: login profiles would write into the protocol stream",
                ));
            }
            "--max-line-bytes" | "--max-capture-bytes" | "--max-parallel-output-bytes" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("{argument}: option requires an argument"))?;
                let bytes: usize = value
                    .parse()
                    .map_err(|_| format!("{argument}: not a byte count: {value}"))?;
                if bytes == 0 {
                    return Err(format!("{argument}: must be greater than zero"));
                }
                match argument {
                    "--max-line-bytes" => options.limits.max_line_bytes = bytes,
                    "--max-capture-bytes" => options.limits.max_capture_bytes = bytes,
                    _ => options.limits.max_parallel_output_bytes = bytes,
                }
                index += 1;
            }
            other if other.starts_with('-') => {
                return Err(format!("unexpected option '{other}'"));
            }
            other => return Err(format!("unexpected argument '{other}'")),
        }
        index += 1;
    }
    let selected = usize::from(stdio) + usize::from(socket.is_some()) + usize::from(mcp);
    if selected > 1 {
        return Err(String::from(
            "--stdio, --socket and --mcp are mutually exclusive: choose one transport",
        ));
    }
    if selected == 0 {
        return Err(String::from(
            "a transport is required: usage is sharky agent --stdio | --socket PATH | --mcp [--no-config] [--redact-env] [--state-file PATH]",
        ));
    }
    let transport = if mcp {
        Transport::Mcp
    } else if let Some(path) = socket {
        Transport::Socket(path)
    } else {
        Transport::Stdio
    };
    if options.multi_client && !matches!(transport, Transport::Socket(_)) {
        return Err(String::from("--multi-client requires --socket PATH"));
    }
    debug_assert!(transport_names().contains(&transport.name()));
    Ok((transport, options))
}

// ------------------------------------------------------------------- startup

fn run_stdio(options: SessionOptions) -> i32 {
    if crate::term::is_tty(0) {
        eprintln!(
            "sharky: agent: standard input is a terminal; the JSONL session protocol needs a pipe"
        );
        return 2;
    }

    // Nothing may be buffered into a descriptor that is about to be replaced.
    // `serve_with_wire` flushes stdio, moves the control descriptors and opens
    // the single null fd used for isolation. Do not duplicate any of that here:
    // a second /dev/null handle would stay open for the whole session.
    let input = match duplicate_high(0) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("sharky: agent: cannot move the control input: {err}");
            return 1;
        }
    };
    let output = match duplicate_high(1) {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(input);
            eprintln!("sharky: agent: cannot move the control output: {err}");
            return 1;
        }
    };
    serve_with_wire(options, input, output)
}

/// `sockaddr_un.sun_path` has room for 108 bytes including the final NUL on
/// Linux; bind rejects anything longer, and a clear usage error is cheaper
/// than letting the client wait for a listener that never appears.
const SUN_PATH_BYTES: usize = 108;

/// Owns the bound socket path until the single client is accepted. `Drop`
/// unlinks it on every exit path, including a fatal signal during `accept`.
struct SocketListener {
    path: PathBuf,
    listener: Option<UnixListener>,
    bound: bool,
}

impl SocketListener {
    fn bind(path: PathBuf) -> Result<Self, String> {
        let bytes = path.as_os_str().as_bytes();
        if bytes.is_empty() {
            return Err(String::from("socket path must not be empty"));
        }
        if bytes.len() >= SUN_PATH_BYTES {
            return Err(format!(
                "socket path is too long: {} bytes; the limit is {}",
                bytes.len(),
                SUN_PATH_BYTES - 1
            ));
        }
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "socket path already exists: {}; refusing to replace it",
                path.display()
            ));
        }
        let listener = UnixListener::bind(&path)
            .map_err(|err| format!("cannot bind socket {}: {err}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("cannot set socket permissions {}: {err}", path.display()))?;
        Ok(Self {
            path,
            listener: Some(listener),
            bound: true,
        })
    }

    /// Waits for one client, then unlinks the path and drops the listener so a
    /// second client cannot attach while the session runs.
    fn accept_one(&mut self) -> Result<OwnedFd, String> {
        let listener = self
            .listener
            .as_mut()
            .ok_or_else(|| String::from("socket listener is already closed"))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| format!("cannot set socket listener non-blocking: {err}"))?;
        loop {
            if let Some(signal) = crate::signal::fatal_signal() {
                return Err(format!("interrupted by signal {signal}"));
            }
            let mut descriptor = nix::libc::pollfd {
                fd: listener.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one descriptor owned by this process and a millisecond
            // timeout, so a signal cannot park the server indefinitely.
            let ready = unsafe { nix::libc::poll(&mut descriptor, 1, 50) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("cannot wait for a socket client: {err}"));
            }
            if ready == 0 {
                continue;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    self.unlink();
                    self.listener.take();
                    stream
                        .set_nonblocking(false)
                        .map_err(|err| format!("cannot use the accepted socket: {err}"))?;
                    return Ok(OwnedFd::from(stream));
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) => return Err(format!("cannot accept a socket client: {err}")),
            }
        }
    }

    fn unlink(&mut self) {
        if self.bound {
            let _ = std::fs::remove_file(&self.path);
            self.bound = false;
        }
    }
}

impl Drop for SocketListener {
    fn drop(&mut self) {
        self.unlink();
    }
}

fn run_socket(options: SessionOptions, path: PathBuf) -> i32 {
    // Install the same handlers `Shell::new` uses before binding, so a signal
    // that arrives while waiting for a client still runs the path guard.
    for number in [
        nix::libc::SIGINT,
        nix::libc::SIGTERM,
        nix::libc::SIGHUP,
    ] {
        crate::signal::fatal(number);
    }
    let mut listener = match SocketListener::bind(path) {
        Ok(listener) => listener,
        Err(message) => {
            eprintln!("sharky: agent: {message}");
            return 2;
        }
    };
    if options.multi_client {
        return run_socket_multi_client(options, &mut listener);
    }
    let source = match listener.accept_one() {
        Ok(stream) => stream,
        Err(message) => {
            if let Some(signal) = crate::signal::fatal_signal() {
                return 128 + signal;
            }
            drop(listener);
            eprintln!("sharky: agent: {message}");
            return 1;
        }
    };
    drop(listener);

    let input = match duplicate_high(source.as_raw_fd()) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("sharky: agent: cannot move the socket input: {err}");
            return 1;
        }
    };
    let output = match duplicate_high(source.as_raw_fd()) {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(input);
            eprintln!("sharky: agent: cannot move the socket output: {err}");
            return 1;
        }
    };
    drop(source);
    serve_with_wire(options, input, output)
}

fn run_socket_multi_client(options: SessionOptions, listener_guard: &mut SocketListener) -> i32 {
    let Some(listener) = listener_guard.listener.as_mut() else {
        return 1;
    };
    if let Err(err) = listener.set_nonblocking(true) {
        eprintln!("sharky: agent: cannot set socket listener non-blocking: {err}");
        return 1;
    }
    let _ = crate::signal::watch_children();
    loop {
        loop {
            match nix::sys::wait::waitpid(
                nix::unistd::Pid::from_raw(-1),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => break,
                Ok(_) => continue,
            }
        }
        if let Some(signal) = crate::signal::fatal_signal() {
            return 128 + signal;
        }
        let raw_listener_fd = listener_guard
            .listener
            .as_ref()
            .expect("listener present")
            .as_raw_fd();
        let mut descriptor = nix::libc::pollfd {
            fd: raw_listener_fd,
            events: nix::libc::POLLIN,
            revents: 0,
        };
        // SAFETY: valid listener fd and bounded 50ms timeout for signal/reap servicing.
        let ready = unsafe { nix::libc::poll(&mut descriptor, 1, 50) };
        if ready <= 0 {
            continue;
        }
        let accepted = match listener_guard.listener.as_ref().expect("listener").accept() {
            Ok((stream, _)) => stream,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                eprintln!("sharky: agent: cannot accept a socket client: {err}");
                continue;
            }
        };
        // SAFETY: each accepted connection is isolated in a forked worker child.
        match unsafe { nix::unistd::fork() } {
            Ok(nix::unistd::ForkResult::Parent { .. }) => {
                drop(accepted);
            }
            Ok(nix::unistd::ForkResult::Child) => {
                // Child must NOT unlink the shared socket path when dropping SocketListener.
                listener_guard.bound = false;
                listener_guard.listener.take();
                let _ = accepted.set_nonblocking(false);
                let fd = accepted.as_raw_fd();
                let input = match duplicate_high(fd) {
                    Ok(d) => d,
                    Err(_) => unsafe { nix::libc::_exit(1) },
                };
                let output = match duplicate_high(fd) {
                    Ok(d) => d,
                    Err(_) => unsafe { nix::libc::_exit(1) },
                };
                drop(accepted);
                let code = serve_with_wire(options, input, output);
                unsafe { nix::libc::_exit(code) }
            }
            Err(err) => {
                drop(accepted);
                eprintln!("sharky: agent: fork worker failed: {err}");
            }
        }
    }
}

/// Shared transport-to-session setup: move the control descriptors to high
/// numbers, isolate 0/1/2, install the supervisor and serve. Both `--stdio`
/// and `--socket` end here so the evaluator and request loop exist once.
fn serve_with_wire(options: SessionOptions, input: RawFd, output: RawFd) -> i32 {
    // The same warnings `Shell::new` emits for non-agent invocations must reach
    // the real stderr here: after isolation, fd 2 points at /dev/null. Keep the
    // command-line argument warning in `cli::run`, which runs before dispatch.
    let mut env_warned = false;
    for (key, value) in std::env::vars_os() {
        let kind = if crate::cli::is_non_utf8(&key) {
            Some("environment variable name")
        } else if crate::cli::is_non_utf8(&value) {
            Some("environment variable")
        } else {
            None
        };
        if let Some(kind) = kind
            && !env_warned
        {
            crate::cli::warn_non_utf8(kind);
            env_warned = true;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        crate::cli::warn_if_non_utf8(cwd.as_os_str(), "working directory");
    }

    capture::flush_stdio();

    let null_fd = match open_null() {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(input);
            close_fd(output);
            eprintln!("sharky: agent: /dev/null: {err}");
            return 1;
        }
    };
    let (input_identity, output_identity) =
        match (capture::identity(input), capture::identity(output)) {
            (Ok(input_identity), Ok(output_identity)) => (input_identity, output_identity),
            (input_result, output_result) => {
                close_fd(null_fd);
                close_fd(input);
                close_fd(output);
                let err = input_result
                    .err()
                    .or(output_result.err())
                    .unwrap_or_else(|| {
                        io::Error::other("control descriptor identity is unavailable")
                    });
                eprintln!("sharky: agent: {err}");
                return 1;
            }
        };

    // From here on, stdio is /dev/null: nothing a command writes can reach the
    // protocol stream, and nothing the client writes can look like command
    // input.
    if let Err(err) = redirect_stdio_to_null(null_fd) {
        close_fd(null_fd);
        close_fd(input);
        close_fd(output);
        eprintln!("sharky: agent: cannot isolate stdio: {err}");
        return 1;
    }

    // Configuration is loaded after isolation, so a config diagnostic can never
    // become an unframed line on the control stream.
    if let Err(error) = supervisor::install(input, output, options.limits) {
        close_fd(null_fd);
        close_fd(input);
        close_fd(output);
        eprintln!("sharky: agent: cannot install the session supervisor: {error}");
        return 1;
    }
    // The self-pipe exists now; the handler only writes a wake-up byte.
    crate::signal::watch_children();

    let mut session = match Session::new_headless(&options, null_fd) {
        Ok(session) => session,
        Err(err) => {
            close_fd(null_fd);
            close_fd(input);
            close_fd(output);
            eprintln!("sharky: agent: {err}");
            return 1;
        }
    };
    session.wire = Some(Wire {
        input,
        output,
        input_identity,
        output_identity,
    });
    session.serve()
}

/// Dispatches one-shot `sharky code ...` before shell startup.
pub(crate) fn try_code_cli(args: &[String]) -> Option<i32> {
    use std::io::Read;
    use std::os::fd::AsRawFd;

    if args.first().map(String::as_str) != Some("code") {
        return None;
    }
    let tail = &args[1..];
    if tail.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        if let Some(text) = crate::help::topic("code") {
            crate::help::print_text(&text);
        }
        return Some(0);
    }

    let mut program: Option<String> = None;
    let mut script_path: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut timeout_ms: Option<i64> = None;
    let mut max_steps: Option<usize> = None;
    let mut max_calls: Option<usize> = None;
    let mut args_val = Value::Null;
    let mut no_config = false;
    let mut redact_env = false;
    let limits = Limits::default();

    let mut i = 0;
    while i < tail.len() {
        let arg = tail[i].as_str();
        match arg {
            "--no-config" => no_config = true,
            "--redact-env" => redact_env = true,
            "-e" => {
                let Some(val) = tail.get(i + 1) else {
                    eprintln!("sharky: code: -e requires a program string");
                    return Some(2);
                };
                if program.is_some() || script_path.is_some() {
                    eprintln!("sharky: code: specify only one of -e, FILE, or -");
                    return Some(2);
                }
                program = Some(val.clone());
                i += 1;
            }
            "--cwd" => {
                let Some(val) = tail.get(i + 1) else {
                    eprintln!("sharky: code: --cwd requires a path");
                    return Some(2);
                };
                if val.is_empty() {
                    eprintln!("sharky: code: --cwd must not be empty");
                    return Some(2);
                }
                cwd = Some(val.clone());
                i += 1;
            }
            "--timeout-ms" => {
                let Some(val) = tail.get(i + 1) else {
                    eprintln!("sharky: code: --timeout-ms requires milliseconds");
                    return Some(2);
                };
                match val.parse::<i64>() {
                    Ok(ms) if ms >= 0 => timeout_ms = Some(ms),
                    _ => {
                        eprintln!("sharky: code: --timeout-ms must be a non-negative integer");
                        return Some(2);
                    }
                }
                i += 1;
            }
            "--max-steps" => {
                let Some(val) = tail.get(i + 1) else {
                    eprintln!("sharky: code: --max-steps requires an integer");
                    return Some(2);
                };
                match val.parse::<usize>() {
                    Ok(n) if (1..=limits.max_code_steps).contains(&n) => max_steps = Some(n),
                    _ => {
                        eprintln!(
                            "sharky: code: --max-steps must be between 1 and {}",
                            limits.max_code_steps
                        );
                        return Some(2);
                    }
                }
                i += 1;
            }
            "--max-calls" => {
                let Some(val) = tail.get(i + 1) else {
                    eprintln!("sharky: code: --max-calls requires an integer");
                    return Some(2);
                };
                match val.parse::<usize>() {
                    Ok(n) if (1..=limits.max_code_calls).contains(&n) => max_calls = Some(n),
                    _ => {
                        eprintln!(
                            "sharky: code: --max-calls must be between 1 and {}",
                            limits.max_code_calls
                        );
                        return Some(2);
                    }
                }
                i += 1;
            }
            "--args" => {
                let Some(raw) = tail.get(i + 1) else {
                    eprintln!("sharky: code: --args requires JSON");
                    return Some(2);
                };
                match json::parse_line(raw.as_bytes()) {
                    Ok(parsed) => args_val = parsed,
                    Err(err) => {
                        eprintln!("sharky: code: invalid --args JSON: {err}");
                        return Some(2);
                    }
                }
                i += 1;
            }
            "-" => {
                if program.is_some() || script_path.is_some() {
                    eprintln!("sharky: code: specify only one of -e, FILE, or -");
                    return Some(2);
                }
                script_path = Some(String::from("-"));
            }
            other if other.starts_with('-') => {
                eprintln!("sharky: code: unexpected option '{other}'");
                return Some(2);
            }
            other => {
                if program.is_some() || script_path.is_some() {
                    eprintln!("sharky: code: unexpected argument '{other}'");
                    return Some(2);
                }
                script_path = Some(other.to_string());
            }
        }
        i += 1;
    }

    let source = match (program, script_path.as_deref()) {
        (Some(p), None) => p,
        (None, Some("-")) => {
            let mut text = String::new();
            if let Err(err) = io::stdin().read_to_string(&mut text) {
                eprintln!("sharky: code: failed to read stdin: {err}");
                return Some(1);
            }
            text
        }
        (None, Some(path)) => match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("sharky: code: {path}: {err}");
                return Some(2);
            }
        },
        _ => {
            eprintln!("sharky: code: pass -e PROGRAM, FILE, or -");
            return Some(2);
        }
    };

    if source.trim().is_empty() {
        eprintln!("sharky: code: program must not be empty");
        return Some(2);
    }

    capture::flush_stdio();
    let null_fd = match open_null() {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("sharky: code: /dev/null: {err}");
            return Some(1);
        }
    };
    let real_stdout = match duplicate_high(1) {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(null_fd);
            eprintln!("sharky: code: cannot duplicate stdout: {err}");
            return Some(1);
        }
    };
    let (dummy_read, dummy_write) = match nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC) {
        Ok(pair) => pair,
        Err(err) => {
            close_fd(null_fd);
            close_fd(real_stdout);
            eprintln!("sharky: code: pipe: {err}");
            return Some(1);
        }
    };

    for target in [0, 1, 2] {
        if let Err(err) = capture::dup2_onto(null_fd, target) {
            close_fd(null_fd);
            close_fd(real_stdout);
            eprintln!("sharky: code: cannot isolate stdio: {err}");
            return Some(1);
        }
    }
    capture::flush_stdio();

    if let Err(err) = supervisor::install(dummy_read.as_raw_fd(), -1, limits) {
        close_fd(null_fd);
        close_fd(real_stdout);
        eprintln!("sharky: code: cannot install supervisor: {err}");
        return Some(1);
    }
    supervisor::close_in_children(&[dummy_write.as_raw_fd(), real_stdout]);
    crate::signal::watch_children();

    let mut shell = crate::shell::Shell::new();
    shell.load_config(no_config);
    shell.set_command_string(false);
    shell.set_session_mode(true);
    shell.set_isolate_groups(true);
    if let Some(dir) = &cwd
        && let Err(err) = shell.enter_task_directory(dir)
    {
        cleanup(supervisor::CLEANUP_WAIT_MS);
        let msg = format!("cannot change directory to {dir}: {err}");
        let mut res = json::CodeResult::new("cli");
        res.status = 2;
        res.error = Some(msg);
        let mut line = Response::Code(res).encode();
        line.push('\n');
        let _ = capture::write_safely(real_stdout, line.as_bytes());
        close_fd(real_stdout);
        close_fd(null_fd);
        drop(dummy_read);
        drop(dummy_write);
        return Some(2);
    }

    let request = protocol::CodeRequest {
        id: String::from("cli"),
        source,
        args: args_val,
        timeout_ms,
        max_steps: max_steps
            .unwrap_or_else(|| json::DEFAULT_CODE_MAX_STEPS.min(limits.max_code_steps)),
        max_calls: max_calls
            .unwrap_or_else(|| json::DEFAULT_CODE_MAX_CALLS.min(limits.max_code_calls)),
    };

    let mut session = Session {
        wire: None,
        null_fd,
        shell,
        limits,
        captures: store::CaptureStore::new(&limits),
        redact_env,
        state_file: None,
        exit_status: None,
        cleanup_wait_ms: supervisor::CLEANUP_WAIT_MS,
        snapshots: std::collections::HashMap::new(),
        snapshot_order: Vec::new(),
        next_snapshot_id: 1,
        jobs: jobs::JobManager::new(),
        policy: None,
    };

    let result = code::execute(&mut session, &request);
    cleanup(session.cleanup_wait_ms);
    let _ = session.finish(Some(result.status as i32));

    let exit_code = match result.termination {
        Termination::Timeout => 124,
        Termination::Cancelled => 143,
        _ if result.error.is_some() => {
            if result.status == 2 {
                2
            } else {
                1
            }
        }
        _ => 0,
    };

    let mut line = Response::Code(result).encode();
    line.push('\n');
    let _ = capture::write_safely(real_stdout, line.as_bytes());
    close_fd(real_stdout);
    close_fd(null_fd);
    drop(dummy_read);
    drop(dummy_write);
    Some(exit_code)
}

pub(super) fn redirect_stdio_to_null(null_fd: RawFd) -> io::Result<()> {
    for target in [0, 1, 2] {
        capture::dup2_onto(null_fd, target)?;
    }
    capture::flush_stdio();
    Ok(())
}

pub(super) fn open_null() -> io::Result<RawFd> {
    let path = c"/dev/null";
    // SAFETY: the path is a valid NUL-terminated C string and no other argument
    // is involved; the returned number is a fresh descriptor this process owns.
    let fd = unsafe { nix::libc::open(path.as_ptr(), nix::libc::O_RDWR | nix::libc::O_CLOEXEC) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(fd)
    }
}

/// `F_DUPFD_CLOEXEC` from `fd`, so the copy is invisible to every `execve`.
pub(super) fn duplicate_high(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `fcntl` reads and duplicates this process's own descriptor.
    let copy = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, CONTROL_FD_MIN) };
    if copy < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(copy)
    }
}

pub(super) fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: the caller owns `fd` and closes it exactly once.
        unsafe {
            nix::libc::close(fd);
        }
    }
}

// ------------------------------------------------------------------- session

/// The control channel: the high-numbered descriptors plus the identity their
/// files had at startup.
pub(super) struct Wire {
    input: RawFd,
    output: RawFd,
    input_identity: capture::Identity,
    output_identity: capture::Identity,
}

pub(super) struct Session {
    pub(super) wire: Option<Wire>,
    pub(super) null_fd: RawFd,
    pub(super) shell: crate::shell::Shell,
    pub(super) limits: Limits,
    pub(super) captures: store::CaptureStore,
    pub(super) redact_env: bool,
    pub(super) state_file: Option<PathBuf>,
    pub(super) exit_status: Option<i32>,
    /// Grace for the shutdown cleanup; `close.wait_ms` overrides the default.
    pub(super) cleanup_wait_ms: i64,
    pub(super) snapshots: std::collections::HashMap<String, crate::shell::ShellSnapshot>,
    pub(super) snapshot_order: Vec<String>,
    pub(super) next_snapshot_id: u64,
    pub(super) jobs: jobs::JobManager,
    pub(super) policy: Option<Arc<policy::Policy>>,
}

pub(super) enum RunOutcome<T> {
    Ok(T),
    FatalExit(i32),
    FatalError(String),
}

impl Session {
    pub(super) fn new_headless(options: &SessionOptions, null_fd: RawFd) -> Result<Self, String> {
        let mut shell = crate::shell::Shell::new();
        shell.load_config(options.no_config);
        shell.set_command_string(false);
        shell.set_session_mode(true);
        shell.set_isolate_groups(true);

        let mut session = Self {
            wire: None,
            null_fd,
            shell,
            limits: options.limits,
            captures: store::CaptureStore::new(&options.limits),
            redact_env: options.redact_env,
            state_file: options.state_file.clone(),
            exit_status: None,
            cleanup_wait_ms: supervisor::CLEANUP_WAIT_MS,
            snapshots: std::collections::HashMap::new(),
            snapshot_order: Vec::new(),
            next_snapshot_id: 1,
            jobs: jobs::JobManager::new(),
            policy: options.policy.clone(),
        };
        if let Some(path) = options.state_file.clone() {
            session.load_state_file(&path)?;
        }
        Ok(session)
    }

    pub(super) fn enforce_target_policy(
        &self,
        target: &protocol::ExecTarget,
        sandbox: &mut Option<protocol::SandboxSpec>,
    ) -> Result<(), (ErrorCode, String)> {
        let Some(ref policy) = self.policy else {
            return Ok(());
        };
        match policy.evaluate_target(target) {
            policy::PolicyDecision::Allow => Ok(()),
            policy::PolicyDecision::ForceSandbox => {
                if sandbox.is_none() {
                    *sandbox = Some(policy::default_forced_sandbox());
                }
                Ok(())
            }
            policy::PolicyDecision::Deny { id, message } => {
                Err((ErrorCode::PolicyDenied, format!("policy rule {id}: {message}")))
            }
            policy::PolicyDecision::RequireApproval { id, message } => Err((
                ErrorCode::RequiresApproval,
                format!("policy rule {id}: {message}"),
            )),
        }
    }

    pub(super) fn create_snapshot(&mut self, custom_id: Option<&str>) -> (String, String) {
        const MAX_SNAPSHOTS: usize = 32;
        let id = match custom_id {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                let s = format!("snap-{}", self.next_snapshot_id);
                self.next_snapshot_id += 1;
                s
            }
        };
        let snap = self.shell.take_snapshot();
        let cwd = self.shell.cwd.to_string_lossy().into_owned();
        if let Some(pos) = self.snapshot_order.iter().position(|k| k == &id) {
            self.snapshot_order.remove(pos);
        }
        self.snapshots.insert(id.clone(), snap);
        self.snapshot_order.push(id.clone());
        while self.snapshot_order.len() > MAX_SNAPSHOTS {
            let oldest = self.snapshot_order.remove(0);
            self.snapshots.remove(&oldest);
        }
        (id, cwd)
    }

    pub(super) fn restore_snapshot(&mut self, id: &str) -> Result<String, String> {
        let Some(snap) = self.snapshots.get(id).cloned() else {
            return Err(format!("unknown snapshot_id: {id}"));
        };
        self.shell.restore_snapshot(&snap);
        self.save_state_file();
        Ok(self.shell.cwd.to_string_lossy().into_owned())
    }

    pub(super) fn load_state_file(&mut self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let bytes = std::fs::read(path)
            .map_err(|err| format!("cannot read state file {}: {err}", path.display()))?;
        let value = json::parse_line(&bytes)
            .map_err(|err| format!("invalid state file {}: {err}", path.display()))?;
        let Some(entries) = value.as_object() else {
            return Err(format!("state file {} must contain a JSON object", path.display()));
        };
        if let Some(vars) = entries
            .iter()
            .find(|(k, _)| k == "variables")
            .and_then(|(_, v)| v.as_object())
        {
            for (name, val) in vars {
                if !matches!(name.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "_")
                    && let Some(text) = val.as_str()
                {
                    let _ = self.shell.set_var(name, text.to_string());
                }
            }
        }
        if let Some(env) = entries
            .iter()
            .find(|(k, _)| k == "env")
            .and_then(|(_, v)| v.as_object())
        {
            for (name, val) in env {
                if !matches!(name.as_str(), "PWD" | "OLDPWD" | "SHLVL" | "_")
                    && let Some(text) = val.as_str()
                {
                    self.shell.set_task_env(name, Some(text.to_string()));
                }
            }
        }
        if let Some(aliases) = entries
            .iter()
            .find(|(k, _)| k == "aliases")
            .and_then(|(_, v)| v.as_object())
        {
            for (name, val) in aliases {
                if let Some(text) = val.as_str() {
                    self.shell.set_alias(name.clone(), text.to_string());
                }
            }
        }
        if let Some(cwd) = entries
            .iter()
            .find(|(k, _)| k == "cwd")
            .and_then(|(_, v)| v.as_str())
        {
            let _ = self.shell.enter_task_directory(cwd);
        }
        Ok(())
    }

    pub(super) fn save_state_file(&self) {
        let Some(path) = &self.state_file else {
            return;
        };
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let env_obj = Value::Object(
            self.shell
                .snapshot_env()
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
        let vars_obj = Value::Object(
            self.shell
                .snapshot_variables()
                .into_iter()
                .filter_map(|(name, value, exported, _)| {
                    if !exported {
                        value.map(|v| (name, Value::String(v)))
                    } else {
                        None
                    }
                })
                .collect(),
        );
        let aliases_obj = Value::Object(
            self.shell
                .snapshot_aliases()
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        );
        let state = Value::Object(vec![
            (String::from("schema_version"), Value::Int(1)),
            (
                String::from("cwd"),
                Value::String(self.shell.cwd.to_string_lossy().into_owned()),
            ),
            (String::from("env"), env_obj),
            (String::from("variables"), vars_obj),
            (String::from("aliases"), aliases_obj),
        ]);
        let mut encoded = state.encode();
        encoded.push('\n');

        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| String::from("sharky-state"));
        let tmp_name = format!(".{file_name}.tmp.{}", std::process::id());
        let tmp_path = path.with_file_name(tmp_name);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
        {
            use std::io::Write;
            if file.write_all(encoded.as_bytes()).is_ok() && file.flush().is_ok() {
                drop(file);
                let _ = std::fs::rename(&tmp_path, path);
            } else {
                let _ = std::fs::remove_file(&tmp_path);
            }
        }
    }

    fn serve(mut self) -> i32 {
        let mut ready = Ready::new(session_name(), i64::from(std::process::id()), VERSION);
        // One contract for `ready` and the decoder: the op and feature lists
        // are built with the flags in `json`, so `ready` never advertises
        // something the driver refuses (design §3.4/§D10).
        ready.protocol = ProtocolInfo::new(self.limits);
        if !self.write(&Response::Ready(ready)) {
            return self.end(None, None);
        }

        loop {
            if let Some(sig) = crate::signal::fatal_signal() {
                return self.end(Some(128 + sig), None);
            }
            // Frames the service pump read during an `exec` come first, so the
            // responses keep the order the requests were accepted in.
            match next_frame() {
                Ok(Frame::Line(line)) => {
                    if let Some(code) = self.handle_line(&line) {
                        return code;
                    }
                    if let Some(sig) = crate::signal::fatal_signal() {
                        return self.end(Some(128 + sig), None);
                    }
                    // A service-pump ack that hit EPIPE means the client is
                    // gone even though its input side may still be open.
                    if supervisor::client_gone() {
                        return self.end(None, None);
                    }
                    // Notices go to fd 1, which is /dev/null between requests;
                    // the call is here to keep the job table from growing.
                    self.shell.reap_jobs();
                }
                Ok(Frame::TooLarge) => {
                    let message =
                        format!("request line exceeds {} bytes", self.limits.max_line_bytes);
                    if !self.report_error(None, ErrorCode::MessageTooLarge, message) {
                        return self.end(None, None);
                    }
                }
                Ok(Frame::Eof { partial }) => {
                    if let Some(line) = partial {
                        // JSONL readers accept a final line without a newline.
                        match json::parse_line(&line) {
                            Ok(_) => {
                                if let Some(code) = self.handle_line(&line) {
                                    return code;
                                }
                            }
                            Err(error) => {
                                self.report_error(
                                    None,
                                    ErrorCode::PartialRequest,
                                    format!("end of input in the middle of a request: {error}"),
                                );
                                return self.end(Some(1), None);
                            }
                        }
                    }
                    if let Some(sig) = crate::signal::fatal_signal() {
                        return self.end(Some(128 + sig), None);
                    }
                    return self.end(None, None);
                }
                Err(error) => {
                    if let Some(sig) = crate::signal::fatal_signal() {
                        return self.end(Some(128 + sig), None);
                    }
                    self.report_error(
                        None,
                        ErrorCode::Internal,
                        format!("control channel read failed: {error}"),
                    );
                    return self.end(Some(1), None);
                }
            }
        }
    }

    // ---- requests ---------------------------------------------------------

    /// Handles one line. `Some(code)` means the session ended and the process
    /// should exit with that status.
    fn handle_line(&mut self, line: &[u8]) -> Option<i32> {
        let value = match json::parse_line(line) {
            Ok(value) => value,
            Err(error) => {
                if !self.report_error(None, ErrorCode::InvalidJson, error.to_string()) {
                    return Some(self.end(None, None));
                }
                return None;
            }
        };

        if let Some(id) = protocol::request_id(&value, &self.limits) {
            if supervisor::id_seen(&id) {
                let message = format!("id {id} was already used on this connection");
                if !self.report_error(Some(&id), ErrorCode::DuplicateId, message) {
                    return Some(self.end(None, None));
                }
                return None;
            }
        }

        let request = match protocol::decode(&value, &self.limits) {
            Ok(request) => request,
            Err(error) => {
                if !self.report_error(error.id.as_deref(), error.code, error.message) {
                    return Some(self.end(None, None));
                }
                return None;
            }
        };
        // `handle_cancel` touches the cancel id after `resolve_cancel`, so an
        // idle self-targeting cancel reports `CANCEL_UNKNOWN_ID` instead of
        // mistaking its own id for an already-finished request.
        if !matches!(request, DecodedRequest::Cancel(_)) {
            supervisor::touch_id(request.id());
        }

        match request {
            DecodedRequest::Exec(request) => self.exec(&request),
            DecodedRequest::State(request) => self.state(&request),
            DecodedRequest::Cancel(request) => self.cancel(&request),
            DecodedRequest::Close(request) => Some(self.close(&request)),
            DecodedRequest::Parallel(request) => self.parallel(&request),
            DecodedRequest::Workflow(request) => self.workflow(&request),
            DecodedRequest::ReadCapture(request) => self.read_capture(&request),
            DecodedRequest::Code(request) => self.code(&request),
        }
    }

    fn code(&mut self, request: &protocol::CodeRequest) -> Option<i32> {
        let result = if take_queued_cancelled(&request.id) {
            let mut res = json::CodeResult::new(request.id.clone());
            res.status = 130;
            res.termination = Termination::Cancelled;
            res
        } else {
            code::execute(self, request)
        };
        if !self.wire_intact() {
            return Some(self.end(Some(1), None));
        }
        if let Some(sig) = crate::signal::fatal_signal() {
            let _ = self.write(&Response::Code(result));
            return Some(self.end(Some(128 + sig), None));
        }
        if let Some(code) = self.shell.exit_requested.take() {
            if let Some(exit) = self.deliver(&Response::Code(result)) {
                return Some(exit);
            }
            return Some(self.end(Some(code), Some(None)));
        }
        self.deliver(&Response::Code(result))
    }

    fn read_capture(&mut self, request: &protocol::ReadCaptureRequest) -> Option<i32> {
        match self.captures.read(request, &self.limits) {
            Ok(result) => self.deliver(&Response::Capture(result)),
            Err((code, message)) => {
                if !self.report_error(Some(&request.id), code, message) {
                    return Some(self.end(None, None));
                }
                None
            }
        }
    }

    /// Runs one `op:"parallel"` request. The group executes in forked children
    /// (design §3), so the session's `Shell` is never borrowed while a task
    /// runs and this driver remains the only `waitpid` consumer; the group's
    /// own loop pumps the supervisor, which is the single place `cancel`,
    /// per-task deadlines and `close`/EOF act from (design §5).
    ///
    /// The parent state is untouched by construction: a task's assignments,
    /// `cd`, exports and traps die with its child. A request cancelled while it
    /// was queued behind another one never spawns and is answered with every
    /// task `cancelled`/`130` (`reason:"group_cancel"`), the same parity `exec`
    /// gives a queued cancel.
    fn parallel(&mut self, request: &protocol::ParallelRequest) -> Option<i32> {
        let mut req = request.clone();
        for task in &mut req.tasks {
            if let Err((code, message)) =
                self.enforce_target_policy(&task.target, &mut task.sandbox)
            {
                if !self.report_error(Some(&request.id), code, message) {
                    return Some(self.end(None, None));
                }
                return None;
            }
        }
        let mut result = if take_queued_cancelled(&req.id) {
            parallel::cancelled(&req, self.shell.cwd.to_string_lossy().into_owned())
        } else {
            match parallel::run(
                &mut self.shell,
                self.null_fd,
                &self.limits,
                &mut self.captures,
                &req,
            ) {
                Ok(result) => result,
                Err(message) => return self.internal(message),
            }
        };
        if self.redact_env {
            for task in &mut result.tasks {
                if let Some(delta) = task.env_delta.take() {
                    task.env_delta = Some(redact_delta(delta, true));
                }
            }
        }
        self.deliver_group(Response::Parallel(result))
    }

    /// Runs one validated `op:"workflow"` DAG request on the shared attempt and
    /// supervisor machinery.
    fn workflow(&mut self, request: &protocol::WorkflowRequest) -> Option<i32> {
        let mut req = request.clone();
        for task in &mut req.tasks {
            if let Err((code, message)) =
                self.enforce_target_policy(&task.base.target, &mut task.base.sandbox)
            {
                if !self.report_error(Some(&request.id), code, message) {
                    return Some(self.end(None, None));
                }
                return None;
            }
        }
        let mut result = if take_queued_cancelled(&req.id) {
            workflow::cancelled(&req, self.shell.cwd.to_string_lossy().into_owned())
        } else {
            match workflow::run(
                &mut self.shell,
                self.null_fd,
                &self.limits,
                &mut self.captures,
                &req,
            ) {
                Ok(result) => result,
                Err(message) => return self.internal(message),
            }
        };
        if self.redact_env {
            for task in &mut result.tasks {
                if let Some(delta) = task.base.env_delta.take() {
                    task.base.env_delta = Some(redact_delta(delta, true));
                }
            }
        }
        self.deliver_group(Response::Workflow(result))
    }

    fn deliver_group(&mut self, response: Response) -> Option<i32> {
        // Tasks cannot reach the control descriptors, but a capture read is the
        // only error that got here, and it already ended the request: keep the
        // same wire check `exec` does so a broken channel is never answered on.
        if !self.wire_intact() {
            return Some(self.end(Some(1), None));
        }
        if let Some(sig) = crate::signal::fatal_signal() {
            let _ = self.write(&response);
            return Some(self.end(Some(128 + sig), None));
        }
        self.deliver(&response)
    }

    fn exec(&mut self, request: &protocol::ExecRequest) -> Option<i32> {
        // A request cancelled while it was queued never runs; its result was
        // promised at cancel time (design §8.2).
        if take_queued_cancelled(&request.id) {
            let mut result = ExecResult::new(
                request.id.clone(),
                130,
                0,
                self.shell.cwd.to_string_lossy().into_owned(),
            );
            result.termination = Termination::Cancelled;
            if request.env_delta {
                result.env_delta = Some(Vec::new());
            }
            return self.deliver(&Response::Result(result));
        }

        let mut req = request.clone();
        if let Err((code, message)) = self.enforce_target_policy(&req.target, &mut req.sandbox) {
            if !self.report_error(Some(&req.id), code, message) {
                return Some(self.end(None, None));
            }
            return None;
        }
        let result = match self.run_exec_internal(&req) {
            RunOutcome::Ok(result) => result,
            RunOutcome::FatalExit(code) => return Some(self.end(Some(code), None)),
            RunOutcome::FatalError(message) => return self.internal(message),
        };

        // `exit` and a `set -e` abort end the session after the result: the
        // real status is reported, then the EXIT action runs once.
        if let Some(code) = self.shell.exit_requested.take() {
            if let Some(exit) = self.deliver(&Response::Result(result)) {
                return Some(exit);
            }
            return Some(self.end(Some(code), Some(None)));
        }
        self.deliver(&Response::Result(result))
    }

    pub(super) fn run_exec_internal(&mut self, request: &protocol::ExecRequest) -> RunOutcome<ExecResult> {
        if request.isolated || request.tty || request.sandbox.is_some() || request.stream {
            return self.run_isolated_exec_internal(request);
        }

        let before = if request.env_delta {
            self.shell.snapshot_env()
        } else {
            Vec::new()
        };
        let fs_before = if request.fs_delta {
            let start_dir = self.shell.cwd.clone();
            Some((start_dir.clone(), snapshot_fs(&start_dir)))
        } else {
            None
        };

        if request.lock.is_none()
            && matches!(&request.target, protocol::ExecTarget::Command(command) if command.trim().is_empty())
        {
            let mut result = ExecResult::new(
                request.id.clone(),
                0,
                0,
                self.shell.cwd.to_string_lossy().into_owned(),
            );
            if request.env_delta {
                result.env_delta = Some(Vec::new());
            }
            if request.fs_delta {
                result.fs_delta = Some(FsDelta {
                    created: Vec::new(),
                    modified: Vec::new(),
                    removed: Vec::new(),
                    truncated: false,
                });
            }
            return RunOutcome::Ok(result);
        }

        crate::signal::clear_interrupt();
        begin_request(&request.id, request.timeout_ms);
        let _lock = match self.acquire_exec_lock(request) {
            Ok(lock) => lock,
            Err(outcome) => return outcome,
        };

        let stdout = match Capture::create("agent-out") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = end_request();
                return RunOutcome::FatalError(format!("cannot create the stdout capture: {error}"));
            }
        };
        let stderr = match Capture::create("agent-err") {
            Ok(capture) => capture,
            Err(error) => {
                let _ = end_request();
                return RunOutcome::FatalError(format!("cannot create the stderr capture: {error}"));
            }
        };
        let payload = match &request.stdin {
            None => None,
            Some(bytes) => match Capture::create("agent-in").and_then(|capture| {
                capture.fill(bytes)?;
                Ok(capture)
            }) {
                Ok(capture) => Some(capture),
                Err(error) => {
                    let _ = end_request();
                    return RunOutcome::FatalError(format!("cannot create the stdin payload: {error}"));
                }
            },
        };

        let mut guard = match StdioGuard::new(self.null_fd) {
            Ok(guard) => guard,
            Err(error) => {
                let _ = end_request();
                return RunOutcome::FatalError(format!("cannot record the request descriptors: {error}"));
            }
        };
        let swap: io::Result<()> = (|| {
            match &payload {
                Some(capture) => guard.swap(0, capture.fd())?,
                None => guard.swap(0, self.null_fd)?,
            }
            guard.swap_output(1, stdout.fd())?;
            guard.swap_output(2, stderr.fd())?;
            Ok(())
        })();
        if let Err(error) = swap {
            let _ = end_request();
            let _ = guard.restore();
            return RunOutcome::FatalError(format!("cannot isolate the request descriptors: {error}"));
        }

        let status = match &request.target {
            protocol::ExecTarget::Command(command) if command.trim().is_empty() => 0,
            protocol::ExecTarget::Command(command) => {
                let outcome = self.shell.session_eval(command);
                self.shell.close_substitutions_from(0);
                match &outcome {
                    Ok(status) => *status,
                    Err(error) => {
                        let line = format!("sharky: {}\n", error.message);
                        let _ = crate::term::write_stderr(&line);
                        2
                    }
                }
            }
            protocol::ExecTarget::Argv(argv) => {
                let status = self.shell.session_exec_argv(argv);
                self.shell.close_substitutions_from(0);
                status
            }
        };
        let duration_ms = self.shell.last_duration_ms;
        let pgid = request_group();
        let supervised = end_request();
        let (status, termination) = match supervised {
            Some(supervisor::Outcome::Cancelled) => (143, Termination::Cancelled),
            Some(supervisor::Outcome::Timeout) => (124, Termination::Timeout),
            None => (status, Termination::Exited),
        };

        if let Err(error) = guard.restore() {
            return RunOutcome::FatalError(format!("cannot restore stdio after the request: {error}"));
        }
        if let Some(sig) = crate::signal::fatal_signal() {
            return RunOutcome::FatalExit(128 + sig);
        }

        let mut exec_budget = usize::MAX;
        let mut ignored_trunc = false;
        let (stdout, out_proj, raw_stdout) = match projection::read_capture_for_projection(
            &stdout,
            request.output.as_ref(),
            self.limits.max_capture_bytes,
            &mut exec_budget,
            &mut ignored_trunc,
        ) {
            Ok(parts) => parts,
            Err(error) => {
                return RunOutcome::FatalError(format!("cannot read the stdout capture: {error}"));
            }
        };
        exec_budget = usize::MAX;
        let (stderr, err_proj, raw_stderr) = match projection::read_capture_for_projection(
            &stderr,
            request.output.as_ref(),
            self.limits.max_capture_bytes,
            &mut exec_budget,
            &mut ignored_trunc,
        ) {
            Ok(parts) => parts,
            Err(error) => {
                return RunOutcome::FatalError(format!("cannot read the stderr capture: {error}"));
            }
        };

        if !self.wire_intact() {
            return RunOutcome::FatalExit(1);
        }

        let parsed = request
            .output
            .as_ref()
            .and_then(|o| o.parse)
            .map(|parser| {
                projection::parse_structured_output(parser, &raw_stdout.bytes, &raw_stderr.bytes)
            });
        let fs_delta = fs_before
            .as_ref()
            .map(|(start_dir, (before_map, before_trunc))| {
                let (after_map, after_trunc) = snapshot_fs(start_dir);
                diff_fs_snapshot(before_map, *before_trunc, &after_map, after_trunc)
            });

        let mut result = ExecResult::new(
            request.id.clone(),
            i64::from(status),
            duration_ms as i64,
            self.shell.cwd.to_string_lossy().into_owned(),
        );
        result.termination = termination;
        result.duration_us = self.shell.last_duration_us as i64;
        result.pgid = pgid;
        result.stdout = stdout;
        result.stderr = stderr;
        result.capture_id = self.captures.record(raw_stdout, raw_stderr);
        result.projection = projection::build_projection_summary(out_proj, err_proj);
        result.parsed = parsed;
        result.fs_delta = fs_delta;
        if request.env_delta {
            result.env_delta = Some(redact_delta(
                self.shell.env_delta(&before),
                self.redact_env,
            ));
        }
        drop(_lock);
        self.save_state_file();

        RunOutcome::Ok(result)
    }

    #[allow(clippy::result_large_err)]
    fn acquire_exec_lock(
        &mut self,
        request: &protocol::ExecRequest,
    ) -> Result<Option<capture::CrossSessionLock>, RunOutcome<ExecResult>> {
        let Some(ref lock_name) = request.lock else {
            return Ok(None);
        };
        let cwd = self.shell.cwd.to_string_lossy().into_owned();
        loop {
            match capture::CrossSessionLock::try_acquire(lock_name) {
                Ok(Some(lock)) => return Ok(Some(lock)),
                Ok(None) => {
                    supervisor::service();
                    if let Some(sig) = crate::signal::fatal_signal() {
                        let _ = end_request();
                        return Err(RunOutcome::FatalExit(128 + sig));
                    }
                    if let Some(outcome) = supervisor::pending_outcome() {
                        let _ = end_request();
                        let (status, termination) = match outcome {
                            supervisor::Outcome::Cancelled => (143, Termination::Cancelled),
                            supervisor::Outcome::Timeout => (124, Termination::Timeout),
                        };
                        let mut result = ExecResult::new(request.id.clone(), status, 0, cwd);
                        result.termination = termination;
                        if request.env_delta {
                            result.env_delta = Some(Vec::new());
                        }
                        return Err(RunOutcome::Ok(result));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) => {
                    let _ = end_request();
                    return Err(RunOutcome::FatalError(format!(
                        "cannot acquire lock {lock_name}: {error}"
                    )));
                }
            }
        }
    }

    fn flush_stream_chunks(
        &mut self,
        request_id: &str,
        stdout: &Capture,
        stderr: &Capture,
        stdout_offset: &mut u64,
        stderr_offset: &mut u64,
        seq: &mut i64,
    ) {
        for (stream_name, capture_ref, offset) in [
            ("stdout_chunk", stdout, stdout_offset),
            ("stderr_chunk", stderr, stderr_offset),
        ] {
            while let Ok(chunk) = capture_ref.read_from(*offset, READ_CHUNK) {
                if chunk.is_empty() {
                    break;
                }
                *offset += chunk.len() as u64;
                let len = chunk.len();
                let data = match String::from_utf8(chunk) {
                    Ok(text) => StreamData::Utf8(text),
                    Err(err) => StreamData::Base64(err.into_bytes()),
                };
                let frame = StreamChunk {
                    id: request_id.to_string(),
                    stream: stream_name.to_string(),
                    seq: *seq,
                    data: Captured {
                        data,
                        bytes: len,
                        truncated: false,
                    },
                };
                *seq += 1;
                if !self.write(&Response::StreamChunk(frame)) {
                    return;
                }
                if len < READ_CHUNK {
                    break;
                }
            }
        }
    }

    fn run_isolated_exec_internal(
        &mut self,
        request: &protocol::ExecRequest,
    ) -> RunOutcome<ExecResult> {
        let session_cwd = self.shell.cwd.to_string_lossy().into_owned();
        let apply_back = !request.isolated && request.sandbox.is_none();
        let task = protocol::TaskRequest {
            id: request.id.clone(),
            target: request.target.clone(),
            stdin: request.stdin.clone(),
            timeout_ms: request.timeout_ms,
            cwd: request.cwd.clone(),
            env: request.env.clone(),
            env_delta: request.env_delta || apply_back,
            output: request.output.clone(),
            tty: request.tty,
            sandbox: request.sandbox.clone(),
        };
        if !supervisor::can_track_group() {
            let msg = String::from("sharky: isolated exec: session process-group registry is full\n");
            let mut result = ExecResult::new(request.id.clone(), 126, 0, session_cwd);
            result.termination = Termination::SpawnError;
            result.stderr = Captured {
                bytes: msg.len(),
                data: StreamData::Utf8(msg),
                truncated: false,
            };
            if request.env_delta {
                result.env_delta = Some(Vec::new());
            }
            return RunOutcome::Ok(result);
        }

        crate::signal::clear_interrupt();
        let fs_before = if request.fs_delta {
            let target_dir = request
                .cwd
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.shell.cwd.clone());
            Some((target_dir.clone(), snapshot_fs(&target_dir)))
        } else {
            None
        };
        begin_request(&request.id, request.timeout_ms);
        let _lock = match self.acquire_exec_lock(request) {
            Ok(lock) => lock,
            Err(outcome) => return outcome,
        };
        let mut close_fds = vec![self.null_fd];
        if let Some(ref lock) = _lock {
            close_fds.push(lock.fd());
        }
        let mut attempt = match attempt::spawn_attempt(
            &mut self.shell,
            &task,
            self.null_fd,
            &close_fds,
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                let _ = end_request();
                let msg = format!("sharky: isolated exec: {error}\n");
                let mut result = ExecResult::new(request.id.clone(), 126, 0, session_cwd);
                result.termination = Termination::SpawnError;
                result.stderr = Captured {
                    bytes: msg.len(),
                    data: StreamData::Utf8(msg),
                    truncated: false,
                };
                if request.env_delta {
                    result.env_delta = Some(Vec::new());
                }
                return RunOutcome::Ok(result);
            }
        };

        note_foreground(attempt.leader);
        let mut stdout_offset: u64 = 0;
        let mut stderr_offset: u64 = 0;
        let mut seq: i64 = 0;
        let wait_status = loop {
            supervisor::service();
            attempt.drain_pty();
            if request.stream {
                self.flush_stream_chunks(
                    &request.id,
                    &attempt.captures.stdout,
                    &attempt.captures.stderr,
                    &mut stdout_offset,
                    &mut stderr_offset,
                    &mut seq,
                );
            }
            match crate::exec::wait_scan(nix::unistd::Pid::from_raw(attempt.leader)) {
                Some(
                    status @ (nix::sys::wait::WaitStatus::Exited(..)
                    | nix::sys::wait::WaitStatus::Signaled(..)),
                ) => break Some(status),
                Some(
                    nix::sys::wait::WaitStatus::StillAlive
                    | nix::sys::wait::WaitStatus::Stopped(..)
                    | nix::sys::wait::WaitStatus::Continued(..),
                ) => {}
                Some(_) | None => break None,
            }
            if crate::signal::fatal_signal().is_some() {
                break None;
            }
            if let Some(pty_fd) = attempt.pty_fd() {
                supervisor::wait_poll_fd(pty_fd);
            } else {
                supervisor::wait_poll();
            }
        };
        clear_foreground();
        let pgid = request_group().or(Some(i64::from(attempt.leader)));
        let supervised = end_request();
        if let Some(sig) = crate::signal::fatal_signal() {
            return RunOutcome::FatalExit(128 + sig);
        }

        attempt.drain_pty();
        if request.stream {
            self.flush_stream_chunks(
                &request.id,
                &attempt.captures.stdout,
                &attempt.captures.stderr,
                &mut stdout_offset,
                &mut stderr_offset,
                &mut seq,
            );
        }

        let stop = supervised.map(|outcome| supervisor::TaskStop {
            outcome,
            reason: None,
        });
        let mut budget = usize::MAX;
        let outcome = match attempt.finish(
            &task,
            &session_cwd,
            wait_status,
            stop,
            &self.limits,
            &mut budget,
        ) {
            Ok(outcome) => outcome,
            Err(message) => return RunOutcome::FatalError(message),
        };
        if !self.wire_intact() {
            return RunOutcome::FatalExit(1);
        }

        if apply_back && outcome.result.termination == Termination::Exited {
            let _ = self.shell.enter_task_directory(&outcome.result.cwd);
            if let Some(ref delta) = outcome.result.env_delta {
                for (name, value) in delta {
                    self.shell.set_task_env(name, value.clone());
                }
            }
        }

        let parsed = request
            .output
            .as_ref()
            .and_then(|o| o.parse)
            .map(|parser| {
                projection::parse_structured_output(
                    parser,
                    &outcome.raw_stdout.bytes,
                    &outcome.raw_stderr.bytes,
                )
            });
        let fs_delta = fs_before.as_ref().map(|(dir, (before_map, before_trunc))| {
            let (after_map, after_trunc) = snapshot_fs(dir);
            diff_fs_snapshot(before_map, *before_trunc, &after_map, after_trunc)
        });

        let mut result = ExecResult::new(
            request.id.clone(),
            outcome.result.status,
            outcome.result.duration_ms,
            outcome.result.cwd,
        );
        result.termination = outcome.result.termination;
        result.signal = outcome.result.signal;
        result.duration_us = outcome.result.duration_us;
        result.pgid = pgid;
        result.stdout = outcome.result.stdout;
        result.stderr = outcome.result.stderr;
        result.capture_id = self
            .captures
            .record(outcome.raw_stdout, outcome.raw_stderr);
        result.projection = outcome.result.projection;
        result.parsed = parsed;
        result.fs_delta = fs_delta;
        result.env_delta = if request.env_delta {
            outcome
                .result
                .env_delta
                .map(|delta| redact_delta(delta, self.redact_env))
        } else {
            None
        };
        drop(_lock);
        if apply_back {
            self.save_state_file();
        }
        RunOutcome::Ok(result)
    }

    fn state(&mut self, request: &protocol::StateRequest) -> Option<i32> {
        let result = self.build_state_result(request);
        self.deliver(&Response::State(result))
    }

    pub(super) fn build_state_result(&mut self, request: &protocol::StateRequest) -> StateResult {
        let mut sections: Vec<(String, Value)> = Vec::new();
        for section in &request.include {
            match section {
                StateSection::Env => {
                    let entries = redact_pairs(self.shell.snapshot_env(), self.redact_env);
                    sections.push(("env".to_string(), pairs_value(&entries)));
                }
                StateSection::Variables => {
                    sections.push((
                        "variables".to_string(),
                        variables_value(&self.shell, self.redact_env),
                    ));
                }
                StateSection::Functions => {
                    let names = self.shell.snapshot_functions();
                    sections.push((
                        "functions".to_string(),
                        Value::Array(names.into_iter().map(Value::String).collect()),
                    ));
                }
                StateSection::Aliases => {
                    sections.push((
                        "aliases".to_string(),
                        pairs_value(&self.shell.snapshot_aliases()),
                    ));
                }
                StateSection::Traps => {
                    sections.push((
                        "traps".to_string(),
                        pairs_value(&self.shell.snapshot_traps()),
                    ));
                }
                StateSection::Jobs => {}
            }
        }

        let mut options = ShellOptions::default();
        for (name, on) in self.shell.snapshot_options() {
            match name {
                "errexit" => options.errexit = on,
                "nounset" => options.nounset = on,
                "pipefail" => options.pipefail = on,
                "noclobber" => options.noclobber = on,
                "xtrace" => options.xtrace = on,
                _ => {}
            }
        }

        let jobs = self
            .shell
            .snapshot_jobs()
            .into_iter()
            .map(|(id, pgid, pids, command, stopped)| JobSummary {
                id: id as i64,
                pgid: i64::from(pgid),
                pids: pids.into_iter().map(i64::from).collect(),
                command,
                state: if stopped { "stopped" } else { "running" }.to_string(),
            })
            .collect();

        StateResult {
            id: request.id.clone(),
            cwd: self.shell.cwd.to_string_lossy().into_owned(),
            last_status: i64::from(self.shell.last_status),
            pid: i64::from(std::process::id()),
            jobs,
            options,
            sections,
        }
    }

    /// `cancel` from the driver's idle path: the supervisor answers the ack or
    /// the `CANCEL_*` error itself, because the same code path serves cancels
    /// pumped while an `exec` runs (design §8.2).
    fn cancel(&mut self, request: &protocol::CancelRequest) -> Option<i32> {
        if cancel_op(request) {
            None
        } else {
            Some(self.end(None, None))
        }
    }

    fn close(&mut self, request: &protocol::CloseRequest) -> i32 {
        let ack = Ack {
            id: request.id.clone(),
            op: "close".to_string(),
            // The vocabulary the supervisor unit extends; for `close` the ack
            // only says the op was accepted.
            outcome: "closed".to_string(),
        };
        if !self.write(&Response::Ack(ack)) {
            return self.end(None, None);
        }
        // `close.wait_ms` is the grace the group cleanup gets before SIGKILL.
        if let Some(wait_ms) = request.wait_ms {
            self.cleanup_wait_ms = wait_ms;
        }
        self.end(Some(0), Some(Some(request.id.clone())))
    }

    // ---- responses and lifecycle -----------------------------------------

    fn write(&mut self, response: &Response) -> bool {
        let Some(wire) = &self.wire else {
            return true;
        };
        let mut line = response.encode();
        line.push('\n');
        capture::write_safely(wire.output, line.as_bytes()).is_ok()
    }

    fn deliver(&mut self, response: &Response) -> Option<i32> {
        if self.write(response) {
            None
        } else {
            // EPIPE (or any write failure) means nobody is listening.
            Some(self.end(None, None))
        }
    }

    fn report_error(
        &mut self,
        id: Option<&str>,
        code: ErrorCode,
        message: impl Into<String>,
    ) -> bool {
        let response = ErrorResponse::new(id.map(str::to_string), code, message);
        self.write(&Response::Error(response))
    }

    fn internal(&mut self, message: String) -> Option<i32> {
        self.report_error(None, ErrorCode::Internal, message);
        Some(self.end(Some(1), None))
    }

    /// Runs the EXIT action once and records the status the process leaves with.
    pub(super) fn finish(&mut self, requested: Option<i32>) -> i32 {
        if let Some(status) = self.exit_status {
            return status;
        }
        let status = self
            .shell
            .finish(requested.unwrap_or(self.shell.last_status));
        self.exit_status = Some(status);
        status
    }

    pub(super) fn end_headless(&mut self, requested: Option<i32>) -> i32 {
        cleanup(self.cleanup_wait_ms);
        let status = self.finish(requested);
        self.save_state_file();
        status
    }

    /// Ends the session: record the terminal state, stop every process group
    /// the session created (TERM, wait up to `cleanup_wait_ms`, KILL), run the
    /// EXIT action once, write an optional `closed` event, then close the
    /// control descriptors and return the exit status.
    ///
    /// `closed_id` is `None` for "do not write `closed`" (EOF has nobody to
    /// answer), `Some(id)` to write it with that id, where the inner `Option`
    /// is `None` for the null id of a session that ended by itself.
    fn end(&mut self, requested: Option<i32>, closed_id: Option<Option<String>>) -> i32 {
        cleanup(self.cleanup_wait_ms);
        let status = self.finish(requested);
        self.save_state_file();
        if let Some(id) = closed_id {
            let _ = self.write(&Response::Closed(Closed {
                id,
                status: i64::from(status),
            }));
            capture::flush_stdio();
        }
        if let Some(wire) = self.wire.take() {
            close_fd(wire.input);
            close_fd(wire.output);
        }
        close_fd(self.null_fd);
        self.null_fd = -1;
        status
    }

    fn wire_intact(&self) -> bool {
        let Some(wire) = &self.wire else {
            return true;
        };
        matches!(capture::identity(wire.input), Ok(identity) if identity == wire.input_identity)
            && matches!(capture::identity(wire.output), Ok(identity) if identity == wire.output_identity)
    }
}

impl code::CodeHost for Session {
    fn limits(&self) -> Limits {
        self.limits
    }

    fn call_exec(&mut self, request: &protocol::ExecRequest) -> Result<ExecResult, String> {
        match self.run_exec_internal(request) {
            RunOutcome::Ok(result) => Ok(result),
            RunOutcome::FatalExit(_) => Err(String::from("session interrupted")),
            RunOutcome::FatalError(message) => Err(message),
        }
    }

    fn call_parallel(
        &mut self,
        request: &protocol::ParallelRequest,
    ) -> Result<json::ParallelResult, String> {
        let mut result = parallel::run(
            &mut self.shell,
            self.null_fd,
            &self.limits,
            &mut self.captures,
            request,
        )?;
        if self.redact_env {
            for task in &mut result.tasks {
                if let Some(delta) = task.env_delta.take() {
                    task.env_delta = Some(redact_delta(delta, true));
                }
            }
        }
        Ok(result)
    }

    fn call_workflow(
        &mut self,
        request: &protocol::WorkflowRequest,
    ) -> Result<json::WorkflowResult, String> {
        let mut result = workflow::run(
            &mut self.shell,
            self.null_fd,
            &self.limits,
            &mut self.captures,
            request,
        )?;
        if self.redact_env {
            for task in &mut result.tasks {
                if let Some(delta) = task.base.env_delta.take() {
                    task.base.env_delta = Some(redact_delta(delta, true));
                }
            }
        }
        Ok(result)
    }

    fn call_state(&mut self, request: &protocol::StateRequest) -> StateResult {
        self.build_state_result(request)
    }

    fn call_read_capture(
        &self,
        request: &protocol::ReadCaptureRequest,
    ) -> Result<json::CaptureResult, (ErrorCode, String)> {
        self.captures.read(request, &self.limits)
    }
}

fn session_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    format!("s-{}-{nanos:08x}", std::process::id())
}

/// Converts one capture read-back into the wire shape. Shared with the
/// parallel executor, whose task streams obey exactly the same rules.
pub(crate) fn render(captured: capture::Captured) -> Captured {
    let data = match captured.content {
        CaptureContent::Text(text) => StreamData::Utf8(text),
        CaptureContent::Binary(bytes) => StreamData::Base64(bytes),
    };
    Captured {
        data,
        bytes: captured.total as usize,
        truncated: captured.truncated,
    }
}

const REDACTED_VALUE: &str = "<redacted>";

fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "API_KEY",
        "APIKEY",
        "PRIVATE_KEY",
        "AUTH",
        "CREDENTIAL",
        "COOKIE",
        "SESSION_KEY",
    ]
    .iter()
    .any(|needle| upper.contains(needle))
}

fn redact_pairs(entries: Vec<(String, String)>, enabled: bool) -> Vec<(String, String)> {
    if !enabled {
        return entries;
    }
    entries
        .into_iter()
        .map(|(name, value)| {
            if is_sensitive_env_name(&name) {
                (name, REDACTED_VALUE.to_string())
            } else {
                (name, value)
            }
        })
        .collect()
}

pub(crate) fn redact_delta(
    delta: Vec<(String, Option<String>)>,
    enabled: bool,
) -> Vec<(String, Option<String>)> {
    if !enabled {
        return delta;
    }
    delta
        .into_iter()
        .map(|(name, value)| {
            if is_sensitive_env_name(&name) {
                (name, value.map(|_| REDACTED_VALUE.to_string()))
            } else {
                (name, value)
            }
        })
        .collect()
}

fn pairs_value(entries: &[(String, String)]) -> Value {
    Value::Object(
        entries
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect(),
    )
}

/// The `variables` section: scalar bindings, indexed arrays and associative
/// arrays in one object, so an unset array element stays a JSON `null` instead
/// of the string `"null"` (design §3.4/§6.7). The shape is
/// `{"scalars":{...},"indexed":{...},"assoc":{...}}`; the design's two shapes
/// for `variables` and arrays cannot both be the top level of one key, and this
/// one keeps every binding reachable from the section that is gated.
fn variables_value(shell: &crate::shell::Shell, redact: bool) -> Value {
    let scalars = Value::Object(
        shell
            .snapshot_variables()
            .into_iter()
            .map(|(name, value, exported, readonly)| {
                let sensitive = redact && is_sensitive_env_name(&name);
                let value = match value {
                    Some(value) => Value::String(if sensitive {
                        REDACTED_VALUE.to_string()
                    } else {
                        value
                    }),
                    None => Value::Null,
                };
                (
                    name,
                    Value::Object(vec![
                        ("value".to_string(), value),
                        ("exported".to_string(), Value::Bool(exported)),
                        ("readonly".to_string(), Value::Bool(readonly)),
                    ]),
                )
            })
            .collect(),
    );
    let indexed = Value::Object(
        shell
            .snapshot_arrays()
            .into_iter()
            .map(|(name, values)| {
                let sensitive = redact && is_sensitive_env_name(&name);
                let values = values
                    .into_iter()
                    .map(|value| match value {
                        Some(value) => Value::String(if sensitive {
                            REDACTED_VALUE.to_string()
                        } else {
                            value
                        }),
                        None => Value::Null,
                    })
                    .collect();
                (name, Value::Array(values))
            })
            .collect(),
    );
    let assoc = Value::Object(
        shell
            .snapshot_assoc()
            .into_iter()
            .map(|(name, entries)| {
                let name_sensitive = redact && is_sensitive_env_name(&name);
                let entries = entries
                    .into_iter()
                    .map(|(key, value)| {
                        let mask = name_sensitive || (redact && is_sensitive_env_name(&key));
                        let rendered = if mask {
                            REDACTED_VALUE.to_string()
                        } else {
                            value
                        };
                        (key, Value::String(rendered))
                    })
                    .collect();
                (name, Value::Object(entries))
            })
            .collect(),
    );
    Value::Object(vec![
        ("scalars".to_string(), scalars),
        ("indexed".to_string(), indexed),
        ("assoc".to_string(), assoc),
    ])
}

// ------------------------------------------------------------------ framing

/// One frame from the control channel.
pub(crate) enum Frame {
    /// A complete line (newline removed).
    Line(Vec<u8>),
    /// A line longer than the limit was discarded up to its newline.
    TooLarge,
    /// End of input; a non-empty `partial` is a final line without a newline.
    Eof { partial: Option<Vec<u8>> },
}

/// Bounded line reader over the control descriptor.
///
/// A line longer than the limit is reported once and then consumed to its
/// newline without buffering it, so the next line is read cleanly and the
/// session keeps serving (design §3.6).
struct FrameReader {
    buffer: Vec<u8>,
    start: usize,
    max_line_bytes: usize,
    discarding: bool,
}

impl FrameReader {
    fn new(max_line_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            max_line_bytes,
            discarding: false,
        }
    }

    pub(super) fn has_buffered_frame(&self) -> bool {
        self.buffer[self.start..].contains(&b'\n')
            || (!self.discarding && self.buffer.len() - self.start > self.max_line_bytes)
    }

    fn next(&mut self, fd: RawFd) -> io::Result<Frame> {
        loop {
            if let Some(frame) = self.take_buffered() {
                return Ok(frame);
            }
            let mut chunk = [0u8; READ_CHUNK];
            let count = read_some(fd, &mut chunk)?;
            if count == 0 {
                return Ok(self.eof());
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }

    /// Extracts what is already buffered or readable without blocking. `None`
    /// means no complete frame yet (the service pump then yields the thread).
    fn next_nonblocking(&mut self, fd: RawFd) -> io::Result<Option<Frame>> {
        loop {
            if crate::signal::fatal_signal().is_some() {
                return Ok(None);
            }
            if let Some(frame) = self.take_buffered() {
                return Ok(Some(frame));
            }
            if !readable(fd)? {
                return Ok(None);
            }
            let mut chunk = [0u8; READ_CHUNK];
            let count = match read_some(fd, &mut chunk) {
                Ok(count) => count,
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && crate::signal::fatal_signal().is_some() =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            if count == 0 {
                return Ok(Some(self.eof()));
            }
            self.buffer.extend_from_slice(&chunk[..count]);
        }
    }

    /// One complete frame from the buffer, if any: a line, an oversized line,
    /// or nothing yet. The oversized-line reporting is the driver's (design
    /// §3.6), so it happens exactly once per discarded line.
    fn take_buffered(&mut self) -> Option<Frame> {
        if let Some(offset) = self.buffer[self.start..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            let end = self.start + offset;
            if self.discarding {
                self.discarding = false;
                self.start = end + 1;
                self.compact();
                return self.take_buffered();
            }
            let line = self.buffer[self.start..end].to_vec();
            self.start = end + 1;
            self.compact();
            if line.len() > self.max_line_bytes {
                return Some(Frame::TooLarge);
            }
            return Some(Frame::Line(line));
        }

        if self.discarding {
            self.buffer.clear();
            self.start = 0;
        } else if self.buffer.len() - self.start > self.max_line_bytes {
            // No newline within the limit: drop what is buffered and consume
            // the rest of the line as it arrives.
            self.buffer.clear();
            self.start = 0;
            self.discarding = true;
            return Some(Frame::TooLarge);
        }
        None
    }

    /// The end-of-input frame plus the trailing bytes of an unterminated line.
    fn eof(&mut self) -> Frame {
        let partial = if self.start < self.buffer.len() {
            Some(self.buffer[self.start..].to_vec())
        } else {
            None
        };
        let frame = if self.discarding {
            // The oversized line was already reported; nothing of it is a
            // request.
            Frame::Eof { partial: None }
        } else {
            Frame::Eof { partial }
        };
        self.buffer.clear();
        self.start = 0;
        self.discarding = false;
        frame
    }

    fn compact(&mut self) {
        if self.start >= COMPACT_THRESHOLD {
            self.buffer.drain(..self.start);
            self.start = 0;
        }
    }
}

/// Whether `poll(2)` reports something to read (or the EOF/hangup the next
/// `read` turns into an empty frame). A signal interruption reads as "nothing
/// yet"; the caller's loop retries.
fn readable(fd: RawFd) -> io::Result<bool> {
    let mut descriptor = nix::libc::pollfd {
        fd,
        events: nix::libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid descriptor and a zero timeout, so the call only
    // inspects readiness.
    let ready = unsafe { nix::libc::poll(&mut descriptor, 1, 0) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(ready > 0 && descriptor.revents != 0)
}

// ------------------------------------------------------------- raw descriptor I/O

fn read_some(fd: RawFd, buffer: &mut [u8]) -> io::Result<usize> {
    loop {
        if crate::signal::fatal_signal().is_some() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        // SAFETY: `buffer` is valid for writes of its own length.
        let count = unsafe { nix::libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };
        if count >= 0 {
            return Ok(count as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FsMeta {
    len: u64,
    mtime_nanos: u128,
    is_dir: bool,
}

fn snapshot_fs(root: &Path) -> (BTreeMap<String, FsMeta>, bool) {
    const MAX_DEPTH: usize = 4;
    const MAX_ENTRIES: usize = 2000;
    let mut map = BTreeMap::new();
    let mut truncated = false;
    walk_fs_bounded(root, root, 0, MAX_DEPTH, MAX_ENTRIES, &mut map, &mut truncated);
    (map, truncated)
}

fn walk_fs_bounded(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    max_entries: usize,
    out: &mut BTreeMap<String, FsMeta>,
    truncated: &mut bool,
) {
    if depth > max_depth || out.len() >= max_entries {
        *truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= max_entries {
            *truncated = true;
            return;
        }
        let name = entry.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let is_dir = meta.is_dir();
        let mtime_nanos = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        out.insert(
            rel.to_string_lossy().into_owned(),
            FsMeta {
                len: meta.len(),
                mtime_nanos,
                is_dir,
            },
        );
        if is_dir && depth < max_depth {
            walk_fs_bounded(root, &path, depth + 1, max_depth, max_entries, out, truncated);
        }
    }
}

fn diff_fs_snapshot(
    before: &BTreeMap<String, FsMeta>,
    before_trunc: bool,
    after: &BTreeMap<String, FsMeta>,
    after_trunc: bool,
) -> FsDelta {
    let mut created = Vec::new();
    let mut modified = Vec::new();
    let mut removed = Vec::new();

    for (path, meta) in after {
        match before.get(path) {
            None => created.push(path.clone()),
            Some(prev) if !meta.is_dir && prev != meta => modified.push(path.clone()),
            _ => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            removed.push(path.clone());
        }
    }

    FsDelta {
        created,
        modified,
        removed,
        truncated: before_trunc || after_trunc,
    }
}
