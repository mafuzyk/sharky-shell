//! Strict request decoding for the agent session protocol.
//!
//! [`json`](crate::agent::json) parses a line into a [`Value`]; this module turns
//! that value into a validated request, or into the exact protocol error of
//! design §3.5. It owns no I/O and no session state: the driver
//! ([`crate::agent`]) calls [`decode`] once per line and formats the result.
//!
//! Validation order is part of the contract, because only the first failure is
//! reported:
//!
//! 1. the line must be a JSON object (`INVALID_REQUEST`);
//! 2. `schema_version` must be an integer equal to
//!    [`SCHEMA_VERSION`](crate::agent::json::SCHEMA_VERSION)
//!    (`INVALID_REQUEST` when missing or not an integer,
//!    `UNSUPPORTED_SCHEMA_VERSION` when a different integer);
//! 3. `id` must be a non-empty string of at most `max_id_bytes`
//!    (`INVALID_ID`, answered with `"id":null` so a garbage id is not echoed);
//! 4. `op` must be a string naming an op this build honors (`INVALID_REQUEST`,
//!    then `UNKNOWN_OP`);
//! 5. the op's own fields must have the documented types.
//!
//! Unknown object keys are ignored for forward compatibility, as design §3.1
//! requires. `cancel` and `timeout_ms` are honored by this build, so
//! `ready.protocol.ops`/`features` list them and the decoder validates them
//! instead of refusing them: a client that sends a deadline must be able to
//! believe it was enforced (design §8.2).
//!
//! `op:"parallel"` is decoded by [`decode_parallel`] (design §2.1/§2.2) and
//! dispatched to the group executor; `json::PARALLEL_AVAILABLE` gates both the
//! dispatch below and the `ops`/`features` advertisement, so a client can never
//! be told about an op this build does not answer. `CancelRequest` carries the
//! optional `task` field the supervisor resolves against a running group
//! (design §5), and its own availability flag
//! (`json::PARALLEL_TASK_CANCEL_AVAILABLE`) advertises that capability.
//!
//! `op:"workflow"` is decoded by [`decode_workflow`] with the same discipline,
//! plus the two-pass DAG validation of design §2.2/§3 (ids first, then edges,
//! then cycles, then conditions).
//!
//! Duplicate-id detection needs history across lines, so it lives in the
//! supervisor; [`request_id`] gives it the id of a line that is shaped well
//! enough to check before full validation, matching "an id already used on
//! this connection" (`DUPLICATE_ID`).

use crate::agent::json::{
    Base64Error, DEFAULT_CODE_MAX_CALLS, DEFAULT_CODE_MAX_STEPS, ErrorCode, ExitCodeMatcher,
    FailurePolicy, Limits, ParallelPolicy, SCHEMA_VERSION, StateSection, Value, base64_decode,
};
use crate::agent::supervisor::CANCEL_GRACE_MS;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// How many ids the driver remembers for duplicate detection (design §3.6).
/// Beyond the cap the oldest id is forgotten rather than the session refusing
/// new requests; the reduced guarantee is documented in `help agent`.
pub(crate) const TRACKED_ID_LIMIT: usize = 8192;

/// A rejected request: the protocol code, human text, and the id to echo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodeError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    /// `Some` only when the request carried a usable id; protocol errors that
    /// cannot be attributed are answered with `"id":null` (design §3.1).
    pub(crate) id: Option<String>,
}

impl DecodeError {
    fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            id: None,
        }
    }

    fn with_id(code: ErrorCode, message: impl Into<String>, id: Option<&str>) -> Self {
        Self {
            code,
            message: message.into(),
            id: id.map(str::to_string),
        }
    }
}

/// One validated request. The encoder-side types live in
/// [`crate::agent::json`]; these are the decode side and carry only what the
/// driver needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodedRequest {
    Exec(ExecRequest),
    State(StateRequest),
    Cancel(CancelRequest),
    Close(CloseRequest),
    Parallel(ParallelRequest),
    Workflow(WorkflowRequest),
    ReadCapture(ReadCaptureRequest),
    Code(CodeRequest),
}

impl DecodedRequest {
    pub(crate) fn id(&self) -> &str {
        match self {
            DecodedRequest::Exec(request) => &request.id,
            DecodedRequest::State(request) => &request.id,
            DecodedRequest::Cancel(request) => &request.id,
            DecodedRequest::Close(request) => &request.id,
            DecodedRequest::Parallel(request) => &request.id,
            DecodedRequest::Workflow(request) => &request.id,
            DecodedRequest::ReadCapture(request) => &request.id,
            DecodedRequest::Code(request) => &request.id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeRequest {
    pub(crate) id: String,
    pub(crate) source: String,
    pub(crate) args: Value,
    pub(crate) timeout_ms: Option<i64>,
    pub(crate) max_steps: usize,
    pub(crate) max_calls: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExecTarget {
    Command(String),
    Argv(Vec<String>),
}

impl ExecTarget {
    #[allow(dead_code)]
    pub(crate) fn command(&self) -> Option<&str> {
        match self {
            ExecTarget::Command(command) => Some(command),
            ExecTarget::Argv(_) => None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn argv(&self) -> Option<&[String]> {
        match self {
            ExecTarget::Command(_) => None,
            ExecTarget::Argv(argv) => Some(argv),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputParser {
    Cargo,
    GitStatus,
    Pytest,
    Rg,
    Json,
    Lines,
}

impl OutputParser {
    pub(crate) fn parse_name(name: &str) -> Option<Self> {
        match name {
            "cargo" => Some(Self::Cargo),
            "git_status" => Some(Self::GitStatus),
            "pytest" => Some(Self::Pytest),
            "rg" => Some(Self::Rg),
            "json" => Some(Self::Json),
            "lines" => Some(Self::Lines),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputSpec {
    pub(crate) strip_ansi: bool,
    pub(crate) grep: Option<String>,
    pub(crate) context: usize,
    pub(crate) head_lines: Option<usize>,
    pub(crate) tail_lines: Option<usize>,
    pub(crate) max_bytes: Option<usize>,
    pub(crate) parse: Option<OutputParser>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaptureStream {
    Stdout,
    Stderr,
}

impl CaptureStream {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            CaptureStream::Stdout => "stdout",
            CaptureStream::Stderr => "stderr",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadCaptureRequest {
    pub(crate) id: String,
    pub(crate) capture_id: String,
    pub(crate) stream: CaptureStream,
    pub(crate) line_start: Option<usize>,
    pub(crate) line_count: Option<usize>,
    pub(crate) byte_offset: Option<usize>,
    pub(crate) max_bytes: Option<usize>,
    pub(crate) grep: Option<String>,
    pub(crate) context: usize,
    pub(crate) strip_ansi: bool,
}

/// Opt-in per-request OS containment and resource limits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SandboxSpec {
    pub(crate) no_network: bool,
    pub(crate) read_only_root: bool,
    pub(crate) max_file_bytes: Option<u64>,
    pub(crate) max_procs: Option<u64>,
    pub(crate) cpu_seconds: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecRequest {
    pub(crate) id: String,
    pub(crate) target: ExecTarget,
    /// The decoded payload bytes, after `stdin_encoding` and the size limit.
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) env_delta: bool,
    /// `None` means no deadline; otherwise milliseconds from evaluation start.
    pub(crate) timeout_ms: Option<i64>,
    /// True evaluates in a forked copy-on-write child without touching session state.
    pub(crate) isolated: bool,
    pub(crate) cwd: Option<String>,
    pub(crate) env: Vec<(String, Option<String>)>,
    pub(crate) output: Option<OutputSpec>,
    pub(crate) tty: bool,
    pub(crate) stream: bool,
    pub(crate) lock: Option<String>,
    pub(crate) sandbox: Option<SandboxSpec>,
    pub(crate) fs_delta: bool,
}

impl ExecRequest {
    #[allow(dead_code)]
    pub(crate) fn command(&self) -> Option<&str> {
        self.target.command()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StateRequest {
    pub(crate) id: String,
    /// Requested sections, deduplicated, in the order the client asked for them.
    pub(crate) include: Vec<StateSection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CancelRequest {
    pub(crate) id: String,
    /// The id of the request whose process group is signalled.
    pub(crate) target: String,
    /// One `parallel` task to cancel; `None` cancels the whole request (or the
    /// `exec` group). Only meaningful when `target` names a parallel request.
    pub(crate) task: Option<String>,
    /// `None` uses `ready.protocol.cancel_grace_ms`.
    pub(crate) grace_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CloseRequest {
    pub(crate) id: String,
    /// Grace for the shutdown cleanup before `SIGKILL`; `None` uses the default.
    pub(crate) wait_ms: Option<i64>,
}

/// Extracts an id that is already well shaped, without validating anything
/// else. The driver uses it for the duplicate check, so a repeated id is
/// answered with `DUPLICATE_ID` even when the rest of the line is also wrong.
pub(crate) fn request_id(value: &Value, limits: &Limits) -> Option<String> {
    let id = value.get("id")?.as_str()?;
    if id.is_empty() || id.len() > limits.max_id_bytes {
        return None;
    }
    Some(id.to_string())
}

/// Validates one parsed line as a request.
pub(crate) fn decode(value: &Value, limits: &Limits) -> Result<DecodedRequest, DecodeError> {
    if value.as_object().is_none() {
        return Err(DecodeError::new(
            ErrorCode::InvalidRequest,
            "a request must be a JSON object",
        ));
    }

    let id = match value.get("id") {
        Some(Value::String(id)) => Some(id.as_str()),
        _ => None,
    };
    let usable_id = id.filter(|id| !id.is_empty() && id.len() <= limits.max_id_bytes);

    match value.get("schema_version") {
        Some(Value::Int(version)) if *version == SCHEMA_VERSION => {}
        Some(Value::Int(version)) => {
            return Err(DecodeError::with_id(
                ErrorCode::UnsupportedSchemaVersion,
                format!(
                    "schema_version {version} is not supported; this build speaks {SCHEMA_VERSION}"
                ),
                usable_id,
            ));
        }
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "schema_version must be an integer",
                usable_id,
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "schema_version is required",
                usable_id,
            ));
        }
    }

    let id = match value.get("id") {
        Some(Value::String(id)) if id.is_empty() => {
            return Err(DecodeError::new(
                ErrorCode::InvalidId,
                "id must not be empty",
            ));
        }
        Some(Value::String(id)) if id.len() > limits.max_id_bytes => {
            return Err(DecodeError::new(
                ErrorCode::InvalidId,
                format!(
                    "id is {} bytes; the limit is {}",
                    id.len(),
                    limits.max_id_bytes
                ),
            ));
        }
        Some(Value::String(id)) => id.clone(),
        Some(_) => {
            return Err(DecodeError::new(
                ErrorCode::InvalidId,
                "id must be a string",
            ));
        }
        None => {
            return Err(DecodeError::new(ErrorCode::InvalidId, "id is required"));
        }
    };

    let op = match value.get("op") {
        Some(Value::String(op)) => op.as_str(),
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "op must be a string",
                Some(&id),
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "op is required",
                Some(&id),
            ));
        }
    };

    match op {
        "exec" => decode_exec(value, limits, id).map(DecodedRequest::Exec),
        "state" => decode_state(value, id).map(DecodedRequest::State),
        "cancel" => decode_cancel(value, limits, id).map(DecodedRequest::Cancel),
        "close" => decode_close(value, id).map(DecodedRequest::Close),
        // The executor exists, so the op is answered. The gate stays: with the
        // flag off, `ready` would not advertise it and this line would fall to
        // `UNKNOWN_OP` instead of accepting a request the driver cannot run.
        "parallel" if crate::agent::json::PARALLEL_AVAILABLE => {
            decode_parallel(value, limits, id).map(DecodedRequest::Parallel)
        }
        // The decoder and the DAG validation exist; the scheduler lands in U2,
        // so the op is unreachable from the wire while the flag is off and the
        // `UNKNOWN_OP` arm below stays the honest answer.
        "workflow" if crate::agent::json::WORKFLOW_AVAILABLE => {
            decode_workflow(value, limits, id).map(DecodedRequest::Workflow)
        }
        "read_capture" if crate::agent::json::CAPTURE_STORE_AVAILABLE => {
            decode_read_capture(value, limits, id).map(DecodedRequest::ReadCapture)
        }
        "code" if crate::agent::json::CODE_MODE_AVAILABLE => {
            decode_code(value, limits, id).map(DecodedRequest::Code)
        }
        other => Err(DecodeError::with_id(
            ErrorCode::UnknownOp,
            format!("unknown op: {other}"),
            Some(&id),
        )),
    }
}

fn decode_target(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<ExecTarget, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    let has_command = value.get("command").is_some();
    let has_argv = value.get("argv").is_some();
    if has_command && has_argv {
        return Err(reject(format!(
            "{prefix}command and {prefix}argv are mutually exclusive"
        )));
    }
    match (value.get("command"), value.get("argv")) {
        (Some(Value::String(command)), None) => Ok(ExecTarget::Command(command.clone())),
        (Some(_), None) => Err(reject(format!("{prefix}command must be a string"))),
        (None, Some(Value::Array(items))) => {
            if items.is_empty() {
                return Err(reject(format!("{prefix}argv must not be empty")));
            }
            let mut argv = Vec::with_capacity(items.len());
            for (pos, item) in items.iter().enumerate() {
                let Some(text) = item.as_str() else {
                    return Err(reject(format!("{prefix}argv[{pos}] must be a string")));
                };
                if pos == 0 && text.is_empty() {
                    return Err(reject(format!("{prefix}argv[0] must not be empty")));
                }
                if text.contains('\0') {
                    return Err(reject(format!(
                        "{prefix}argv[{pos}] must not contain NUL bytes"
                    )));
                }
                argv.push(text.to_string());
            }
            Ok(ExecTarget::Argv(argv))
        }
        (None, Some(_)) => Err(reject(format!("{prefix}argv must be an array of strings"))),
        (None, None) => {
            let owner = if prefix.is_empty() {
                "exec"
            } else {
                prefix.trim_end_matches('.')
            };
            Err(reject(format!("{owner} requires command or argv")))
        }
        (Some(_), Some(_)) => unreachable!(),
    }
}

fn decode_optional_cwd(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<Option<String>, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    match value.get("cwd") {
        None => Ok(None),
        Some(Value::String(cwd)) if cwd.is_empty() => {
            Err(reject(format!("{prefix}cwd must not be empty")))
        }
        Some(Value::String(cwd)) => Ok(Some(cwd.clone())),
        Some(_) => Err(reject(format!("{prefix}cwd must be a string"))),
    }
}

fn decode_optional_env(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<Vec<(String, Option<String>)>, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    let mut env: Vec<(String, Option<String>)> = Vec::new();
    match value.get("env") {
        None => Ok(env),
        Some(Value::Object(entries)) => {
            for (name, entry) in entries {
                if name.is_empty() || name.contains('=') {
                    return Err(reject(format!(
                        "{prefix}env names must be non-empty and must not contain '=' ({name:?})"
                    )));
                }
                if name.contains('\0') {
                    return Err(reject(format!(
                        "{prefix}env names must not contain NUL bytes ({name:?})"
                    )));
                }
                let val = match entry {
                    Value::String(val) => {
                        if val.contains('\0') {
                            return Err(reject(format!(
                                "{prefix}env.{name} must not contain NUL bytes"
                            )));
                        }
                        Some(val.clone())
                    }
                    Value::Null => None,
                    _ => {
                        return Err(reject(format!(
                            "{prefix}env.{name} must be a string or null"
                        )));
                    }
                };
                env.push((name.clone(), val));
            }
            Ok(env)
        }
        Some(_) => Err(reject(format!("{prefix}env must be an object"))),
    }
}

pub(crate) fn decode_exec(
    value: &Value,
    limits: &Limits,
    id: String,
) -> Result<ExecRequest, DecodeError> {
    let target = decode_target(value, &id, "")?;

    let stdin = decode_stdin(value, limits, &id)?;

    let env_delta = match value.get("env_delta") {
        None => true,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "env_delta must be a boolean",
                Some(&id),
            ));
        }
    };

    // A deadline the session honors, so a client can rely on it (design §8.2).
    let timeout_ms = match value.get("timeout_ms") {
        None => None,
        Some(Value::Int(ms)) if *ms >= 0 => Some(*ms),
        Some(Value::Int(_)) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "timeout_ms must not be negative",
                Some(&id),
            ));
        }
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "timeout_ms must be an integer",
                Some(&id),
            ));
        }
    };

    let isolated = match value.get("isolated") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "isolated must be a boolean",
                Some(&id),
            ));
        }
    };

    let cwd = decode_optional_cwd(value, &id, "")?;
    let env = decode_optional_env(value, &id, "")?;
    if !isolated && (value.get("cwd").is_some() || value.get("env").is_some()) {
        return Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "cwd and env on exec require isolated: true",
            Some(&id),
        ));
    }
    let output = decode_output_spec(value, &id, "")?;
    let tty = decode_bool_flag(value, "tty", false, &id, "")?;
    let stream = decode_bool_flag(value, "stream", false, &id, "")?;
    let lock = decode_lock_name(value, &id, "")?;
    let sandbox = decode_sandbox(value, &id, "")?;
    let fs_delta = decode_bool_flag(value, "fs_delta", false, &id, "")?;

    Ok(ExecRequest {
        id,
        target,
        stdin,
        env_delta,
        timeout_ms,
        isolated,
        cwd,
        env,
        output,
        tty,
        stream,
        lock,
        sandbox,
        fs_delta,
    })
}

fn decode_bool_flag(
    value: &Value,
    key: &str,
    default: bool,
    request_id: &str,
    prefix: &str,
) -> Result<bool, DecodeError> {
    match value.get(key) {
        None => Ok(default),
        Some(Value::Bool(flag)) => Ok(*flag),
        Some(_) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            format!("{prefix}{key} must be a boolean"),
            Some(request_id),
        )),
    }
}

fn decode_sandbox(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<Option<SandboxSpec>, DecodeError> {
    let Some(raw) = value.get("sandbox") else {
        return Ok(None);
    };
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    match raw {
        Value::Null | Value::Bool(false) => Ok(None),
        Value::Bool(true) => Ok(Some(SandboxSpec {
            no_network: false,
            read_only_root: false,
            max_file_bytes: Some(64 * 1024 * 1024),
            max_procs: None,
            cpu_seconds: Some(60),
        })),
        Value::Object(entries) => {
            let mut spec = SandboxSpec::default();
            for (key, val) in entries {
                match key.as_str() {
                    "no_network" => {
                        let Some(flag) = val.as_bool() else {
                            return Err(reject(format!(
                                "{prefix}sandbox.no_network must be a boolean"
                            )));
                        };
                        spec.no_network = flag;
                    }
                    "read_only_root" => {
                        let Some(flag) = val.as_bool() else {
                            return Err(reject(format!(
                                "{prefix}sandbox.read_only_root must be a boolean"
                            )));
                        };
                        spec.read_only_root = flag;
                    }
                    "max_file_bytes" => {
                        spec.max_file_bytes = Some(decode_pos_u64(
                            val,
                            &format!("{prefix}sandbox.max_file_bytes"),
                            request_id,
                        )?);
                    }
                    "max_procs" => {
                        spec.max_procs = Some(decode_pos_u64(
                            val,
                            &format!("{prefix}sandbox.max_procs"),
                            request_id,
                        )?);
                    }
                    "cpu_seconds" => {
                        spec.cpu_seconds = Some(decode_pos_u64(
                            val,
                            &format!("{prefix}sandbox.cpu_seconds"),
                            request_id,
                        )?);
                    }
                    other => {
                        return Err(reject(format!(
                            "unknown {prefix}sandbox option: {other}"
                        )));
                    }
                }
            }
            Ok(Some(spec))
        }
        _ => Err(reject(format!(
            "{prefix}sandbox must be a boolean or object"
        ))),
    }
}

fn decode_pos_u64(value: &Value, field: &str, request_id: &str) -> Result<u64, DecodeError> {
    match value {
        Value::Int(n) if *n > 0 => Ok(*n as u64),
        _ => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            format!("{field} must be a positive integer"),
            Some(request_id),
        )),
    }
}

fn decode_output_spec(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<Option<OutputSpec>, DecodeError> {
    let Some(raw) = value.get("output") else {
        return Ok(None);
    };
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    if raw.as_object().is_none() {
        return Err(reject(format!("{prefix}output must be an object")));
    }

    let strip_ansi = match raw.get("strip_ansi") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(reject(format!("{prefix}output.strip_ansi must be a boolean"))),
    };

    let grep = match raw.get("grep") {
        None => None,
        Some(Value::String(pattern)) if pattern.is_empty() => {
            return Err(reject(format!("{prefix}output.grep must not be empty")));
        }
        Some(Value::String(pattern)) => {
            if let Err(err) = regex_lite::Regex::new(pattern) {
                return Err(reject(format!("{prefix}output.grep is invalid regex: {err}")));
            }
            Some(pattern.clone())
        }
        Some(_) => return Err(reject(format!("{prefix}output.grep must be a string"))),
    };

    let context = match raw.get("context") {
        None => 0,
        Some(Value::Int(ctx)) if (0..=100).contains(ctx) => *ctx as usize,
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "{prefix}output.context must be between 0 and 100"
            )));
        }
        Some(_) => return Err(reject(format!("{prefix}output.context must be an integer"))),
    };
    if context > 0 && grep.is_none() {
        return Err(reject(format!(
            "{prefix}output.context requires {prefix}output.grep"
        )));
    }

    let head_lines = match raw.get("head_lines") {
        None => None,
        Some(Value::Int(n)) if (0..=100_000).contains(n) => Some(*n as usize),
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "{prefix}output.head_lines must be between 0 and 100000"
            )));
        }
        Some(_) => {
            return Err(reject(format!(
                "{prefix}output.head_lines must be an integer"
            )));
        }
    };

    let tail_lines = match raw.get("tail_lines") {
        None => None,
        Some(Value::Int(n)) if (0..=100_000).contains(n) => Some(*n as usize),
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "{prefix}output.tail_lines must be between 0 and 100000"
            )));
        }
        Some(_) => {
            return Err(reject(format!(
                "{prefix}output.tail_lines must be an integer"
            )));
        }
    };

    let max_bytes = match raw.get("max_bytes") {
        None => None,
        Some(Value::Int(n)) if *n >= 1 => Some(*n as usize),
        Some(Value::Int(_)) => {
            return Err(reject(format!("{prefix}output.max_bytes must be >= 1")));
        }
        Some(_) => {
            return Err(reject(format!("{prefix}output.max_bytes must be an integer")));
        }
    };

    let parse = match raw.get("parse") {
        None => None,
        Some(Value::String(name)) => match OutputParser::parse_name(name) {
            Some(parser) => Some(parser),
            None => {
                return Err(reject(format!(
                    "{prefix}output.parse unknown parser '{name}'; expected cargo, git_status, pytest, rg, json, or lines"
                )));
            }
        },
        Some(_) => {
            return Err(reject(format!("{prefix}output.parse must be a string")));
        }
    };

    Ok(Some(OutputSpec {
        strip_ansi,
        grep,
        context,
        head_lines,
        tail_lines,
        max_bytes,
        parse,
    }))
}

pub(crate) fn decode_read_capture(
    value: &Value,
    limits: &Limits,
    id: String,
) -> Result<ReadCaptureRequest, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(&id))
    };

    let capture_id = match value.get("capture_id") {
        Some(Value::String(cid)) if cid.is_empty() => {
            return Err(reject(String::from("capture_id must not be empty")));
        }
        Some(Value::String(cid)) if cid.len() > limits.max_id_bytes => {
            return Err(reject(format!(
                "capture_id exceeds max_id_bytes ({})",
                limits.max_id_bytes
            )));
        }
        Some(Value::String(cid)) => cid.clone(),
        Some(_) => return Err(reject(String::from("capture_id must be a string"))),
        None => return Err(reject(String::from("read_capture requires capture_id"))),
    };

    let stream = match value.get("stream") {
        None => CaptureStream::Stdout,
        Some(Value::String(s)) if s == "stdout" => CaptureStream::Stdout,
        Some(Value::String(s)) if s == "stderr" => CaptureStream::Stderr,
        Some(Value::String(other)) => {
            return Err(reject(format!(
                "unknown stream {other:?}; expected stdout or stderr"
            )));
        }
        Some(_) => return Err(reject(String::from("stream must be a string"))),
    };

    let line_start = match value.get("line_start") {
        None => None,
        Some(Value::Int(n)) if *n >= 1 => Some(*n as usize),
        Some(Value::Int(_)) => return Err(reject(String::from("line_start must be >= 1"))),
        Some(_) => return Err(reject(String::from("line_start must be an integer"))),
    };

    let line_count = match value.get("line_count") {
        None => None,
        Some(Value::Int(n)) if (1..=10_000).contains(n) => Some(*n as usize),
        Some(Value::Int(_)) => {
            return Err(reject(String::from("line_count must be between 1 and 10000")));
        }
        Some(_) => return Err(reject(String::from("line_count must be an integer"))),
    };

    let byte_offset = match value.get("byte_offset") {
        None => None,
        Some(Value::Int(n)) if *n >= 0 => Some(*n as usize),
        Some(Value::Int(_)) => {
            return Err(reject(String::from("byte_offset must not be negative")));
        }
        Some(_) => return Err(reject(String::from("byte_offset must be an integer"))),
    };

    let max_bytes = match value.get("max_bytes") {
        None => None,
        Some(Value::Int(n)) if *n >= 1 && (*n as usize) <= limits.max_capture_bytes => {
            Some(*n as usize)
        }
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "max_bytes must be between 1 and {}",
                limits.max_capture_bytes
            )));
        }
        Some(_) => return Err(reject(String::from("max_bytes must be an integer"))),
    };

    let grep = match value.get("grep") {
        None => None,
        Some(Value::String(pattern)) if pattern.is_empty() => {
            return Err(reject(String::from("grep must not be empty")));
        }
        Some(Value::String(pattern)) => {
            if let Err(err) = regex_lite::Regex::new(pattern) {
                return Err(reject(format!("grep is invalid regex: {err}")));
            }
            Some(pattern.clone())
        }
        Some(_) => return Err(reject(String::from("grep must be a string"))),
    };

    let context = match value.get("context") {
        None => 0,
        Some(Value::Int(n)) if (0..=100).contains(n) => *n as usize,
        Some(Value::Int(_)) => {
            return Err(reject(String::from("context must be between 0 and 100")));
        }
        Some(_) => return Err(reject(String::from("context must be an integer"))),
    };
    if context > 0 && grep.is_none() {
        return Err(reject(String::from("context requires grep")));
    }

    let strip_ansi = match value.get("strip_ansi") {
        None => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => return Err(reject(String::from("strip_ansi must be a boolean"))),
    };

    if byte_offset.is_some()
        && (line_start.is_some()
            || line_count.is_some()
            || grep.is_some()
            || context > 0
            || strip_ansi)
    {
        return Err(reject(String::from(
            "byte_offset cannot be combined with line filters or strip_ansi",
        )));
    }

    Ok(ReadCaptureRequest {
        id,
        capture_id,
        stream,
        line_start,
        line_count,
        byte_offset,
        max_bytes,
        grep,
        context,
        strip_ansi,
    })
}

pub(crate) fn decode_code(
    value: &Value,
    limits: &Limits,
    id: String,
) -> Result<CodeRequest, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(&id))
    };

    let source = match value.get("code") {
        Some(Value::String(code)) if code.trim().is_empty() => {
            return Err(reject(String::from("code must not be empty")));
        }
        Some(Value::String(code)) if code.len() > limits.max_line_bytes => {
            return Err(reject(format!(
                "code exceeds max_line_bytes ({})",
                limits.max_line_bytes
            )));
        }
        Some(Value::String(code)) => code.clone(),
        Some(_) => return Err(reject(String::from("code must be a string"))),
        None => return Err(reject(String::from("code requires code"))),
    };

    let args = value.get("args").cloned().unwrap_or(Value::Null);

    let timeout_ms = decode_group_timeout(value, &id)?;

    let default_steps = DEFAULT_CODE_MAX_STEPS.min(limits.max_code_steps).max(1);
    let max_steps = match value.get("max_steps") {
        None => default_steps,
        Some(Value::Int(n)) if *n >= 1 && (*n as usize) <= limits.max_code_steps => *n as usize,
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "max_steps must be between 1 and {}",
                limits.max_code_steps
            )));
        }
        Some(_) => return Err(reject(String::from("max_steps must be an integer"))),
    };

    let default_calls = DEFAULT_CODE_MAX_CALLS.min(limits.max_code_calls).max(1);
    let max_calls = match value.get("max_calls") {
        None => default_calls,
        Some(Value::Int(n)) if *n >= 1 && (*n as usize) <= limits.max_code_calls => *n as usize,
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "max_calls must be between 1 and {}",
                limits.max_code_calls
            )));
        }
        Some(_) => return Err(reject(String::from("max_calls must be an integer"))),
    };

    Ok(CodeRequest {
        id,
        source,
        args,
        timeout_ms,
        max_steps,
        max_calls,
    })
}

fn decode_stdin(value: &Value, limits: &Limits, id: &str) -> Result<Option<Vec<u8>>, DecodeError> {
    let encoding = match value.get("stdin_encoding") {
        None => "utf8",
        Some(Value::String(encoding)) => encoding.as_str(),
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidEncoding,
                "stdin_encoding must be a string",
                Some(id),
            ));
        }
    };
    if !matches!(encoding, "utf8" | "base64") {
        return Err(DecodeError::with_id(
            ErrorCode::InvalidEncoding,
            format!("unknown stdin_encoding: {encoding}"),
            Some(id),
        ));
    }

    let payload = match value.get("stdin") {
        None => {
            if value.get("stdin_encoding").is_some() {
                return Err(DecodeError::with_id(
                    ErrorCode::InvalidRequest,
                    "stdin_encoding requires stdin",
                    Some(id),
                ));
            }
            return Ok(None);
        }
        Some(Value::String(payload)) => payload,
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "stdin must be a string",
                Some(id),
            ));
        }
    };

    let bytes = if encoding == "base64" {
        base64_decode(payload).map_err(|error: Base64Error| {
            DecodeError::with_id(
                ErrorCode::InvalidEncoding,
                format!("stdin is not valid base64: {error}"),
                Some(id),
            )
        })?
    } else {
        payload.as_bytes().to_vec()
    };

    if bytes.len() > limits.max_stdin_bytes {
        return Err(DecodeError::with_id(
            ErrorCode::StdinTooLarge,
            format!(
                "stdin payload is {} bytes; the limit is {}",
                bytes.len(),
                limits.max_stdin_bytes
            ),
            Some(id),
        ));
    }

    Ok(Some(bytes))
}

pub(crate) fn decode_state(value: &Value, id: String) -> Result<StateRequest, DecodeError> {
    let mut include = Vec::new();
    match value.get("include") {
        None => {}
        Some(Value::Array(entries)) => {
            for entry in entries {
                let Some(name) = entry.as_str() else {
                    return Err(DecodeError::with_id(
                        ErrorCode::InvalidRequest,
                        "include entries must be strings",
                        Some(&id),
                    ));
                };
                let section = match name {
                    "env" => StateSection::Env,
                    "variables" => StateSection::Variables,
                    "functions" => StateSection::Functions,
                    "aliases" => StateSection::Aliases,
                    "traps" => StateSection::Traps,
                    "jobs" => StateSection::Jobs,
                    other => {
                        return Err(DecodeError::with_id(
                            ErrorCode::InvalidRequest,
                            format!("unknown state section: {other}"),
                            Some(&id),
                        ));
                    }
                };
                if !include.contains(&section) {
                    include.push(section);
                }
            }
        }
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "include must be an array",
                Some(&id),
            ));
        }
    }
    Ok(StateRequest { id, include })
}

fn decode_cancel(value: &Value, limits: &Limits, id: String) -> Result<CancelRequest, DecodeError> {
    let target = match value.get("target") {
        Some(Value::String(target)) if target.is_empty() => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "target must not be empty",
                Some(&id),
            ));
        }
        Some(Value::String(target)) if target.len() > limits.max_id_bytes => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                format!(
                    "target is {} bytes; the limit is {}",
                    target.len(),
                    limits.max_id_bytes
                ),
                Some(&id),
            ));
        }
        Some(Value::String(target)) => target.clone(),
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "target must be a string",
                Some(&id),
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "cancel requires target",
                Some(&id),
            ));
        }
    };

    let grace_ms = match value.get("grace_ms") {
        None => None,
        Some(Value::Int(grace)) if (0..=CANCEL_GRACE_MS).contains(grace) => Some(*grace),
        Some(Value::Int(_)) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                format!("grace_ms must be between 0 and {CANCEL_GRACE_MS}"),
                Some(&id),
            ));
        }
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "grace_ms must be an integer",
                Some(&id),
            ));
        }
    };

    // Only the shape is validated here; whether `target` is a parallel request
    // is state the decoder does not have. The supervisor answers
    // `INVALID_REQUEST` for `task` on an `exec` target (design §5).
    let task = match value.get("task") {
        None => None,
        Some(Value::String(task)) if task.is_empty() => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "task must not be empty",
                Some(&id),
            ));
        }
        Some(Value::String(task)) if task.len() > limits.max_id_bytes => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                format!(
                    "task is {} bytes; the limit is {}",
                    task.len(),
                    limits.max_id_bytes
                ),
                Some(&id),
            ));
        }
        Some(Value::String(task)) => Some(task.clone()),
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "task must be a string",
                Some(&id),
            ));
        }
    };

    Ok(CancelRequest {
        id,
        target,
        task,
        grace_ms,
    })
}

fn decode_close(value: &Value, id: String) -> Result<CloseRequest, DecodeError> {
    let wait_ms = match value.get("wait_ms") {
        None => None,
        Some(Value::Int(wait)) if *wait >= 0 => Some(*wait),
        Some(Value::Int(_)) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "wait_ms must not be negative",
                Some(&id),
            ));
        }
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "wait_ms must be an integer",
                Some(&id),
            ));
        }
    };
    Ok(CloseRequest { id, wait_ms })
}

// --------------------------------------------------------------- parallel (D1)

/// Default `parallel.max_concurrency` (design §4): low enough to keep fd
/// pressure and CPU contention down, high enough to prove real overlap. A
/// client that knows better asks for more.
pub(crate) const DEFAULT_MAX_CONCURRENCY: usize = 4;

/// One validated `op:"parallel"` request (design §2.1). Strategy, spawning and
/// capture chopping belong to the executor; this type only states what the
/// client asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParallelRequest {
    pub(crate) id: String,
    pub(crate) policy: ParallelPolicy,
    /// Already validated against `limits.max_parallel`; the driver's spawn
    /// window is `min(max_concurrency, tasks.len())`.
    pub(crate) max_concurrency: usize,
    /// Group wall clock, counted from the first spawn; `None` means unbounded.
    pub(crate) timeout_ms: Option<i64>,
    /// Request order, unique ids, never empty, at most
    /// `limits.max_parallel_tasks` entries.
    pub(crate) tasks: Vec<TaskRequest>,
}

/// One validated task of a `parallel` request (design §2.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskRequest {
    pub(crate) id: String,
    /// Shell source or typed argument vector. An empty or whitespace-only
    /// command is a valid no-op with status 0, exactly as in `exec` (design §2.1).
    pub(crate) target: ExecTarget,
    /// The decoded payload bytes, after `stdin_encoding` and the size limit.
    pub(crate) stdin: Option<Vec<u8>>,
    /// Per-task deadline, counted from that task's spawn.
    pub(crate) timeout_ms: Option<i64>,
    /// `None` keeps the snapshot's cwd.
    pub(crate) cwd: Option<String>,
    /// `Some(value)` sets, `None` unsets; applied in the child on top of the
    /// session snapshot, so the parent shell is never touched.
    pub(crate) env: Vec<(String, Option<String>)>,
    /// `false` by default here, unlike `exec` (design §2.1): a task delta is
    /// informational, since a task can never modify the session.
    pub(crate) env_delta: bool,
    pub(crate) output: Option<OutputSpec>,
    pub(crate) tty: bool,
    pub(crate) sandbox: Option<SandboxSpec>,
}

impl TaskRequest {
    #[allow(dead_code)]
    pub(crate) fn command(&self) -> Option<&str> {
        self.target.command()
    }
}

/// `policy` of a group op, shared by `parallel` and `workflow` (design §2.2):
/// the same two values, and the field decides whether a failure stops the
/// group, so an unknown value is refused instead of clamped.
fn decode_group_policy(value: &Value, request_id: &str) -> Result<ParallelPolicy, DecodeError> {
    match value.get("policy") {
        None => Ok(ParallelPolicy::WaitAll),
        Some(Value::String(name)) => match name.as_str() {
            "wait-all" => Ok(ParallelPolicy::WaitAll),
            "fail-fast" => Ok(ParallelPolicy::FailFast),
            // Not a quiet coercion: the policy decides whether a failure stops
            // the group, so guessing would hide a client bug (design §2.2).
            other => Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                format!("unknown policy: {other}; expected wait-all or fail-fast"),
                Some(request_id),
            )),
        },
        Some(_) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "policy must be a string",
            Some(request_id),
        )),
    }
}

/// Largest accepted spawn window of a group op, shared by `parallel` and
/// `workflow`. Clamping would hide a client bug, so an out-of-range window is
/// an error whose message names the accepted range (design §2.2).
fn decode_max_concurrency(
    value: &Value,
    limits: &Limits,
    request_id: &str,
) -> Result<usize, DecodeError> {
    match value.get("max_concurrency") {
        None => Ok(DEFAULT_MAX_CONCURRENCY.min(limits.max_parallel)),
        Some(Value::Int(window)) => {
            let window = usize::try_from(*window)
                .ok()
                .filter(|window| (1..=limits.max_parallel).contains(window));
            match window {
                Some(window) => Ok(window),
                None => Err(DecodeError::with_id(
                    ErrorCode::InvalidRequest,
                    format!(
                        "max_concurrency must be between 1 and {}",
                        limits.max_parallel
                    ),
                    Some(request_id),
                )),
            }
        }
        Some(_) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "max_concurrency must be an integer",
            Some(request_id),
        )),
    }
}

/// Group wall clock of `parallel` and `workflow` (design §2.1): absent means
/// unbounded, and a negative deadline is refused rather than clamped.
fn decode_group_timeout(value: &Value, request_id: &str) -> Result<Option<i64>, DecodeError> {
    match value.get("timeout_ms") {
        None => Ok(None),
        Some(Value::Int(ms)) if *ms >= 0 => Ok(Some(*ms)),
        Some(Value::Int(_)) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "timeout_ms must not be negative",
            Some(request_id),
        )),
        Some(_) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "timeout_ms must be an integer",
            Some(request_id),
        )),
    }
}

/// Validates an `op:"parallel"` request.
///
/// Order, so only the first failure is reported and it is the most structural
/// one: `tasks` (present, array, non-empty, within `max_parallel_tasks`),
/// `policy`, `max_concurrency`, group `timeout_ms`, then each task in request
/// order (`id`, `command`, `stdin`, `timeout_ms`, `cwd`, `env`, `env_delta`).
/// Unknown keys are ignored for forward compatibility, like every other op.
///
/// `id` is passed in already validated by [`decode`], matching
/// [`decode_exec`]; that is also what every error here echoes.
///
/// Validation refuses the whole request: nothing runs, so a malformed request
/// cannot leave a partially spawned group behind (design §2.1).
pub(crate) fn decode_parallel(
    value: &Value,
    limits: &Limits,
    id: String,
) -> Result<ParallelRequest, DecodeError> {
    let entries = match value.get("tasks") {
        Some(Value::Array(entries)) if entries.is_empty() => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "parallel requires at least one task",
                Some(&id),
            ));
        }
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "tasks must be an array",
                Some(&id),
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "parallel requires tasks",
                Some(&id),
            ));
        }
    };
    if entries.len() > limits.max_parallel_tasks {
        return Err(DecodeError::with_id(
            ErrorCode::TooManyTasks,
            format!(
                "the request has {} tasks; the limit is {}",
                entries.len(),
                limits.max_parallel_tasks
            ),
            Some(&id),
        ));
    }

    let policy = decode_group_policy(value, &id)?;

    let max_concurrency = decode_max_concurrency(value, limits, &id)?;

    let timeout_ms = decode_group_timeout(value, &id)?;

    let mut tasks: Vec<TaskRequest> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let task = decode_task(entry, limits, index, &id)?;
        if let Some(earlier) = tasks.iter().position(|seen| seen.id == task.id) {
            return Err(DecodeError::with_id(
                ErrorCode::DuplicateTaskId,
                format!(
                    "tasks[{index}].id \"{}\" is already used by tasks[{earlier}]",
                    task.id
                ),
                Some(&id),
            ));
        }
        tasks.push(task);
    }

    Ok(ParallelRequest {
        id,
        policy,
        max_concurrency,
        timeout_ms,
        tasks,
    })
}

fn decode_task(
    value: &Value,
    limits: &Limits,
    index: usize,
    request_id: &str,
) -> Result<TaskRequest, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    if value.as_object().is_none() {
        return Err(reject(format!("tasks[{index}] must be an object")));
    }

    let id = match value.get("id") {
        Some(Value::String(id)) if id.is_empty() => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidId,
                format!("tasks[{index}].id must not be empty"),
                Some(request_id),
            ));
        }
        Some(Value::String(id)) if id.len() > limits.max_id_bytes => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidId,
                format!(
                    "tasks[{index}].id is {} bytes; the limit is {}",
                    id.len(),
                    limits.max_id_bytes
                ),
                Some(request_id),
            ));
        }
        Some(Value::String(id)) => id.clone(),
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidId,
                format!("tasks[{index}].id must be a string"),
                Some(request_id),
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidId,
                format!("tasks[{index}] requires id"),
                Some(request_id),
            ));
        }
    };

    let prefix = format!("tasks[{index}].");
    let target = decode_target(value, request_id, &prefix)?;

    let stdin = decode_stdin(value, limits, request_id)?;

    let timeout_ms = match value.get("timeout_ms") {
        None => None,
        Some(Value::Int(ms)) if *ms >= 0 => Some(*ms),
        Some(Value::Int(_)) => {
            return Err(reject(format!(
                "tasks[{index}].timeout_ms must not be negative"
            )));
        }
        Some(_) => {
            return Err(reject(format!(
                "tasks[{index}].timeout_ms must be an integer"
            )));
        }
    };

    let cwd = decode_optional_cwd(value, request_id, &prefix)?;
    let env = decode_optional_env(value, request_id, &prefix)?;

    let env_delta = decode_bool_flag(value, "env_delta", false, request_id, &prefix)?;
    let output = decode_output_spec(value, request_id, &prefix)?;
    let tty = decode_bool_flag(value, "tty", false, request_id, &prefix)?;
    let sandbox = decode_sandbox(value, request_id, &prefix)?;

    Ok(TaskRequest {
        id,
        target,
        stdin,
        timeout_ms,
        cwd,
        env,
        env_delta,
        output,
        tty,
        sandbox,
    })
}

// --------------------------------------------------------------- workflow (D2)

/// Which outcomes may be retried (design §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryOn {
    /// A completed non-zero exit, or a signal the session did not send.
    Failure,
    /// The attempt's own deadline.
    Timeout,
    /// The attempt never got a fair chance to run.
    SpawnError,
}

/// Shape of the retry delay (design §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backoff {
    Fixed,
    Exponential,
}

/// One node's retry contract (design §5.1). An absent `retry` means exactly one
/// attempt; every present field keeps the default of the design's table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// `1..=limits.max_workflow_attempts`; `1` means no retry.
    pub(crate) max_attempts: usize,
    /// Never empty; defaults to `[Timeout, SpawnError]`.
    pub(crate) on: Vec<RetryOn>,
    /// Base delay in milliseconds; `0..=limits.max_workflow_backoff_ms`.
    pub(crate) backoff_ms: i64,
    pub(crate) backoff: Backoff,
}

/// One node's gate over the result of one of its dependencies (design §6).
/// Every present predicate must hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Condition {
    /// The dependency id as the client wrote it, for the response echo.
    pub(crate) task: String,
    /// The same dependency resolved to its request index.
    pub(crate) task_index: usize,
    pub(crate) exit_code: Option<ExitCodeMatcher>,
    pub(crate) stdout_contains: Option<String>,
    pub(crate) stdout_equals: Option<String>,
    pub(crate) stderr_contains: Option<String>,
}

/// One validated `op:"workflow"` node: the `parallel` task fields, decoded by
/// the same rules ([`decode_task`]), plus the workflow-only ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowTask {
    pub(crate) base: TaskRequest,
    /// Resolved request indices in the order the client listed them; a
    /// dependency always precedes the node in [`WorkflowRequest::order`].
    pub(crate) depends_on: Vec<usize>,
    pub(crate) retry: Option<RetryPolicy>,
    pub(crate) condition: Option<Condition>,
    /// Named semaphore of one (design §8); held for the whole node.
    pub(crate) lock: Option<String>,
}

/// One validated `op:"workflow"` request (design §2.1). The scheduler consumes
/// [`order`](Self::order) and never recomputes it (design §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowRequest {
    pub(crate) id: String,
    pub(crate) policy: ParallelPolicy,
    pub(crate) failure_policy: FailurePolicy,
    /// Validated against `limits.max_parallel`; the effective window is
    /// `min(max_concurrency, tasks.len())`.
    pub(crate) max_concurrency: usize,
    /// Group wall clock, counted from the first spawn; `None` is unbounded.
    pub(crate) timeout_ms: Option<i64>,
    /// Request order, unique ids, never empty, at most
    /// `limits.max_parallel_tasks` entries.
    pub(crate) tasks: Vec<WorkflowTask>,
    /// Every request index exactly once, dependency before dependent, with the
    /// request index as the deterministic tie-break (design §3.2).
    pub(crate) order: Vec<usize>,
}

/// Longest accepted `tasks[].lock` name (design §8): a lock is a scheduler
/// semaphore key, not a path, so a short name keeps the response readable.
const MAX_LOCK_BYTES: usize = 64;

/// Largest accepted `condition.exit_code.in` list (design §6.1).
const MAX_CONDITION_EXIT_CODES: usize = 8;

/// Validates an `op:"workflow"` request and its DAG (design §2.2, §3).
///
/// Order, so only the first failure is reported and it is the most structural
/// one: group fields (`tasks` present/array/non-empty/bounded, `policy`,
/// `failure_policy`, `max_concurrency`, group `timeout_ms`), then every task's
/// own fields in request order, then id uniqueness, then `depends_on`
/// references, then cycles, then conditions. Conditions come last because a
/// condition asserts something about an edge that must exist first.
///
/// Unknown keys are ignored for forward compatibility, except inside
/// `condition`, whose closed predicate set would let a typo silently weaken a
/// gate.
///
/// Validation refuses the whole request: nothing runs, so a malformed request
/// cannot leave a partially spawned DAG behind.
pub(crate) fn decode_workflow(
    value: &Value,
    limits: &Limits,
    id: String,
) -> Result<WorkflowRequest, DecodeError> {
    let entries = match value.get("tasks") {
        Some(Value::Array(entries)) if entries.is_empty() => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "workflow requires at least one task",
                Some(&id),
            ));
        }
        Some(Value::Array(entries)) => entries,
        Some(_) => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "tasks must be an array",
                Some(&id),
            ));
        }
        None => {
            return Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                "workflow requires tasks",
                Some(&id),
            ));
        }
    };
    if entries.len() > limits.max_parallel_tasks {
        return Err(DecodeError::with_id(
            ErrorCode::TooManyTasks,
            format!(
                "the request has {} tasks; the limit is {}",
                entries.len(),
                limits.max_parallel_tasks
            ),
            Some(&id),
        ));
    }

    let policy = decode_group_policy(value, &id)?;
    let failure_policy = decode_failure_policy(value, &id)?;
    let max_concurrency = decode_max_concurrency(value, limits, &id)?;
    let timeout_ms = decode_group_timeout(value, &id)?;

    // Pass one: every task's own fields, in request order, with the `parallel`
    // task rules (the shared helper, so the two ops cannot drift) plus the
    // workflow-only ones. `depends_on` keeps the client's ids here; resolving
    // them needs the whole id set, which pass two builds.
    let mut tasks: Vec<WorkflowTask> = Vec::with_capacity(entries.len());
    let mut requested_depends_on: Vec<Vec<String>> = Vec::with_capacity(entries.len());
    let mut requested_conditions: Vec<Option<&Value>> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let base = decode_task(entry, limits, index, &id)?;
        let depends_on = decode_depends_on(entry, index, &id)?;
        let retry = decode_retry(entry, limits, index, &id)?;
        let lock = decode_lock_name(entry, &id, &format!("tasks[{index}]."))?;
        // Only the presence is recorded here: a condition is validated against
        // the edge set, which does not exist until pass two completes.
        requested_conditions.push(entry.get("condition"));
        requested_depends_on.push(depends_on);
        tasks.push(WorkflowTask {
            base,
            depends_on: Vec::new(),
            retry,
            condition: None,
            lock,
        });
    }

    // Uniqueness is a property of the id set, so it is checked after every
    // task's own fields (`DUPLICATE_TASK_ID`, same message shape as parallel).
    for (index, task) in tasks.iter().enumerate() {
        if let Some(earlier) = tasks[..index]
            .iter()
            .position(|seen| seen.base.id == task.base.id)
        {
            return Err(DecodeError::with_id(
                ErrorCode::DuplicateTaskId,
                format!(
                    "tasks[{index}].id \"{}\" is already used by tasks[{earlier}]",
                    task.base.id
                ),
                Some(&id),
            ));
        }
    }

    resolve_dependencies(&mut tasks, &requested_depends_on, &id)?;

    // Kahn runs anyway to produce `order`, so it is the cheap primary cycle
    // check; only a short graph pays for the second walk (design §3.2).
    let order = topological_order(&tasks);
    if order.len() != tasks.len() {
        let cycle = find_cycle(&tasks);
        let mut message = String::from("cycle: ");
        for (position, &index) in cycle.iter().enumerate() {
            if position > 0 {
                message.push_str(" -> ");
            }
            message.push_str(&tasks[index].base.id);
        }
        return Err(DecodeError::with_id(
            ErrorCode::WorkflowCycle,
            message,
            Some(&id),
        ));
    }

    for (index, condition) in requested_conditions.into_iter().enumerate() {
        let Some(condition) = condition else { continue };
        let decoded = decode_condition(condition, index, &tasks, &id)?;
        tasks[index].condition = Some(decoded);
    }

    Ok(WorkflowRequest {
        id,
        policy,
        failure_policy,
        max_concurrency,
        timeout_ms,
        tasks,
        order,
    })
}

/// `failure_policy` of a workflow group (design §4.3): orthogonal to
/// `policy`, so it has its own vocabulary and is never coerced.
fn decode_failure_policy(value: &Value, request_id: &str) -> Result<FailurePolicy, DecodeError> {
    match value.get("failure_policy") {
        None => Ok(FailurePolicy::Skip),
        Some(Value::String(name)) => match name.as_str() {
            "skip" => Ok(FailurePolicy::Skip),
            "fail" => Ok(FailurePolicy::Fail),
            "continue" => Ok(FailurePolicy::Continue),
            other => Err(DecodeError::with_id(
                ErrorCode::InvalidRequest,
                format!("unknown failure_policy: {other}; expected skip, fail or continue"),
                Some(request_id),
            )),
        },
        Some(_) => Err(DecodeError::with_id(
            ErrorCode::InvalidRequest,
            "failure_policy must be a string",
            Some(request_id),
        )),
    }
}

/// `tasks[i].depends_on` as written: ids only, references resolved later.
fn decode_depends_on(
    value: &Value,
    index: usize,
    request_id: &str,
) -> Result<Vec<String>, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidDependency, message, Some(request_id))
    };
    match value.get("depends_on") {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut ids = Vec::with_capacity(items.len());
            for (position, item) in items.iter().enumerate() {
                let Some(dependency) = item.as_str() else {
                    return Err(reject(format!(
                        "tasks[{index}].depends_on[{position}] must be a string"
                    )));
                };
                ids.push(dependency.to_string());
            }
            Ok(ids)
        }
        Some(_) => Err(reject(format!(
            "tasks[{index}].depends_on must be an array of strings"
        ))),
    }
}

/// `tasks[i].retry` (design §5.1). Absent means one attempt; a present object
/// fills the defaults of the design's table.
fn decode_retry(
    value: &Value,
    limits: &Limits,
    index: usize,
    request_id: &str,
) -> Result<Option<RetryPolicy>, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    let Some(retry) = value.get("retry") else {
        return Ok(None);
    };
    if retry.as_object().is_none() {
        return Err(reject(format!("tasks[{index}].retry must be an object")));
    }

    let max_attempts = match retry.get("max_attempts") {
        None => 1,
        Some(Value::Int(attempts)) => {
            let attempts = usize::try_from(*attempts)
                .ok()
                .filter(|attempts| (1..=limits.max_workflow_attempts).contains(attempts));
            match attempts {
                Some(attempts) => attempts,
                None => {
                    return Err(reject(format!(
                        "tasks[{index}].retry.max_attempts must be between 1 and {}",
                        limits.max_workflow_attempts
                    )));
                }
            }
        }
        Some(_) => {
            return Err(reject(format!(
                "tasks[{index}].retry.max_attempts must be an integer"
            )));
        }
    };

    let on = match retry.get("on") {
        // The two outcomes that say "the attempt did not get a fair chance".
        None => vec![RetryOn::Timeout, RetryOn::SpawnError],
        Some(Value::Array(outcomes)) if outcomes.is_empty() => {
            return Err(reject(format!(
                "tasks[{index}].retry.on must list at least one outcome"
            )));
        }
        Some(Value::Array(outcomes)) => {
            let mut on = Vec::with_capacity(outcomes.len());
            for (position, outcome) in outcomes.iter().enumerate() {
                let Some(name) = outcome.as_str() else {
                    return Err(reject(format!(
                        "tasks[{index}].retry.on[{position}] must be a string"
                    )));
                };
                let outcome = match name {
                    "failure" => RetryOn::Failure,
                    "timeout" => RetryOn::Timeout,
                    "spawn_error" => RetryOn::SpawnError,
                    other => {
                        return Err(reject(format!(
                            "unknown tasks[{index}].retry.on[{position}]: {other}; expected failure, timeout or spawn_error"
                        )));
                    }
                };
                on.push(outcome);
            }
            on
        }
        Some(_) => {
            return Err(reject(format!(
                "tasks[{index}].retry.on must be an array of strings"
            )));
        }
    };

    let backoff_ms = match retry.get("backoff_ms") {
        None => 0,
        Some(Value::Int(ms)) => {
            let in_range =
                u64::try_from(*ms).is_ok_and(|ms| ms <= limits.max_workflow_backoff_ms as u64);
            if !in_range {
                return Err(reject(format!(
                    "tasks[{index}].retry.backoff_ms must be between 0 and {}",
                    limits.max_workflow_backoff_ms
                )));
            }
            *ms
        }
        Some(_) => {
            return Err(reject(format!(
                "tasks[{index}].retry.backoff_ms must be an integer"
            )));
        }
    };

    let backoff = match retry.get("backoff") {
        None => Backoff::Fixed,
        Some(Value::String(name)) => match name.as_str() {
            "fixed" => Backoff::Fixed,
            "exponential" => Backoff::Exponential,
            other => {
                return Err(reject(format!(
                    "unknown tasks[{index}].retry.backoff: {other}; expected fixed or exponential"
                )));
            }
        },
        Some(_) => {
            return Err(reject(format!(
                "tasks[{index}].retry.backoff must be a string"
            )));
        }
    };

    Ok(Some(RetryPolicy {
        max_attempts,
        on,
        backoff_ms,
        backoff,
    }))
}

/// Lock name (`1..=64` bytes, `[A-Za-z0-9._:-]`).
fn decode_lock_name(
    value: &Value,
    request_id: &str,
    prefix: &str,
) -> Result<Option<String>, DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidRequest, message, Some(request_id))
    };
    match value.get("lock") {
        None => Ok(None),
        Some(Value::String(lock)) if lock.is_empty() => {
            Err(reject(format!("{prefix}lock must not be empty")))
        }
        Some(Value::String(lock)) if lock.len() > MAX_LOCK_BYTES => Err(reject(format!(
            "{prefix}lock is {} bytes; the limit is {MAX_LOCK_BYTES}",
            lock.len()
        ))),
        Some(Value::String(lock))
            if !lock
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-')) =>
        {
            Err(reject(format!(
                "{prefix}lock must only contain [A-Za-z0-9._:-]"
            )))
        }
        Some(Value::String(lock)) => Ok(Some(lock.clone())),
        Some(_) => Err(reject(format!("{prefix}lock must be a string"))),
    }
}

/// `tasks[i].condition` (design §6.1): one dependency of this node, at least
/// one predicate, and `exit_code` with exactly one operator.
fn decode_condition(
    value: &Value,
    index: usize,
    tasks: &[WorkflowTask],
    request_id: &str,
) -> Result<Condition, DecodeError> {
    let invalid = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidCondition, message, Some(request_id))
    };
    let Some(entries) = value.as_object() else {
        return Err(invalid(format!(
            "tasks[{index}].condition must be an object"
        )));
    };
    // A predicate this build ignored would silently weaken the gate, so the
    // closed set of design §6.1 is enforced instead of the usual "ignore
    // unknown keys".
    for (name, _) in entries {
        if !matches!(
            name.as_str(),
            "task" | "exit_code" | "stdout_contains" | "stdout_equals" | "stderr_contains"
        ) {
            return Err(invalid(format!(
                "tasks[{index}].condition has an unknown key: {name}"
            )));
        }
    }

    let task = match value.get("task") {
        Some(Value::String(task)) if task.is_empty() => {
            return Err(invalid(format!(
                "tasks[{index}].condition.task must not be empty"
            )));
        }
        Some(Value::String(task)) => task,
        Some(_) => {
            return Err(invalid(format!(
                "tasks[{index}].condition.task must be a string"
            )));
        }
        None => {
            return Err(invalid(format!("tasks[{index}].condition requires task")));
        }
    };
    let Some(task_index) = tasks
        .iter()
        .position(|candidate| candidate.base.id == *task)
    else {
        return Err(invalid(format!(
            "tasks[{index}].condition.task \"{task}\" does not name a task in this request"
        )));
    };
    if !tasks[index].depends_on.contains(&task_index) {
        return Err(invalid(format!(
            "tasks[{index}].condition.task \"{task}\" is not in tasks[{index}].depends_on"
        )));
    }

    let exit_code = match value.get("exit_code") {
        None => None,
        Some(Value::Object(operators)) => {
            let mut matcher = None;
            for (operator, operand) in operators {
                if matcher.is_some() {
                    return Err(invalid(format!(
                        "tasks[{index}].condition.exit_code must use exactly one of eq, ne or in"
                    )));
                }
                matcher = Some(match (operator.as_str(), operand) {
                    ("eq", Value::Int(code)) => ExitCodeMatcher::Eq(*code),
                    ("ne", Value::Int(code)) => ExitCodeMatcher::Ne(*code),
                    ("in", Value::Array(codes)) => {
                        if codes.is_empty() || codes.len() > MAX_CONDITION_EXIT_CODES {
                            return Err(invalid(format!(
                                "tasks[{index}].condition.exit_code.in must have between 1 and {MAX_CONDITION_EXIT_CODES} entries"
                            )));
                        }
                        let mut matching = Vec::with_capacity(codes.len());
                        for code in codes {
                            let Value::Int(code) = code else {
                                return Err(invalid(format!(
                                    "tasks[{index}].condition.exit_code.in entries must be integers"
                                )));
                            };
                            matching.push(*code);
                        }
                        ExitCodeMatcher::In(matching)
                    }
                    ("eq" | "ne", _) => {
                        return Err(invalid(format!(
                            "tasks[{index}].condition.exit_code.{operator} must be an integer"
                        )));
                    }
                    ("in", _) => {
                        return Err(invalid(format!(
                            "tasks[{index}].condition.exit_code.in must be an array of integers"
                        )));
                    }
                    (other, _) => {
                        return Err(invalid(format!(
                            "unknown tasks[{index}].condition.exit_code operator: {other}; expected eq, ne or in"
                        )));
                    }
                });
            }
            match matcher {
                Some(matcher) => Some(matcher),
                None => {
                    return Err(invalid(format!(
                        "tasks[{index}].condition.exit_code must use exactly one of eq, ne or in"
                    )));
                }
            }
        }
        Some(_) => {
            return Err(invalid(format!(
                "tasks[{index}].condition.exit_code must be an object"
            )));
        }
    };

    let text = |key: &str| -> Result<Option<String>, DecodeError> {
        match value.get(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(_) => Err(invalid(format!(
                "tasks[{index}].condition.{key} must be a string"
            ))),
        }
    };
    let stdout_contains = text("stdout_contains")?;
    let stdout_equals = text("stdout_equals")?;
    let stderr_contains = text("stderr_contains")?;

    if exit_code.is_none()
        && stdout_contains.is_none()
        && stdout_equals.is_none()
        && stderr_contains.is_none()
    {
        return Err(invalid(format!(
            "tasks[{index}].condition asserts nothing; add exit_code, stdout_contains, stdout_equals or stderr_contains"
        )));
    }

    Ok(Condition {
        task: task.clone(),
        task_index,
        exit_code,
        stdout_contains,
        stdout_equals,
        stderr_contains,
    })
}

/// Resolves each `depends_on` id to its request index (design §3.1 step 3).
/// A self-reference, a repeated edge and an absent id are all client bugs, so
/// none of them is normalized away; the message names `tasks[i].depends_on[j]`
/// and the offending value.
fn resolve_dependencies(
    tasks: &mut [WorkflowTask],
    requested: &[Vec<String>],
    request_id: &str,
) -> Result<(), DecodeError> {
    let reject = |message: String| {
        DecodeError::with_id(ErrorCode::InvalidDependency, message, Some(request_id))
    };
    for index in 0..tasks.len() {
        for (position, dependency) in requested[index].iter().enumerate() {
            let label = format!("tasks[{index}].depends_on[{position}] = \"{dependency}\"");
            if *dependency == tasks[index].base.id {
                return Err(reject(format!("{label}: a task cannot depend on itself")));
            }
            if requested[index][..position].contains(dependency) {
                return Err(reject(format!("{label}: the dependency is repeated")));
            }
            let Some(target) = tasks
                .iter()
                .position(|candidate| candidate.base.id == *dependency)
            else {
                return Err(reject(format!(
                    "{label}: no task in this request has that id"
                )));
            };
            tasks[index].depends_on.push(target);
        }
    }
    Ok(())
}

/// Kahn's algorithm over the resolved edges, with the request index as the
/// tie-break, so `order` is stable and explainable (design §3.2). A short
/// result means the graph has a cycle.
fn topological_order(tasks: &[WorkflowTask]) -> Vec<usize> {
    let mut in_degree = Vec::with_capacity(tasks.len());
    let mut successors = vec![Vec::new(); tasks.len()];
    for (index, task) in tasks.iter().enumerate() {
        in_degree.push(task.depends_on.len());
        for &dependency in &task.depends_on {
            successors[dependency].push(index);
        }
    }
    let mut ready: BinaryHeap<Reverse<usize>> = (0..tasks.len())
        .filter(|&index| in_degree[index] == 0)
        .map(Reverse)
        .collect();
    let mut order = Vec::with_capacity(tasks.len());
    while let Some(Reverse(index)) = ready.pop() {
        order.push(index);
        for &next in &successors[index] {
            in_degree[next] -= 1;
            if in_degree[next] == 0 {
                ready.push(Reverse(next));
            }
        }
    }
    order
}

/// One real cycle of the unresolved graph, as request indices with the first
/// node repeated at the end, so the caller renders `cycle: a -> b -> a`
/// (design §3.2). Iterative DFS: the graph is at most 128 nodes, but recursion
/// depth would be a client-controlled input.
fn find_cycle(tasks: &[WorkflowTask]) -> Vec<usize> {
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;

    let mut state = vec![WHITE; tasks.len()];
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut path: Vec<usize> = Vec::new();
    for start in 0..tasks.len() {
        if state[start] != WHITE {
            continue;
        }
        state[start] = GRAY;
        path.push(start);
        stack.push((start, 0));
        while let Some(&(node, next)) = stack.last() {
            if next >= tasks[node].depends_on.len() {
                state[node] = BLACK;
                stack.pop();
                path.pop();
                continue;
            }
            stack.last_mut().expect("the stack is not empty").1 += 1;
            let dependency = tasks[node].depends_on[next];
            match state[dependency] {
                GRAY => {
                    let from = path
                        .iter()
                        .position(|&seen| seen == dependency)
                        .expect("a gray node is on the path");
                    let mut cycle = path[from..].to_vec();
                    cycle.push(dependency);
                    return cycle;
                }
                WHITE => {
                    state[dependency] = GRAY;
                    path.push(dependency);
                    stack.push((dependency, 0));
                }
                _ => {}
            }
        }
    }
    // Kahn already proved a cycle exists, and this walk visits every edge of
    // the graph, so a back edge is guaranteed.
    unreachable!("Kahn reported unresolved nodes but the depth-first walk found no cycle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::json::{Request, parse_line};

    fn limits() -> Limits {
        Limits::default()
    }

    fn decode_line(line: &str) -> Result<DecodedRequest, DecodeError> {
        let value = parse_line(line.as_bytes()).expect("valid JSON");
        decode(&value, &limits())
    }

    fn rejected(line: &str) -> DecodeError {
        decode_line(line).expect_err("expected a rejected request")
    }

    #[test]
    fn exec_decodes_command_and_defaults() {
        let request =
            decode_line(r#"{"schema_version":1,"id":"r1","op":"exec","command":"echo hi"}"#)
                .expect("valid exec");
        let DecodedRequest::Exec(request) = request else {
            panic!("not exec: {request:?}");
        };
        assert_eq!(request.id, "r1");
        assert_eq!(request.command(), Some("echo hi"));
        assert_eq!(request.stdin, None);
        assert!(request.env_delta);
        assert_eq!(request.timeout_ms, None);
    }

    #[test]
    fn exec_decodes_a_timeout_and_rejects_a_bad_one() {
        for (line, expected) in [
            (
                r#"{"schema_version":1,"id":"r","op":"exec","command":"sleep 5","timeout_ms":100}"#,
                Some(100),
            ),
            (
                r#"{"schema_version":1,"id":"r","op":"exec","command":"sleep 5","timeout_ms":0}"#,
                Some(0),
            ),
            (
                r#"{"schema_version":1,"id":"r","op":"exec","command":"true"}"#,
                None,
            ),
        ] {
            let DecodedRequest::Exec(request) = decode_line(line).expect("valid exec") else {
                panic!("not exec");
            };
            assert_eq!(request.timeout_ms, expected, "{line}");
        }
        for line in [
            r#"{"schema_version":1,"id":"r","op":"exec","command":"true","timeout_ms":-1}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":"true","timeout_ms":"10"}"#,
        ] {
            assert_eq!(rejected(line).code, ErrorCode::InvalidRequest, "{line}");
        }
    }

    #[test]
    fn cancel_decodes_target_and_bounds_the_grace() {
        let DecodedRequest::Cancel(request) =
            decode_line(r#"{"schema_version":1,"id":"c","op":"cancel","target":"r1"}"#)
                .expect("valid cancel")
        else {
            panic!("not cancel");
        };
        assert_eq!(request.id, "c");
        assert_eq!(request.target, "r1");
        assert_eq!(request.grace_ms, None);

        let DecodedRequest::Cancel(request) = decode_line(
            r#"{"schema_version":1,"id":"c","op":"cancel","target":"r1","grace_ms":200}"#,
        )
        .expect("valid cancel") else {
            panic!("not cancel");
        };
        assert_eq!(request.grace_ms, Some(200));

        for line in [
            r#"{"schema_version":1,"id":"c","op":"cancel"}"#,
            r#"{"schema_version":1,"id":"c","op":"cancel","target":""}"#,
            r#"{"schema_version":1,"id":"c","op":"cancel","target":7}"#,
            r#"{"schema_version":1,"id":"c","op":"cancel","target":"r1","grace_ms":-1}"#,
            r#"{"schema_version":1,"id":"c","op":"cancel","target":"r1","grace_ms":2001}"#,
            r#"{"schema_version":1,"id":"c","op":"cancel","target":"r1","grace_ms":"200"}"#,
        ] {
            let error = rejected(line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{line}");
            assert_eq!(error.id.as_deref(), Some("c"), "{line}");
        }
    }

    #[test]
    fn exec_decodes_utf8_and_base64_stdin() {
        for (line, expected) in [
            (
                r#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"hi\n"}"#,
                b"hi\n".to_vec(),
            ),
            (
                r#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"aGkK","stdin_encoding":"base64"}"#,
                b"hi\n".to_vec(),
            ),
        ] {
            let DecodedRequest::Exec(request) = decode_line(line).expect("valid exec") else {
                panic!("not exec");
            };
            assert_eq!(request.stdin, Some(expected));
        }
    }

    #[test]
    fn state_decodes_and_deduplicates_include() {
        let DecodedRequest::State(request) = decode_line(
            r#"{"schema_version":1,"id":"s","op":"state","include":["env","traps","env"]}"#,
        )
        .expect("valid state") else {
            panic!("not state");
        };
        assert_eq!(
            request.include,
            vec![StateSection::Env, StateSection::Traps]
        );
    }

    #[test]
    fn state_defaults_to_no_include() {
        let DecodedRequest::State(request) =
            decode_line(r#"{"schema_version":1,"id":"s","op":"state"}"#).expect("valid state")
        else {
            panic!("not state");
        };
        assert!(request.include.is_empty());
    }

    #[test]
    fn close_decodes_wait_ms() {
        let DecodedRequest::Close(request) =
            decode_line(r#"{"schema_version":1,"id":"c","op":"close","wait_ms":5}"#)
                .expect("valid close")
        else {
            panic!("not close");
        };
        assert_eq!(request.id, "c");
        assert_eq!(request.wait_ms, Some(5));
        let DecodedRequest::Close(request) =
            decode_line(r#"{"schema_version":1,"id":"c","op":"close"}"#).expect("valid close")
        else {
            panic!("not close");
        };
        assert_eq!(request.wait_ms, None);
        assert_eq!(
            rejected(r#"{"schema_version":1,"id":"c","op":"close","wait_ms":-1}"#).code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn a_non_object_is_invalid_request() {
        let error = rejected("[1,2]");
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.id, None);
    }

    #[test]
    fn a_wrong_schema_version_is_reported_and_echoes_the_id() {
        let error = rejected(r#"{"schema_version":2,"id":"r1","op":"exec","command":"true"}"#);
        assert_eq!(error.code, ErrorCode::UnsupportedSchemaVersion);
        assert_eq!(error.id.as_deref(), Some("r1"));
        let error = rejected(r#"{"id":"r1","op":"exec","command":"true"}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        let error = rejected(r#"{"schema_version":"1","id":"r1","op":"exec","command":"true"}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn an_unusable_id_is_invalid_id_without_an_echo() {
        for line in [
            r#"{"schema_version":1,"op":"exec","command":"true"}"#,
            r#"{"schema_version":1,"id":null,"op":"exec","command":"true"}"#,
            r#"{"schema_version":1,"id":"","op":"exec","command":"true"}"#,
            r#"{"schema_version":1,"id":7,"op":"exec","command":"true"}"#,
        ] {
            let error = rejected(line);
            assert_eq!(error.code, ErrorCode::InvalidId, "{line}");
            assert_eq!(error.id, None, "{line}");
        }
        let long = "a".repeat(257);
        let error = rejected(&format!(
            r#"{{"schema_version":1,"id":"{long}","op":"exec","command":"true"}}"#
        ));
        assert_eq!(error.code, ErrorCode::InvalidId);
        assert_eq!(error.id, None);
    }

    #[test]
    fn an_unknown_op_is_never_fatal_and_cancel_is_honored() {
        let error = rejected(r#"{"schema_version":1,"id":"r","op":"bogus"}"#);
        assert_eq!(error.code, ErrorCode::UnknownOp);
        assert_eq!(error.id.as_deref(), Some("r"));
        assert!(!error.code.is_fatal());
        // Both `parallel` and `workflow` are honored now, so this shape
        // reaches their own validators instead of `UNKNOWN_OP`.
        let error = rejected(r#"{"schema_version":1,"id":"r","op":"parallel"}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        let error = rejected(r#"{"schema_version":1,"id":"r","op":"workflow"}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(matches!(
            decode_line(r#"{"schema_version":1,"id":"r","op":"cancel","target":"r1"}"#),
            Ok(DecodedRequest::Cancel(_))
        ));
        let error = rejected(r#"{"schema_version":1,"id":"r","op":5}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        let error = rejected(r#"{"schema_version":1,"id":"r"}"#);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn exec_field_types_are_enforced() {
        for line in [
            r#"{"schema_version":1,"id":"r","op":"exec"}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":5}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":"x","env_delta":"yes"}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":"x","stdin":5}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":"x","stdin_encoding":"base64"}"#,
            r#"{"schema_version":1,"id":"r","op":"exec","command":"x","timeout_ms":"10"}"#,
        ] {
            let error = rejected(line);
            assert!(
                matches!(
                    error.code,
                    ErrorCode::InvalidRequest | ErrorCode::InvalidEncoding
                ),
                "{line}: {:?}",
                error.code
            );
        }
    }

    #[test]
    fn malformed_base64_is_invalid_encoding() {
        let error = rejected(
            r#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"aGk","stdin_encoding":"base64"}"#,
        );
        assert_eq!(error.code, ErrorCode::InvalidEncoding);
        assert_eq!(error.id.as_deref(), Some("r"));
        let error = rejected(
            r#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"!!!!","stdin_encoding":"base64"}"#,
        );
        assert_eq!(error.code, ErrorCode::InvalidEncoding);
        let error = rejected(
            r#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"hi","stdin_encoding":"latin1"}"#,
        );
        assert_eq!(error.code, ErrorCode::InvalidEncoding);
    }

    #[test]
    fn a_stdin_payload_over_the_limit_is_refused() {
        let limits = Limits {
            max_stdin_bytes: 4,
            ..Limits::default()
        };
        let value = parse_line(
            br#"{"schema_version":1,"id":"r","op":"exec","command":"cat","stdin":"hello"}"#,
        )
        .expect("valid JSON");
        let error = decode(&value, &limits).expect_err("too large");
        assert_eq!(error.code, ErrorCode::StdinTooLarge);
        assert_eq!(error.id.as_deref(), Some("r"));
    }

    #[test]
    fn state_sections_are_validated() {
        for line in [
            r#"{"schema_version":1,"id":"s","op":"state","include":"env"}"#,
            r#"{"schema_version":1,"id":"s","op":"state","include":[1]}"#,
            r#"{"schema_version":1,"id":"s","op":"state","include":["secrets"]}"#,
        ] {
            assert_eq!(rejected(line).code, ErrorCode::InvalidRequest, "{line}");
        }
    }

    #[test]
    fn request_id_only_accepts_a_well_shaped_id() {
        let value = parse_line(br#"{"id":"r1","op":"exec"}"#).expect("valid JSON");
        assert_eq!(request_id(&value, &limits()).as_deref(), Some("r1"));
        let value = parse_line(br#"{"id":"","op":"exec"}"#).expect("valid JSON");
        assert_eq!(request_id(&value, &limits()), None);
        let value = parse_line(br#"{"id":5}"#).expect("valid JSON");
        assert_eq!(request_id(&value, &limits()), None);
    }

    #[test]
    fn the_encoder_types_and_the_decoder_agree_on_every_op() {
        use crate::agent::json::{
            CancelRequest, CloseRequest as EncodedClose, ExecRequest as EncodedExec,
            StateRequest as EncodedState,
        };
        let lines = [
            Request::Exec(EncodedExec::new("r1", "echo hi")).encode(),
            Request::State(EncodedState {
                id: "r2".to_string(),
                include: vec![StateSection::Env, StateSection::Jobs],
            })
            .encode(),
            Request::Close(EncodedClose {
                id: "r3".to_string(),
                wait_ms: None,
            })
            .encode(),
        ];
        for line in lines {
            decode_line(&line).unwrap_or_else(|error| panic!("{line}: {error:?}"));
        }
        let cancel = Request::Cancel(CancelRequest {
            id: "r4".to_string(),
            target: "r1".to_string(),
            task: None,
            grace_ms: None,
        })
        .encode();
        let DecodedRequest::Cancel(request) =
            decode_line(&cancel).unwrap_or_else(|error| panic!("{cancel}: {error:?}"))
        else {
            panic!("not cancel");
        };
        assert_eq!(request.id, "r4");
        assert_eq!(request.target, "r1");
        assert_eq!(request.grace_ms, None);

        let exec = Request::Exec(EncodedExec {
            timeout_ms: Some(250),
            ..EncodedExec::new("r5", "sleep 5")
        })
        .encode();
        let DecodedRequest::Exec(request) =
            decode_line(&exec).unwrap_or_else(|error| panic!("{exec}: {error:?}"))
        else {
            panic!("not exec");
        };
        assert_eq!(request.timeout_ms, Some(250));
    }

    // ------------------------------------------------------ workflow wire contract

    fn workflow(line: &str) -> Result<WorkflowRequest, DecodeError> {
        workflow_with(line, &limits())
    }

    fn workflow_with(line: &str, limits: &Limits) -> Result<WorkflowRequest, DecodeError> {
        let value = parse_line(line.as_bytes()).expect("valid JSON");
        decode_workflow(&value, limits, "w1".to_string())
    }

    fn workflow_rejected(line: &str) -> DecodeError {
        workflow(line).expect_err("expected a rejected workflow request")
    }

    fn workflow_line(tasks: &str) -> String {
        format!(r#"{{"schema_version":1,"id":"w1","op":"workflow","tasks":[{tasks}]}}"#)
    }

    /// A request with one dependency and one malformed-condition slot: `gate`
    /// and `other` exist, `probe` depends only on `gate`.
    fn condition_line(condition: &str) -> String {
        workflow_line(&format!(
            r#"{{"id":"gate","command":"true"}},{{"id":"other","command":"true"}},{{"id":"probe","command":"true","depends_on":["gate"],"condition":{condition}}}"#
        ))
    }

    /// Every index appears once and every dependency precedes its node.
    fn assert_topological(request: &WorkflowRequest) {
        assert_eq!(request.order.len(), request.tasks.len());
        let mut seen = vec![false; request.tasks.len()];
        for &index in &request.order {
            assert!(!seen[index], "index {index} appears twice");
            for &dependency in &request.tasks[index].depends_on {
                assert!(
                    seen[dependency],
                    "{} precedes its dependency {}",
                    request.tasks[index].base.id, request.tasks[dependency].base.id
                );
            }
            seen[index] = true;
        }
    }

    #[test]
    fn workflow_decodes_group_defaults_and_one_attempt() {
        let request = workflow(&workflow_line(
            r#"{"id":"status","command":"git status --short"}"#,
        ))
        .expect("valid workflow");
        assert_eq!(request.id, "w1");
        assert_eq!(request.policy, ParallelPolicy::WaitAll);
        assert_eq!(request.failure_policy, FailurePolicy::Skip);
        assert_eq!(request.max_concurrency, 4);
        assert_eq!(request.timeout_ms, None);
        assert_eq!(request.order, vec![0]);

        let task = &request.tasks[0];
        assert_eq!(task.base.id, "status");
        assert_eq!(task.base.command(), Some("git status --short"));
        assert_eq!(task.base.stdin, None);
        assert_eq!(task.base.timeout_ms, None);
        assert_eq!(task.base.cwd, None);
        assert!(task.base.env.is_empty());
        assert!(!task.base.env_delta);
        assert!(task.depends_on.is_empty());
        // No `retry` means exactly one attempt (design §5.1).
        assert_eq!(task.retry, None);
        assert_eq!(task.condition, None);
        assert_eq!(task.lock, None);
    }

    #[test]
    fn workflow_decodes_every_group_and_node_field() {
        let request = workflow(
            r#"{"schema_version":1,"id":"w1","op":"workflow","policy":"fail-fast",
                "failure_policy":"continue","max_concurrency":3,"timeout_ms":600000,
                "tasks":[
                  {"id":"status","command":"git status --short"},
                  {"id":"tests","command":"cargo test -q","lock":"cargo-target",
                   "timeout_ms":120000,"cwd":"/repo","env":{"GIT_PAGER":"cat","DROP":null},
                   "stdin":"aGkK","stdin_encoding":"base64","env_delta":true,
                   "retry":{"max_attempts":8,"on":["failure","timeout","spawn_error"],
                            "backoff_ms":60000,"backoff":"exponential"}},
                  {"id":"report","command":"scripts/report.sh","depends_on":["status"],
                   "condition":{"task":"status","exit_code":{"in":[0,2]}}}
                ]}"#,
        )
        .expect("every field");
        assert_eq!(request.policy, ParallelPolicy::FailFast);
        assert_eq!(request.failure_policy, FailurePolicy::Continue);
        assert_eq!(request.max_concurrency, 3);
        assert_eq!(request.timeout_ms, Some(600_000));
        assert_eq!(request.order, vec![0, 1, 2]);

        let tests = &request.tasks[1];
        assert_eq!(tests.lock.as_deref(), Some("cargo-target"));
        assert_eq!(tests.base.timeout_ms, Some(120_000));
        assert_eq!(tests.base.cwd.as_deref(), Some("/repo"));
        assert_eq!(tests.base.stdin, Some(b"hi\n".to_vec()));
        assert_eq!(
            tests.base.env,
            vec![
                ("GIT_PAGER".to_string(), Some("cat".to_string())),
                ("DROP".to_string(), None),
            ]
        );
        assert!(tests.base.env_delta);
        assert_eq!(
            tests.retry,
            Some(RetryPolicy {
                max_attempts: 8,
                on: vec![RetryOn::Failure, RetryOn::Timeout, RetryOn::SpawnError],
                backoff_ms: 60_000,
                backoff: Backoff::Exponential,
            })
        );

        let report = &request.tasks[2];
        assert_eq!(report.depends_on, vec![0]);
        assert_eq!(
            report.condition,
            Some(Condition {
                task: "status".to_string(),
                task_index: 0,
                exit_code: Some(ExitCodeMatcher::In(vec![0, 2])),
                stdout_contains: None,
                stdout_equals: None,
                stderr_contains: None,
            })
        );
    }

    #[test]
    fn workflow_orders_a_diamond_dependency_before_dependents() {
        let request = workflow(&workflow_line(
            r#"{"id":"a","command":"true"},
               {"id":"b","command":"true","depends_on":["a"]},
               {"id":"c","command":"true","depends_on":["a"]},
               {"id":"d","command":"true","depends_on":["b","c"]}"#,
        ))
        .expect("diamond");
        assert_eq!(request.order, vec![0, 1, 2, 3]);
        assert_eq!(request.tasks[3].depends_on, vec![1, 2]);
        assert_topological(&request);
    }

    #[test]
    fn workflow_breaks_ready_ties_by_request_index() {
        // `report` is written first but is only ready once `status` and `tests`
        // ran; `fmt` is ready from the start but has a higher index than both.
        let request = workflow(&workflow_line(
            r#"{"id":"report","command":"true","depends_on":["status","tests"]},
               {"id":"status","command":"true"},
               {"id":"tests","command":"true"},
               {"id":"fmt","command":"true"}"#,
        ))
        .expect("ties");
        assert_eq!(request.order, vec![1, 2, 0, 3]);
        assert_topological(&request);
    }

    #[test]
    fn workflow_orders_a_hundred_node_chain() {
        let body: Vec<String> = (0..100)
            .map(|index| {
                if index == 0 {
                    format!(r#"{{"id":"t{index}","command":"true"}}"#)
                } else {
                    format!(
                        r#"{{"id":"t{index}","command":"true","depends_on":["t{}"]}}"#,
                        index - 1
                    )
                }
            })
            .collect();
        let request = workflow(&workflow_line(&body.join(","))).expect("long chain");
        assert_eq!(request.order, (0..100).collect::<Vec<usize>>());
        assert_topological(&request);
    }

    #[test]
    fn workflow_names_a_real_cycle() {
        // `after` waits on the cycle but is not part of it.
        let request = workflow_line(
            r#"{"id":"after","command":"true","depends_on":["tests"]},
               {"id":"fmt","command":"true","depends_on":["clippy"]},
               {"id":"tests","command":"true","depends_on":["fmt"]},
               {"id":"clippy","command":"true","depends_on":["tests"]}"#,
        );
        let error = workflow_rejected(&request);
        assert_eq!(error.code, ErrorCode::WorkflowCycle);
        assert_eq!(error.id.as_deref(), Some("w1"));
        assert!(!error.code.is_fatal());
        assert_eq!(error.message, "cycle: tests -> fmt -> clippy -> tests");
        assert!(!error.message.contains("after"), "{}", error.message);

        let error = workflow_rejected(&workflow_line(
            r#"{"id":"a","command":"true","depends_on":["b"]},
               {"id":"b","command":"true","depends_on":["a"]}"#,
        ));
        assert_eq!(error.code, ErrorCode::WorkflowCycle);
        assert_eq!(error.message, "cycle: a -> b -> a");
    }

    #[test]
    fn workflow_rejects_dependency_errors() {
        let self_dependency = workflow_rejected(&workflow_line(
            r#"{"id":"a","command":"true","depends_on":["a"]}"#,
        ));
        assert_eq!(self_dependency.code, ErrorCode::InvalidDependency);
        assert_eq!(self_dependency.id.as_deref(), Some("w1"));
        assert!(!self_dependency.code.is_fatal());
        assert!(
            self_dependency.message.contains(r#"tasks[0].depends_on[0] = "a""#),
            "{}",
            self_dependency.message
        );
        assert!(
            self_dependency.message.contains("cannot depend on itself"),
            "{}",
            self_dependency.message
        );

        let repeated = workflow_rejected(&workflow_line(
            r#"{"id":"a","command":"true"},{"id":"b","command":"true","depends_on":["a","a"]}"#,
        ));
        assert_eq!(repeated.code, ErrorCode::InvalidDependency);
        assert!(
            repeated.message.contains(r#"tasks[1].depends_on[1] = "a""#),
            "{}",
            repeated.message
        );
        assert!(
            repeated.message.contains("repeated"),
            "{}",
            repeated.message
        );

        let missing = workflow_rejected(&workflow_line(
            r#"{"id":"a","command":"true"},{"id":"b","command":"true","depends_on":["ghost"]}"#,
        ));
        assert_eq!(missing.code, ErrorCode::InvalidDependency);
        assert!(
            missing.message.contains(r#"tasks[1].depends_on[0] = "ghost""#),
            "{}",
            missing.message
        );
        assert!(
            missing.message.contains("no task in this request"),
            "{}",
            missing.message
        );

        for task in [
            r#"{"id":"a","command":"true","depends_on":"a"}"#,
            r#"{"id":"a","command":"true","depends_on":7}"#,
            r#"{"id":"a","command":"true","depends_on":[7]}"#,
            r#"{"id":"a","command":"true","depends_on":[null]}"#,
        ] {
            let error = workflow_rejected(&workflow_line(task));
            assert_eq!(error.code, ErrorCode::InvalidDependency, "{task}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{task}");
        }
    }

    #[test]
    fn workflow_rejects_an_empty_or_malformed_task_list() {
        for line in [
            r#"{"schema_version":1,"id":"w1","op":"workflow"}"#.to_string(),
            r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":[]}"#.to_string(),
            r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":{}}"#.to_string(),
            r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":[7]}"#.to_string(),
            r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":[[]]}"#.to_string(),
        ] {
            let error = workflow_rejected(&line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{line}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{line}");
        }
    }

    #[test]
    fn workflow_bounds_the_task_count() {
        let tasks = |count: usize| {
            let body: Vec<String> = (0..count)
                .map(|index| format!(r#"{{"id":"t{index}","command":"true"}}"#))
                .collect();
            workflow_line(&body.join(","))
        };

        assert_eq!(workflow_rejected(&tasks(0)).code, ErrorCode::InvalidRequest);
        assert_eq!(workflow(&tasks(128)).expect("at the limit").tasks.len(), 128);
        let error = workflow_rejected(&tasks(129));
        assert_eq!(error.code, ErrorCode::TooManyTasks);
        assert_eq!(error.id.as_deref(), Some("w1"));
        assert!(!error.code.is_fatal());

        let limits = Limits {
            max_parallel_tasks: 2,
            ..Limits::default()
        };
        let error = workflow_with(&tasks(3), &limits).expect_err("over the lowered limit");
        assert_eq!(error.code, ErrorCode::TooManyTasks);
        assert!(error.message.contains('3') && error.message.contains('2'));
    }

    #[test]
    fn workflow_task_ids_are_validated_and_unique() {
        for task in [
            r#"{"command":"true"}"#,
            r#"{"id":"","command":"true"}"#,
            r#"{"id":7,"command":"true"}"#,
        ] {
            let error = workflow_rejected(&workflow_line(task));
            assert_eq!(error.code, ErrorCode::InvalidId, "{task}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{task}");
        }
        let long = "a".repeat(257);
        let error = workflow_rejected(&workflow_line(&format!(
            r#"{{"id":"{long}","command":"true"}}"#
        )));
        assert_eq!(error.code, ErrorCode::InvalidId);

        let error = workflow_rejected(&workflow_line(
            r#"{"id":"dup","command":"a"},{"id":"dup","command":"b"}"#,
        ));
        assert_eq!(error.code, ErrorCode::DuplicateTaskId);
        assert_eq!(error.id.as_deref(), Some("w1"));
        assert!(!error.code.is_fatal());
        assert!(
            error.message.contains(r#"tasks[1].id "dup" is already used by tasks[0]"#),
            "{}",
            error.message
        );
    }

    #[test]
    fn workflow_decodes_group_vocabularies_and_bounds_the_window() {
        for (policy, expected) in [
            ("wait-all", ParallelPolicy::WaitAll),
            ("fail-fast", ParallelPolicy::FailFast),
        ] {
            let line = format!(
                r#"{{"schema_version":1,"id":"w1","op":"workflow","policy":"{policy}","tasks":[{{"id":"a","command":"true"}}]}}"#
            );
            assert_eq!(workflow(&line).expect("known policy").policy, expected);
        }
        for (failure_policy, expected) in [
            ("skip", FailurePolicy::Skip),
            ("fail", FailurePolicy::Fail),
            ("continue", FailurePolicy::Continue),
        ] {
            let line = format!(
                r#"{{"schema_version":1,"id":"w1","op":"workflow","failure_policy":"{failure_policy}","tasks":[{{"id":"a","command":"true"}}]}}"#
            );
            assert_eq!(
                workflow(&line).expect("known failure policy").failure_policy,
                expected
            );
        }

        for line in [
            r#"{"schema_version":1,"id":"w1","op":"workflow","policy":"wait-all-first","tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","policy":7,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","failure_policy":"ignore","tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","failure_policy":7,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":0,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":33,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":-1,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":"4","tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","timeout_ms":-1,"tasks":[{"id":"a","command":"true"}]}"#,
            r#"{"schema_version":1,"id":"w1","op":"workflow","timeout_ms":"10","tasks":[{"id":"a","command":"true"}]}"#,
        ] {
            let error = workflow_rejected(line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{line}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{line}");
        }

        for window in [1, 4, 32] {
            let line = format!(
                r#"{{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":{window},"tasks":[{{"id":"a","command":"true"}}]}}"#
            );
            assert_eq!(
                workflow(&line).expect("within range").max_concurrency,
                window as usize
            );
        }
        let window_error = workflow_rejected(
            r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":33,"tasks":[{"id":"a","command":"true"}]}"#,
        );
        assert!(
            window_error.message.contains("max_concurrency") && window_error.message.contains("32"),
            "{}",
            window_error.message
        );
        let limits = Limits {
            max_parallel: 2,
            ..Limits::default()
        };
        let line = r#"{"schema_version":1,"id":"w1","op":"workflow","max_concurrency":3,"tasks":[{"id":"a","command":"true"}]}"#;
        assert_eq!(
            workflow_with(line, &limits)
                .expect_err("over the lowered window")
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn workflow_decodes_retry_defaults_and_bounds() {
        let request = workflow(&workflow_line(
            r#"{"id":"tests","command":"true","retry":{}}"#,
        ))
        .expect("an empty retry keeps the defaults");
        assert_eq!(
            request.tasks[0].retry,
            Some(RetryPolicy {
                max_attempts: 1,
                on: vec![RetryOn::Timeout, RetryOn::SpawnError],
                backoff_ms: 0,
                backoff: Backoff::Fixed,
            })
        );

        let request = workflow(&workflow_line(
            r#"{"id":"tests","command":"true","retry":{"max_attempts":2,"on":["failure"],"backoff_ms":500}}"#,
        ))
        .expect("valid retry");
        assert_eq!(
            request.tasks[0].retry,
            Some(RetryPolicy {
                max_attempts: 2,
                on: vec![RetryOn::Failure],
                backoff_ms: 500,
                backoff: Backoff::Fixed,
            })
        );

        for retry in [
            r#"{"max_attempts":0}"#,
            r#"{"max_attempts":9}"#,
            r#"{"max_attempts":"2"}"#,
            r#"{"on":[]}"#,
            r#"{"on":["bogus"]}"#,
            r#"{"on":["failure",7]}"#,
            r#"{"on":"failure"}"#,
            r#"{"backoff_ms":60001}"#,
            r#"{"backoff_ms":-1}"#,
            r#"{"backoff_ms":"0"}"#,
            r#"{"backoff":"linear"}"#,
            r#"{"backoff":7}"#,
        ] {
            let line = workflow_line(&format!(
                r#"{{"id":"tests","command":"true","retry":{retry}}}"#
            ));
            let error = workflow_rejected(&line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{retry}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{retry}");
            assert!(!error.code.is_fatal(), "{retry}");
        }
        let error = workflow_rejected(&workflow_line(
            r#"{"id":"tests","command":"true","retry":"yes"}"#,
        ));
        assert_eq!(error.code, ErrorCode::InvalidRequest);

        // 8 attempts and 60000 ms are exactly at the default limits.
        let at_limit = workflow_line(
            r#"{"id":"tests","command":"true","retry":{"max_attempts":8,"backoff_ms":60000}}"#,
        );
        assert!(workflow(&at_limit).is_ok());

        // The bounds follow `limits`, not hardcoded literals.
        let limits = Limits {
            max_workflow_attempts: 2,
            max_workflow_backoff_ms: 10,
            ..Limits::default()
        };
        let attempts =
            workflow_line(r#"{"id":"tests","command":"true","retry":{"max_attempts":3}}"#);
        assert_eq!(
            workflow_with(&attempts, &limits)
                .expect_err("over the lowered attempt limit")
                .code,
            ErrorCode::InvalidRequest
        );
        let backoff = workflow_line(r#"{"id":"tests","command":"true","retry":{"backoff_ms":11}}"#);
        assert_eq!(
            workflow_with(&backoff, &limits)
                .expect_err("over the lowered backoff limit")
                .code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn workflow_bounds_the_lock_name() {
        let exact = "a".repeat(64);
        let line = workflow_line(&format!(
            r#"{{"id":"tests","command":"true","lock":"{exact}"}}"#
        ));
        assert_eq!(
            workflow(&line).expect("64 bytes").tasks[0].lock,
            Some(exact)
        );

        let too_long = "a".repeat(65);
        for lock in [
            format!(r#""{too_long}""#),
            "\"\"".to_string(),
            r#""a\u0007b""#.to_string(),
            r#""a\u007fb""#.to_string(),
            "7".to_string(),
        ] {
            let line = workflow_line(&format!(
                r#"{{"id":"tests","command":"true","lock":{lock}}}"#
            ));
            let error = workflow_rejected(&line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{lock}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{lock}");
        }
    }

    #[test]
    fn workflow_decodes_conditions() {
        let request = workflow(&workflow_line(
            r#"{"id":"status","command":"true"},
               {"id":"eq","command":"true","depends_on":["status"],
                "condition":{"task":"status","exit_code":{"eq":0}}},
               {"id":"ne","command":"true","depends_on":["status"],
                "condition":{"task":"status","exit_code":{"ne":1}}},
               {"id":"inn","command":"true","depends_on":["status"],
                "condition":{"task":"status","exit_code":{"in":[0,2]}}},
               {"id":"text","command":"true","depends_on":["status"],
                "condition":{"task":"status","stdout_contains":"src/",
                             "stdout_equals":"yes\n","stderr_contains":"warning"}}"#,
        ))
        .expect("valid conditions");

        let condition = |index: usize| request.tasks[index].condition.clone().expect("condition");
        assert_eq!(
            condition(1),
            Condition {
                task: "status".to_string(),
                task_index: 0,
                exit_code: Some(ExitCodeMatcher::Eq(0)),
                stdout_contains: None,
                stdout_equals: None,
                stderr_contains: None,
            }
        );
        assert_eq!(condition(2).exit_code, Some(ExitCodeMatcher::Ne(1)));
        assert_eq!(
            condition(3).exit_code,
            Some(ExitCodeMatcher::In(vec![0, 2]))
        );
        let text = condition(4);
        assert_eq!(text.task_index, 0);
        assert_eq!(text.stdout_contains.as_deref(), Some("src/"));
        assert_eq!(text.stdout_equals.as_deref(), Some("yes\n"));
        assert_eq!(text.stderr_contains.as_deref(), Some("warning"));
    }

    #[test]
    fn workflow_rejects_malformed_conditions() {
        for condition in [
            "{}",
            r#"{"task":"gate"}"#,
            r#"{"task":"gate","exit_code":{}}"#,
            r#"{"task":"gate","exit_code":{"eq":0,"ne":1}}"#,
            r#"{"task":"gate","exit_code":{"eq":0,"in":[0]}}"#,
            r#"{"task":"gate","exit_code":{"gte":0}}"#,
            r#"{"task":"gate","exit_code":{"eq":"0"}}"#,
            r#"{"task":"gate","exit_code":{"ne":null}}"#,
            r#"{"task":"gate","exit_code":{"in":[]}}"#,
            r#"{"task":"gate","exit_code":{"in":[0,1,2,3,4,5,6,7,8]}}"#,
            r#"{"task":"gate","exit_code":{"in":["0"]}}"#,
            r#"{"task":"gate","exit_code":7}"#,
            r#"{"task":"gate","stdout_contains":7}"#,
            r#"{"task":"gate","stdout_equals":7}"#,
            r#"{"task":"gate","stderr_contains":7}"#,
            r#"{"task":"gate","exit_code":{"eq":0},"when":"always"}"#,
            r#"{"task":7,"exit_code":{"eq":0}}"#,
            r#"{"task":"","exit_code":{"eq":0}}"#,
            r#"{"exit_code":{"eq":0}}"#,
            r#""gate""#,
        ] {
            let error = workflow_rejected(&condition_line(condition));
            assert_eq!(error.code, ErrorCode::InvalidCondition, "{condition}");
            assert_eq!(error.id.as_deref(), Some("w1"), "{condition}");
            assert!(!error.code.is_fatal(), "{condition}");
        }

        let not_a_dependency =
            workflow_rejected(&condition_line(r#"{"task":"other","exit_code":{"eq":0}}"#));
        assert_eq!(not_a_dependency.code, ErrorCode::InvalidCondition);
        assert!(
            not_a_dependency.message.contains(r#"tasks[2].condition.task "other""#),
            "{}",
            not_a_dependency.message
        );
        assert!(
            not_a_dependency.message.contains("depends_on"),
            "{}",
            not_a_dependency.message
        );

        let absent = workflow_rejected(&condition_line(r#"{"task":"ghost","exit_code":{"eq":0}}"#));
        assert_eq!(absent.code, ErrorCode::InvalidCondition);
        assert!(
            absent.message.contains(r#"tasks[2].condition.task "ghost""#),
            "{}",
            absent.message
        );
        assert!(
            absent.message.contains("does not name a task"),
            "{}",
            absent.message
        );
    }

    #[test]
    fn workflow_reports_the_most_structural_failure_first() {
        // A cycle outranks a malformed condition: conditions are validated
        // against the edge set, which must exist first.
        let cycle = workflow_line(
            r#"{"id":"a","command":"true","depends_on":["b"],"condition":{}},
               {"id":"b","command":"true","depends_on":["a"]}"#,
        );
        assert_eq!(workflow_rejected(&cycle).code, ErrorCode::WorkflowCycle);

        // Duplicate ids outrank a dependency reference.
        let duplicate = workflow_line(
            r#"{"id":"dup","command":"true"},
               {"id":"dup","command":"true","depends_on":["ghost"]}"#,
        );
        assert_eq!(
            workflow_rejected(&duplicate).code,
            ErrorCode::DuplicateTaskId
        );

        // A task's own scalars outrank the id set.
        let scalars = workflow_line(r#"{"id":"dup","command":"true"},{"id":"dup"}"#);
        assert_eq!(workflow_rejected(&scalars).code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn workflow_decodes_when_executor_is_enabled() {
        const {
            assert!(
                crate::agent::json::WORKFLOW_AVAILABLE,
                "U2 flipped the workflow executor live"
            );
        }
        let decoded = decode_line(
            r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":[{"id":"a","command":"true"}]}"#,
        )
        .expect("workflow is honored in U2");
        assert!(matches!(decoded, DecodedRequest::Workflow(_)));
    }

    #[test]
    fn parallel_task_env_rejects_nul_bytes() {
        for line in [
            r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"a","command":"true","env":{"BAD\u0000NAME":"1"}}]}"#,
            r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"a","command":"true","env":{"OK":"bad\u0000val"}}]}"#,
        ] {
            let error = rejected(line);
            assert_eq!(error.code, ErrorCode::InvalidRequest, "{line}");
            assert_eq!(error.id.as_deref(), Some("p1"), "{line}");
        }
    }

    #[test]
    fn exec_decodes_argv_and_isolated_overrides() {
        let DecodedRequest::Exec(req) = decode_line(
            r#"{"schema_version":1,"id":"x1","op":"exec","isolated":true,"cwd":"/tmp","env":{"A":"1","B":null},"argv":["printf","%s","$RAW"]}"#,
        )
        .expect("valid isolated argv exec") else {
            panic!("expected exec");
        };
        assert!(req.isolated);
        assert_eq!(req.cwd.as_deref(), Some("/tmp"));
        assert_eq!(
            req.env,
            vec![("A".to_string(), Some("1".to_string())), ("B".to_string(), None)]
        );
        assert_eq!(
            req.target.argv(),
            Some(&["printf".to_string(), "%s".to_string(), "$RAW".to_string()][..])
        );

        for bad in [
            r#"{"schema_version":1,"id":"x2","op":"exec","command":"true","argv":["true"]}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","argv":[]}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","argv":[""]}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","argv":["echo","a\u0000b"]}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","command":"true","cwd":"/tmp"}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","command":"true","env":{"A":"1"}}"#,
            r#"{"schema_version":1,"id":"x2","op":"exec","command":"true","isolated":"yes"}"#,
        ] {
            assert_eq!(rejected(bad).code, ErrorCode::InvalidRequest, "{bad}");
        }
    }

    #[test]
    fn output_spec_and_read_capture_are_validated() {
        let DecodedRequest::Exec(req) = decode_line(
            r#"{"schema_version":1,"id":"o1","op":"exec","command":"ls","output":{"strip_ansi":true,"grep":"FAILED","context":2,"head_lines":5,"tail_lines":10,"max_bytes":4096}}"#,
        )
        .expect("valid output spec") else {
            panic!("expected exec");
        };
        assert_eq!(
            req.output,
            Some(OutputSpec {
                strip_ansi: true,
                grep: Some("FAILED".to_string()),
                context: 2,
                head_lines: Some(5),
                tail_lines: Some(10),
                max_bytes: Some(4096),
                parse: None,
            })
        );

        let DecodedRequest::ReadCapture(rc) = decode_line(
            r#"{"schema_version":1,"id":"rc1","op":"read_capture","capture_id":"cap-7","stream":"stderr","line_start":3,"line_count":20,"grep":"err","context":1,"strip_ansi":true,"max_bytes":1024}"#,
        )
        .expect("valid read_capture") else {
            panic!("expected read_capture");
        };
        assert_eq!(rc.capture_id, "cap-7");
        assert_eq!(rc.stream, CaptureStream::Stderr);
        assert_eq!(rc.line_start, Some(3));
        assert_eq!(rc.line_count, Some(20));
        assert_eq!(rc.grep.as_deref(), Some("err"));
        assert_eq!(rc.context, 1);
        assert!(rc.strip_ansi);
        assert_eq!(rc.max_bytes, Some(1024));

        for bad in [
            r#"{"schema_version":1,"id":"b","op":"exec","command":"ls","output":{"grep":"["}}"#,
            r#"{"schema_version":1,"id":"b","op":"exec","command":"ls","output":{"grep":""}}"#,
            r#"{"schema_version":1,"id":"b","op":"exec","command":"ls","output":{"context":1}}"#,
            r#"{"schema_version":1,"id":"b","op":"exec","command":"ls","output":{"max_bytes":0}}"#,
            r#"{"schema_version":1,"id":"b","op":"read_capture"}"#,
            r#"{"schema_version":1,"id":"b","op":"read_capture","capture_id":"cap-1","stream":"stdin"}"#,
            r#"{"schema_version":1,"id":"b","op":"read_capture","capture_id":"cap-1","byte_offset":0,"line_start":1}"#,
            r#"{"schema_version":1,"id":"b","op":"read_capture","capture_id":"cap-1","line_start":0}"#,
            r#"{"schema_version":1,"id":"b","op":"read_capture","capture_id":"cap-1","context":1}"#,
        ] {
            assert_eq!(rejected(bad).code, ErrorCode::InvalidRequest, "{bad}");
        }
    }

    #[test]
    fn code_decodes_and_enforces_bounds() {
        let DecodedRequest::Code(req) = decode_line(
            r#"{"schema_version":1,"id":"cd1","op":"code","code":"run([\"true\"])","args":{"n":2},"timeout_ms":500,"max_steps":1000,"max_calls":10}"#,
        )
        .expect("valid code request") else {
            panic!("expected code");
        };
        assert_eq!(req.source, r#"run(["true"])"#);
        assert_eq!(req.args.get("n"), Some(&Value::Int(2)));
        assert_eq!(req.timeout_ms, Some(500));
        assert_eq!(req.max_steps, 1000);
        assert_eq!(req.max_calls, 10);

        for bad in [
            r#"{"schema_version":1,"id":"c","op":"code"}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"   "}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"1","max_steps":0}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"1","max_steps":100001}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"1","max_calls":0}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"1","max_calls":257}"#,
            r#"{"schema_version":1,"id":"c","op":"code","code":"1","timeout_ms":-1}"#,
        ] {
            assert_eq!(rejected(bad).code, ErrorCode::InvalidRequest, "{bad}");
        }
    }
}
