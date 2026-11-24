//! Optional native Model Context Protocol (MCP) server over stdio.
//!
//! Activated exclusively via `sharky agent --mcp [--no-config] [--redact-env] [--state-file PATH]`.
//! Speaks JSON-RPC 2.0 supporting both line-delimited JSON (NDJSON) and
//! `Content-Length` framed stdio transport, keeping shell stdio isolated on `/dev/null`.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

use nix::fcntl::OFlag;
use nix::libc;

use super::capture;
use super::code::{self, CodeHost};
use super::jobs::WaitUntilSpec;
use super::json::{self, Response, Value};
use super::protocol;
use super::supervisor;
use super::{Session, SessionOptions, VERSION, close_fd, duplicate_high, open_null, redirect_stdio_to_null};

const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Framing {
    Ndjson,
    ContentLength,
}

struct McpReader {
    fd: RawFd,
    buf: Vec<u8>,
    eof: bool,
}

impl McpReader {
    fn new(fd: RawFd) -> Self {
        Self {
            fd,
            buf: Vec::with_capacity(8192),
            eof: false,
        }
    }

    fn fill(&mut self) -> io::Result<usize> {
        if self.eof {
            return Ok(0);
        }
        let mut chunk = [0u8; 8192];
        loop {
            // SAFETY: `self.fd` is an open control descriptor owned by MCP server.
            let n = unsafe { libc::read(self.fd, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n > 0 {
                self.buf.extend_from_slice(&chunk[..n as usize]);
                return Ok(n as usize);
            }
            if n == 0 {
                self.eof = true;
                return Ok(0);
            }
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
    }

    fn skip_leading_ws(&mut self) {
        let idx = self
            .buf
            .iter()
            .position(|b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            .unwrap_or(self.buf.len());
        if idx > 0 {
            self.buf.drain(..idx);
        }
    }

    fn ensure_bytes(&mut self, needed: usize) -> io::Result<bool> {
        while self.buf.len() < needed {
            if self.fill()? == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn next_frame(&mut self) -> io::Result<Option<(Vec<u8>, Framing)>> {
        loop {
            self.skip_leading_ws();
            if self.buf.is_empty() {
                if self.fill()? == 0 {
                    return Ok(None);
                }
                continue;
            }
            break;
        }

        if matches!(self.buf[0], b'{' | b'[') {
            loop {
                if let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
                    while matches!(line.last(), Some(b'\n' | b'\r')) {
                        line.pop();
                    }
                    return Ok(Some((line, Framing::Ndjson)));
                }
                if self.fill()? == 0 {
                    let mut line = std::mem::take(&mut self.buf);
                    while matches!(line.last(), Some(b'\n' | b'\r')) {
                        line.pop();
                    }
                    return if line.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some((line, Framing::Ndjson)))
                    };
                }
            }
        }

        // Content-Length framing: headers terminated by `\r\n\r\n` or `\n\n`.
        let (header_end, sep_len) = loop {
            if let Some(pos) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break (pos, 4);
            }
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\n\n") {
                break (pos, 2);
            }
            if self.fill()? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete MCP Content-Length header",
                ));
            }
        };

        let header_bytes: Vec<u8> = self.buf.drain(..header_end + sep_len).collect();
        let header_str = std::str::from_utf8(&header_bytes[..header_end])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 MCP header"))?;
        let mut content_length: Option<usize> = None;
        for line in header_str.lines() {
            if let Some((name, val)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                let parsed = val.trim().parse::<usize>().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "invalid Content-Length value")
                })?;
                content_length = Some(parsed);
            }
        }
        let len = content_length.ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
        })?;
        if !self.ensure_bytes(len)? {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "truncated MCP Content-Length body",
            ));
        }
        let body: Vec<u8> = self.buf.drain(..len).collect();
        Ok(Some((body, Framing::ContentLength)))
    }
}

struct McpServer {
    base_options: SessionOptions,
    null_fd: RawFd,
    default_session: Session,
    sessions: BTreeMap<String, Session>,
    next_id: usize,
}

impl McpServer {
    fn alloc_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    fn dispatch(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = match msg.get("method").and_then(Value::as_str) {
            Some(method) => method,
            None => {
                return id.map(|req_id| {
                    rpc_error(Some(req_id), -32600, "Invalid Request: missing method")
                });
            }
        };

        // Notifications have no `id` and expect no JSON-RPC response.
        let req_id = id?;

        let params = msg.get("params");
        let response = match method {
            "initialize" => {
                let proto = params
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                let result = Value::Object(vec![
                    ks("protocolVersion", proto),
                    kv(
                        "capabilities",
                        Value::Object(vec![kv(
                            "tools",
                            Value::Object(vec![(String::from("listChanged"), Value::Bool(false))]),
                        )]),
                    ),
                    kv(
                        "serverInfo",
                        Value::Object(vec![ks("name", "sharky"), ks("version", VERSION)]),
                    ),
                ]);
                rpc_result(req_id, result)
            }
            "ping" => rpc_result(req_id, Value::Object(Vec::new())),
            "tools/list" => rpc_result(req_id, Value::Object(vec![kv("tools", tools_schema())])),
            "tools/call" => self.handle_tool_call(req_id, params),
            other => rpc_error(Some(req_id), -32601, &format!("Method not found: {other}")),
        };
        Some(response)
    }

    fn handle_tool_call(&mut self, req_id: Value, params: Option<&Value>) -> Value {
        let Some(params_obj) = params.filter(|v| v.as_object().is_some()) else {
            return rpc_error(Some(req_id), -32602, "tools/call requires object params");
        };
        let Some(name) = params_obj.get("name").and_then(Value::as_str) else {
            return rpc_error(Some(req_id), -32602, "tools/call requires string tool name");
        };
        let args = match params_obj.get("arguments") {
            None | Some(Value::Null) => Value::Object(Vec::new()),
            Some(Value::String(raw)) => {
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    Value::Object(Vec::new())
                } else {
                    match json::parse_line(trimmed.as_bytes()) {
                        Ok(obj @ Value::Object(_)) => obj,
                        _ => {
                            return rpc_error(
                                Some(req_id),
                                -32602,
                                "tool arguments must be a JSON object (got string; pass an object like {\"command\": \"...\"} or {\"code\": \"...\"})",
                            );
                        }
                    }
                }
            }
            Some(other) => other.clone(),
        };
        if args.as_object().is_none() {
            return rpc_error(
                Some(req_id),
                -32602,
                "tool arguments must be a JSON object",
            );
        }

        let (payload, is_error) = match name {
            "sharky_capabilities" => (crate::cli::capabilities_value(), false),
            "sharky_exec" => self.tool_exec(&args, true),
            "sharky_session_open" => self.tool_session_open(&args),
            "sharky_session_exec" => self.tool_exec(&args, false),
            "sharky_parallel" => self.tool_parallel(&args),
            "sharky_workflow" => self.tool_workflow(&args),
            "sharky_code" => self.tool_code(&args),
            "sharky_read_capture" => self.tool_read_capture(&args),
            "sharky_cancel" => self.tool_cancel(&args),
            "sharky_session_close" => self.tool_session_close(&args),
            "sharky_snapshot" => self.tool_snapshot(&args),
            "sharky_restore" => self.tool_restore(&args),
            "sharky_job_start" => self.tool_job_start(&args),
            "sharky_job_poll" => self.tool_job_poll(&args),
            "sharky_job_send" => self.tool_job_send(&args),
            "sharky_job_stop" => self.tool_job_stop(&args),
            other => {
                return rpc_error(Some(req_id), -32602, &format!("Unknown tool: {other}"));
            }
        };

        let result = Value::Object(vec![
            kv(
                "content",
                Value::Array(vec![Value::Object(vec![
                    ks("type", "text"),
                    ks("text", &payload.encode()),
                ])]),
            ),
            kv("structuredContent", payload),
            (String::from("isError"), Value::Bool(is_error)),
        ]);
        rpc_result(req_id, result)
    }

    fn session_mut(&mut self, args: &Value) -> Result<Option<String>, (Value, bool)> {
        match args.get("session_id") {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(id)) if id == "default" => Ok(None),
            Some(Value::String(id)) if !id.is_empty() => {
                if self.sessions.contains_key(id) {
                    Ok(Some(id.clone()))
                } else {
                    Err((tool_err("UNKNOWN_SESSION", &format!("unknown session_id: {id}")), true))
                }
            }
            Some(_) => Err((tool_err("INVALID_REQUEST", "session_id must be a non-empty string"), true)),
        }
    }

    fn target_session_mut(&mut self, key: Option<&str>) -> &mut Session {
        match key {
            Some(id) => self.sessions.get_mut(id).expect("checked session_id"),
            None => &mut self.default_session,
        }
    }

    fn tool_exec(&mut self, args: &Value, default_only: bool) -> (Value, bool) {
        let session_key = if default_only {
            None
        } else {
            match self.session_mut(args) {
                Ok(key) => key,
                Err(err) => return err,
            }
        };

        let req_id = self.alloc_id("mcp-exec");
        let limits = self.target_session_mut(session_key.as_deref()).limits();
        let mut decoded = match protocol::decode_exec(args, &limits, req_id) {
            Ok(req) => req,
            Err(err) => return (tool_err(err.code.as_str(), &err.message), true),
        };
        let result = {
            let session = self.target_session_mut(session_key.as_deref());
            if let Err((code, msg)) =
                session.enforce_target_policy(&decoded.target, &mut decoded.sandbox)
            {
                return (tool_err(code.as_str(), &msg), true);
            }
            let out = session.call_exec(&decoded);
            if let Some(_exit_code) = session.shell.exit_requested.take() {
                if let Some(ref sid) = session_key {
                    if let Some(mut closing) = self.sessions.remove(sid) {
                        let _ = closing.end_headless(Some(_exit_code));
                    }
                }
            }
            out
        };
        match result {
            Ok(res) => decode_encoded(Response::Result(res).encode()),
            Err(err) => (tool_err("INTERNAL", &err), true),
        }
    }

    fn tool_session_open(&mut self, args: &Value) -> (Value, bool) {
        let sid = match args.get("session_id") {
            None | Some(Value::Null) => self.alloc_id("mcp-s"),
            Some(Value::String(id)) if !id.is_empty() && id != "default" => id.clone(),
            Some(_) => {
                return (
                    tool_err(
                        "INVALID_REQUEST",
                        "session_id must be a non-empty string other than 'default'",
                    ),
                    true,
                );
            }
        };
        if self.sessions.contains_key(&sid) {
            return (
                tool_err("DUPLICATE_SESSION", &format!("session already exists: {sid}")),
                true,
            );
        }

        let mut opts = self.base_options.clone();
        opts.state_file = None;
        if let Some(no_cfg) = args.get("no_config") {
            let Some(flag) = no_cfg.as_bool() else {
                return (tool_err("INVALID_REQUEST", "no_config must be a boolean"), true);
            };
            opts.no_config = flag;
        }
        if let Some(redact) = args.get("redact_env") {
            let Some(flag) = redact.as_bool() else {
                return (tool_err("INVALID_REQUEST", "redact_env must be a boolean"), true);
            };
            opts.redact_env = flag;
        }
        if let Some(path) = args.get("state_file") {
            let Some(p) = path.as_str() else {
                return (tool_err("INVALID_REQUEST", "state_file must be a string"), true);
            };
            if p.is_empty() {
                return (tool_err("INVALID_REQUEST", "state_file must not be empty"), true);
            }
            opts.state_file = Some(std::path::PathBuf::from(p));
        }

        let mut session = match Session::new_headless(&opts, self.null_fd) {
            Ok(session) => session,
            Err(err) => return (tool_err("INTERNAL", &err), true),
        };
        if let Some(cwd) = args.get("cwd") {
            let Some(dir) = cwd.as_str() else {
                return (tool_err("INVALID_REQUEST", "cwd must be a string"), true);
            };
            if let Err(err) = session.shell.enter_task_directory(dir) {
                return (
                    tool_err("INVALID_REQUEST", &format!("cannot change directory to {dir}: {err}")),
                    true,
                );
            }
        }
        if let Some(env) = args.get("env") {
            let Some(entries) = env.as_object() else {
                return (tool_err("INVALID_REQUEST", "env must be an object"), true);
            };
            for (k, v) in entries {
                match v {
                    Value::String(s) => session.shell.set_task_env(k, Some(s.clone())),
                    Value::Null => session.shell.set_task_env(k, None),
                    _ => {
                        return (
                            tool_err("INVALID_REQUEST", "env values must be strings or null"),
                            true,
                        );
                    }
                }
            }
        }

        let cwd = session.shell.cwd.to_string_lossy().into_owned();
        self.sessions.insert(sid.clone(), session);
        (
            Value::Object(vec![
                ks("session_id", &sid),
                ks("cwd", &cwd),
                (String::from("pid"), Value::Int(i64::from(std::process::id()))),
            ]),
            false,
        )
    }

    fn tool_parallel(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let req_id = self.alloc_id("mcp-par");
        let limits = self.target_session_mut(session_key.as_deref()).limits();
        let mut decoded = match protocol::decode_parallel(args, &limits, req_id) {
            Ok(req) => req,
            Err(err) => return (tool_err(err.code.as_str(), &err.message), true),
        };
        let session = self.target_session_mut(session_key.as_deref());
        for task in &mut decoded.tasks {
            if let Err((code, msg)) =
                session.enforce_target_policy(&task.target, &mut task.sandbox)
            {
                return (tool_err(code.as_str(), &msg), true);
            }
        }
        match session.call_parallel(&decoded) {
            Ok(res) => decode_encoded(Response::Parallel(res).encode()),
            Err(err) => (tool_err("INTERNAL", &err), true),
        }
    }

    fn tool_workflow(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let req_id = self.alloc_id("mcp-wf");
        let limits = self.target_session_mut(session_key.as_deref()).limits();
        let mut decoded = match protocol::decode_workflow(args, &limits, req_id) {
            Ok(req) => req,
            Err(err) => return (tool_err(err.code.as_str(), &err.message), true),
        };
        let session = self.target_session_mut(session_key.as_deref());
        for task in &mut decoded.tasks {
            if let Err((code, msg)) =
                session.enforce_target_policy(&task.base.target, &mut task.base.sandbox)
            {
                return (tool_err(code.as_str(), &msg), true);
            }
        }
        match session.call_workflow(&decoded) {
            Ok(res) => decode_encoded(Response::Workflow(res).encode()),
            Err(err) => (tool_err("INTERNAL", &err), true),
        }
    }

    fn tool_code(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let normalized = if args.get("code").is_none()
            && let Some(source) = args.get("source")
            && let Some(entries) = args.as_object()
        {
            let mut next = entries.to_vec();
            next.push((String::from("code"), source.clone()));
            Value::Object(next)
        } else {
            args.clone()
        };
        let req_id = self.alloc_id("mcp-code");
        let limits = self.target_session_mut(session_key.as_deref()).limits();
        let decoded = match protocol::decode_code(&normalized, &limits, req_id) {
            Ok(req) => req,
            Err(err) => return (tool_err(err.code.as_str(), &err.message), true),
        };
        let session = self.target_session_mut(session_key.as_deref());
        let res = code::execute(session, &decoded);
        let _ = session.shell.exit_requested.take();
        decode_encoded(Response::Code(res).encode())
    }

    fn tool_read_capture(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let req_id = self.alloc_id("mcp-cap");
        let limits = self.target_session_mut(session_key.as_deref()).limits();
        let decoded = match protocol::decode_read_capture(args, &limits, req_id) {
            Ok(req) => req,
            Err(err) => return (tool_err(err.code.as_str(), &err.message), true),
        };
        match self
            .target_session_mut(session_key.as_deref())
            .call_read_capture(&decoded)
        {
            Ok(res) => decode_encoded(Response::Capture(res).encode()),
            Err((code, msg)) => (tool_err(code.as_str(), &msg), true),
        }
    }

    fn tool_cancel(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let target_pgid = match args.get("pgid") {
            None | Some(Value::Null) => None,
            Some(Value::Int(pgid)) if *pgid > 0 => Some(*pgid as i32),
            Some(_) => {
                return (
                    tool_err("INVALID_REQUEST", "pgid must be a positive integer"),
                    true,
                );
            }
        };
        let session = self.target_session_mut(session_key.as_deref());
        let jobs = session.shell.snapshot_jobs();
        let mut signaled = Vec::new();
        for (_id, pgid, _pids, _cmd, _stopped) in jobs {
            if target_pgid.is_none_or(|wanted| wanted == pgid) && pgid > 0 {
                // SAFETY: `pgid` is an active job process group owned by this session.
                let rc = unsafe { libc::killpg(pgid, libc::SIGTERM) };
                if rc == 0 {
                    signaled.push(Value::Int(i64::from(pgid)));
                }
            }
        }
        session.shell.reap_jobs();
        (
            Value::Object(vec![
                (String::from("cancelled"), Value::Bool(!signaled.is_empty())),
                kv("signaled_pgids", Value::Array(signaled)),
            ]),
            false,
        )
    }

    fn tool_session_close(&mut self, args: &Value) -> (Value, bool) {
        let Some(sid) = args.get("session_id").and_then(Value::as_str) else {
            return (
                tool_err("INVALID_REQUEST", "sharky_session_close requires string session_id"),
                true,
            );
        };
        if sid == "default" {
            return (
                tool_err("INVALID_REQUEST", "default session cannot be closed explicitly"),
                true,
            );
        }
        let Some(mut session) = self.sessions.remove(sid) else {
            return (
                tool_err("UNKNOWN_SESSION", &format!("unknown session_id: {sid}")),
                true,
            );
        };
        let status = session.end_headless(Some(0));
        (
            Value::Object(vec![
                ks("session_id", sid),
                (String::from("closed"), Value::Bool(true)),
                (String::from("status"), Value::Int(i64::from(status))),
            ]),
            false,
        )
    }

    fn tool_snapshot(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let custom_id = match args.get("snapshot_id") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => Some(s.as_str()),
            Some(_) => {
                return (
                    tool_err("INVALID_REQUEST", "snapshot_id must be a non-empty string"),
                    true,
                );
            }
        };
        let session = self.target_session_mut(session_key.as_deref());
        let (snapshot_id, cwd) = session.create_snapshot(custom_id);
        (
            Value::Object(vec![ks("snapshot_id", &snapshot_id), ks("cwd", &cwd)]),
            false,
        )
    }

    fn tool_restore(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let Some(snapshot_id) = args.get("snapshot_id").and_then(Value::as_str) else {
            return (
                tool_err("INVALID_REQUEST", "snapshot_id is required"),
                true,
            );
        };
        let session = self.target_session_mut(session_key.as_deref());
        match session.restore_snapshot(snapshot_id) {
            Ok(cwd) => (
                Value::Object(vec![
                    (String::from("restored"), Value::Bool(true)),
                    ks("snapshot_id", snapshot_id),
                    ks("cwd", &cwd),
                ]),
                false,
            ),
            Err(msg) => (tool_err("UNKNOWN_SNAPSHOT", &msg), true),
        }
    }

    fn tool_job_start(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let target = match (args.get("command"), args.get("argv")) {
            (Some(_), Some(_)) => {
                return (
                    tool_err("INVALID_REQUEST", "command and argv are mutually exclusive"),
                    true,
                );
            }
            (Some(Value::String(cmd)), None) => protocol::ExecTarget::Command(cmd.clone()),
            (None, Some(Value::Array(items))) if !items.is_empty() => {
                let mut argv = Vec::with_capacity(items.len());
                for item in items {
                    let Some(s) = item.as_str() else {
                        return (
                            tool_err("INVALID_REQUEST", "argv items must be strings"),
                            true,
                        );
                    };
                    argv.push(s.to_string());
                }
                protocol::ExecTarget::Argv(argv)
            }
            _ => {
                return (
                    tool_err("INVALID_REQUEST", "command or non-empty argv is required"),
                    true,
                );
            }
        };
        let cwd = match args.get("cwd") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return (tool_err("INVALID_REQUEST", "cwd must be a string"), true),
        };
        let mut env_overrides = Vec::new();
        if let Some(env_val) = args.get("env") {
            let Some(entries) = env_val.as_object() else {
                return (tool_err("INVALID_REQUEST", "env must be an object"), true);
            };
            for (k, v) in entries {
                match v {
                    Value::String(s) => env_overrides.push((k.clone(), Some(s.clone()))),
                    Value::Null => env_overrides.push((k.clone(), None)),
                    _ => {
                        return (
                            tool_err("INVALID_REQUEST", "env values must be strings or null"),
                            true,
                        );
                    }
                }
            }
        }
        let wait_until = match args.get("wait_until") {
            None | Some(Value::Null) => None,
            Some(val) => {
                if val.as_object().is_none() {
                    return (
                        tool_err("INVALID_REQUEST", "wait_until must be an object"),
                        true,
                    );
                }
                let regex = match val.get("regex") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(_) => {
                        return (
                            tool_err("INVALID_REQUEST", "wait_until.regex must be a string"),
                            true,
                        );
                    }
                };
                let port = match val.get("port") {
                    None | Some(Value::Null) => None,
                    Some(Value::Int(p)) if (1..=65535).contains(p) => Some(*p as u16),
                    Some(_) => {
                        return (
                            tool_err(
                                "INVALID_REQUEST",
                                "wait_until.port must be an integer between 1 and 65535",
                            ),
                            true,
                        );
                    }
                };
                let timeout_ms = match val.get("timeout_ms") {
                    None | Some(Value::Null) => 5_000,
                    Some(Value::Int(ms)) if *ms >= 0 => *ms as u64,
                    Some(_) => {
                        return (
                            tool_err(
                                "INVALID_REQUEST",
                                "wait_until.timeout_ms must be a non-negative integer",
                            ),
                            true,
                        );
                    }
                };
                if regex.is_none() && port.is_none() {
                    return (
                        tool_err("INVALID_REQUEST", "wait_until requires regex or port"),
                        true,
                    );
                }
                Some(WaitUntilSpec {
                    regex,
                    port,
                    timeout_ms,
                })
            }
        };
        let session = self.target_session_mut(session_key.as_deref());
        let mut dummy_sandbox = None;
        if let Err((code, msg)) = session.enforce_target_policy(&target, &mut dummy_sandbox) {
            return (tool_err(code.as_str(), &msg), true);
        }
        match session.jobs.start(
            &mut session.shell,
            &target,
            cwd,
            &env_overrides,
            wait_until.as_ref(),
        ) {
            Ok(val) => (val, false),
            Err(msg) => (tool_err("JOB_START_FAILED", &msg), true),
        }
    }

    fn tool_job_poll(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let Some(job_id) = args.get("job_id").and_then(Value::as_str) else {
            return (tool_err("INVALID_REQUEST", "job_id is required"), true);
        };
        let stdout_cursor = match args.get("stdout_cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            Some(_) => {
                return (
                    tool_err("INVALID_REQUEST", "stdout_cursor must be non-negative"),
                    true,
                );
            }
        };
        let stderr_cursor = match args.get("stderr_cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::Int(n)) if *n >= 0 => *n as usize,
            Some(_) => {
                return (
                    tool_err("INVALID_REQUEST", "stderr_cursor must be non-negative"),
                    true,
                );
            }
        };
        let wait_ms = match args.get("wait_ms") {
            None | Some(Value::Null) => 0,
            Some(Value::Int(n)) if *n >= 0 => *n as u64,
            Some(_) => {
                return (
                    tool_err("INVALID_REQUEST", "wait_ms must be non-negative"),
                    true,
                );
            }
        };
        let session = self.target_session_mut(session_key.as_deref());
        match session.jobs.poll(job_id, stdout_cursor, stderr_cursor, wait_ms) {
            Ok(val) => (val, false),
            Err(msg) => (tool_err("UNKNOWN_JOB", &msg), true),
        }
    }

    fn tool_job_send(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let Some(job_id) = args.get("job_id").and_then(Value::as_str) else {
            return (tool_err("INVALID_REQUEST", "job_id is required"), true);
        };
        let data = match args.get("data") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return (tool_err("INVALID_REQUEST", "data must be a string"), true),
        };
        let close_stdin = args
            .get("close_stdin")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let session = self.target_session_mut(session_key.as_deref());
        match session.jobs.send(job_id, data, close_stdin) {
            Ok(val) => (val, false),
            Err(msg) => (tool_err("JOB_SEND_FAILED", &msg), true),
        }
    }

    fn tool_job_stop(&mut self, args: &Value) -> (Value, bool) {
        let session_key = match self.session_mut(args) {
            Ok(key) => key,
            Err(err) => return err,
        };
        let Some(job_id) = args.get("job_id").and_then(Value::as_str) else {
            return (tool_err("INVALID_REQUEST", "job_id is required"), true);
        };
        let signal = match args.get("signal") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.as_str()),
            Some(_) => return (tool_err("INVALID_REQUEST", "signal must be a string"), true),
        };
        let session = self.target_session_mut(session_key.as_deref());
        match session.jobs.stop(job_id, signal) {
            Ok(val) => (val, false),
            Err(msg) => (tool_err("UNKNOWN_JOB", &msg), true),
        }
    }
}

pub(crate) fn serve(options: SessionOptions) -> i32 {
    if crate::term::is_tty(0) {
        eprintln!("sharky: agent: standard input is a terminal; --mcp requires a pipe");
        return 2;
    }

    let input = match duplicate_high(0) {
        Ok(fd) => fd,
        Err(err) => {
            eprintln!("sharky: agent: cannot move MCP input: {err}");
            return 1;
        }
    };
    let output = match duplicate_high(1) {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(input);
            eprintln!("sharky: agent: cannot move MCP output: {err}");
            return 1;
        }
    };
    let null_fd = match open_null() {
        Ok(fd) => fd,
        Err(err) => {
            close_fd(input);
            close_fd(output);
            eprintln!("sharky: agent: /dev/null: {err}");
            return 1;
        }
    };
    let (dummy_read, dummy_write) = match nix::unistd::pipe2(OFlag::O_CLOEXEC) {
        Ok(pair) => pair,
        Err(err) => {
            close_fd(null_fd);
            close_fd(input);
            close_fd(output);
            eprintln!("sharky: agent: pipe: {err}");
            return 1;
        }
    };
    if let Err(err) = redirect_stdio_to_null(null_fd) {
        close_fd(null_fd);
        close_fd(input);
        close_fd(output);
        eprintln!("sharky: agent: cannot isolate stdio: {err}");
        return 1;
    }
    if let Err(err) = supervisor::install(dummy_read.as_raw_fd(), -1, options.limits) {
        close_fd(null_fd);
        close_fd(input);
        close_fd(output);
        eprintln!("sharky: agent: cannot install session supervisor: {err}");
        return 1;
    }
    supervisor::close_in_children(&[dummy_write.as_raw_fd(), input, output]);
    crate::signal::watch_children();

    let default_session = match Session::new_headless(&options, null_fd) {
        Ok(session) => session,
        Err(err) => {
            close_fd(null_fd);
            close_fd(input);
            close_fd(output);
            eprintln!("sharky: agent: {err}");
            return 1;
        }
    };

    let mut server = McpServer {
        base_options: options,
        null_fd,
        default_session,
        sessions: BTreeMap::new(),
        next_id: 0,
    };

    let mut reader = McpReader::new(input);
    let mut exit_code = 0;

    loop {
        if let Some(sig) = crate::signal::fatal_signal() {
            exit_code = 128 + sig;
            break;
        }
        match reader.next_frame() {
            Ok(Some((bytes, framing))) => {
                let response = match json::parse_line(&bytes) {
                    Ok(value) => server.dispatch(&value),
                    Err(err) => Some(rpc_error(None, -32700, &format!("Parse error: {err}"))),
                };
                if let Some(resp) = response {
                    if write_mcp_frame(output, &resp, framing).is_err() {
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                let err_resp = rpc_error(None, -32700, &format!("Framing error: {err}"));
                let _ = write_mcp_frame(output, &err_resp, Framing::Ndjson);
                exit_code = 1;
                break;
            }
        }
    }

    for (_id, mut session) in std::mem::take(&mut server.sessions) {
        let _ = session.end_headless(None);
    }
    let _ = server.default_session.end_headless(None);
    drop(dummy_read);
    drop(dummy_write);
    close_fd(input);
    close_fd(output);
    close_fd(null_fd);
    exit_code
}

fn write_mcp_frame(fd: RawFd, value: &Value, framing: Framing) -> io::Result<()> {
    let mut body = value.encode();
    match framing {
        Framing::Ndjson => {
            body.push('\n');
            capture::write_safely(fd, body.as_bytes())
        }
        Framing::ContentLength => {
            let framed = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
            capture::write_safely(fd, framed.as_bytes())
        }
    }
}

fn decode_encoded(encoded: String) -> (Value, bool) {
    match json::parse_line(encoded.as_bytes()) {
        Ok(value) => (value, false),
        Err(err) => (tool_err("INTERNAL", &err.to_string()), true),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    Value::Object(vec![
        ks("jsonrpc", "2.0"),
        (String::from("id"), id),
        kv("result", result),
    ])
}

fn rpc_error(id: Option<Value>, code: i64, message: &str) -> Value {
    let mut fields = vec![ks("jsonrpc", "2.0")];
    if let Some(id_val) = id {
        fields.push((String::from("id"), id_val));
    } else {
        fields.push((String::from("id"), Value::Null));
    }
    fields.push(kv(
        "error",
        Value::Object(vec![
            (String::from("code"), Value::Int(code)),
            ks("message", message),
        ]),
    ));
    Value::Object(fields)
}

fn tool_err(code: &str, message: &str) -> Value {
    Value::Object(vec![ks("code", code), ks("message", message)])
}

fn kv(key: &str, value: Value) -> (String, Value) {
    (String::from(key), value)
}

fn ks(key: &str, value: &str) -> (String, Value) {
    (String::from(key), Value::String(String::from(value)))
}

fn prop_str(desc: &str) -> Value {
    Value::Object(vec![ks("type", "string"), ks("description", desc)])
}

fn prop_int(desc: &str) -> Value {
    Value::Object(vec![ks("type", "integer"), ks("description", desc)])
}

fn prop_bool(desc: &str) -> Value {
    Value::Object(vec![ks("type", "boolean"), ks("description", desc)])
}

fn tool_def(
    name: &str,
    description: &str,
    properties: Vec<(&str, Value)>,
    required: &[&str],
) -> Value {
    let props = Value::Object(
        properties
            .into_iter()
            .map(|(k, v)| (String::from(k), v))
            .collect(),
    );
    let mut schema = vec![ks("type", "object"), kv("properties", props)];
    if !required.is_empty() {
        schema.push(kv(
            "required",
            Value::Array(
                required
                    .iter()
                    .map(|r| Value::String(String::from(*r)))
                    .collect(),
            ),
        ));
    }
    Value::Object(vec![
        ks("name", name),
        ks("description", description),
        kv("inputSchema", Value::Object(schema)),
    ])
}

fn tools_schema() -> Value {
    let exec_props = vec![
        ("command", prop_str("Shell command string to evaluate")),
        (
            "argv",
            Value::Object(vec![
                ks("type", "array"),
                kv("items", prop_str("Argument")),
                ks("description", "Direct argv execution bypassing shell parsing"),
            ]),
        ),
        ("stdin", prop_str("Optional stdin payload")),
        ("stdin_encoding", prop_str("utf8 or base64")),
        ("timeout_ms", prop_int("Execution timeout in milliseconds")),
        ("env_delta", prop_bool("Include changed exported env variables")),
        ("isolated", prop_bool("Run in a copy-on-write child")),
        ("cwd", prop_str("Working directory (requires isolated: true)")),
        (
            "env",
            Value::Object(vec![
                ks("type", "object"),
                ks(
                    "description",
                    "Per-call environment overrides, null unsets (requires isolated: true)",
                ),
            ]),
        ),
        (
            "output",
            Value::Object(vec![
                ks("type", "object"),
                ks(
                    "description",
                    "Output projection ({strip_ansi, grep, context, head_lines, tail_lines, max_bytes, parse})",
                ),
            ]),
        ),
        ("tty", prop_bool("Allocate a pseudo-terminal on stdout/stdin")),
        ("lock", prop_str("Cross-session advisory OS lock name")),
        ("fs_delta", prop_bool("Include created/modified/removed files under cwd")),
        (
            "sandbox",
            Value::Object(vec![ks(
                "description",
                "Boolean or object ({no_network, read_only_root, max_file_bytes, max_procs, cpu_seconds})",
            )]),
        ),
    ];

    let mut session_exec_props = vec![(
        "session_id",
        prop_str("Named session from sharky_session_open, or omit for default session"),
    )];
    session_exec_props.extend(exec_props.clone());

    let tools = vec![
        tool_def(
            "sharky_capabilities",
            "Discover Sharky shell version, transports, protocol ops, features, Code Mode builtins, and isolation guarantees.",
            vec![],
            &[],
        ),
        tool_def(
            "sharky_exec",
            "Execute a shell command or argv in the default session with structured stdout/stderr, status, duration, and capture_id.",
            exec_props,
            &[],
        ),
        tool_def(
            "sharky_session_open",
            "Open a new persistent isolated shell session with optional cwd, env overrides, and state_file.",
            vec![
                ("session_id", prop_str("Optional custom session identifier")),
                ("cwd", prop_str("Initial working directory")),
                ("env", Value::Object(vec![ks("type", "object")])),
                ("no_config", prop_bool("Skip user config loading")),
                ("redact_env", prop_bool("Redact sensitive variables in responses")),
                ("state_file", prop_str("State file path to restore/persist")),
            ],
            &[],
        ),
        tool_def(
            "sharky_session_exec",
            "Execute a shell command or argv inside a named persistent session (or the default session).",
            session_exec_props,
            &[],
        ),
        tool_def(
            "sharky_parallel",
            "Run independent tasks concurrently in copy-on-write forked workers with shared output budgeting.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("policy", prop_str("wait-all (default) or fail-fast")),
                ("max_concurrency", prop_int("Maximum concurrent workers")),
                ("timeout_ms", prop_int("Group wall-clock timeout in milliseconds")),
                (
                    "tasks",
                    Value::Object(vec![
                        ks("type", "array"),
                        kv("items", Value::Object(vec![ks("type", "object")])),
                        ks("description", "Array of task objects ({id, command|argv, cwd, env, timeout_ms, output, tty, sandbox})"),
                    ]),
                ),
            ],
            &["tasks"],
        ),
        tool_def(
            "sharky_workflow",
            "Run a dependency DAG workflow with conditional edges, bounded retries, and cross-session named locks.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("policy", prop_str("wait-all or fail-fast")),
                ("failure_policy", prop_str("skip, fail, or continue")),
                ("max_concurrency", prop_int("Maximum concurrent workers")),
                ("timeout_ms", prop_int("Group wall-clock timeout in milliseconds")),
                (
                    "tasks",
                    Value::Object(vec![
                        ks("type", "array"),
                        kv("items", Value::Object(vec![ks("type", "object")])),
                        ks("description", "Array of workflow DAG task objects"),
                    ]),
                ),
            ],
            &["tasks"],
        ),
        tool_def(
            "sharky_code",
            "Execute a native Code Mode script programmatically orchestrating shell calls, parallel fan-out, and JSON transforms.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("code", prop_str("Code Mode program source")),
                ("args", Value::Object(vec![ks("description", "JSON argument exposed as global args")])),
                ("timeout_ms", prop_int("Execution timeout in milliseconds")),
                ("max_steps", prop_int("Step limit")),
                ("max_calls", prop_int("External tool call limit")),
            ],
            &["code"],
        ),
        tool_def(
            "sharky_read_capture",
            "Page raw unprojected output bytes by capture_id from the session capture store.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("capture_id", prop_str("Capture identifier from an earlier result")),
                ("stream", prop_str("stdout or stderr")),
                ("offset", prop_int("Byte offset")),
                ("max_bytes", prop_int("Maximum bytes to read")),
            ],
            &["capture_id"],
        ),
        tool_def(
            "sharky_cancel",
            "Signal active background process groups tracked inside a session.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("pgid", prop_int("Optional specific job process group id to signal")),
            ],
            &[],
        ),
        tool_def(
            "sharky_session_close",
            "Close a named persistent session opened with sharky_session_open and run its EXIT cleanup.",
            vec![("session_id", prop_str("Session id to close"))],
            &["session_id"],
        ),
        tool_def(
            "sharky_snapshot",
            "Save an in-memory checkpoint of session shell state (cwd, env, vars, arrays, funcs, traps, options).",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("snapshot_id", prop_str("Optional custom snapshot id")),
            ],
            &[],
        ),
        tool_def(
            "sharky_restore",
            "Restore a previously saved session checkpoint by snapshot_id.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("snapshot_id", prop_str("Snapshot identifier from sharky_snapshot")),
            ],
            &["snapshot_id"],
        ),
        tool_def(
            "sharky_job_start",
            "Start an asynchronous background job with optional wait_until (regex or localhost TCP port readiness).",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("command", prop_str("Shell command string")),
                (
                    "argv",
                    Value::Object(vec![
                        ks("type", "array"),
                        kv("items", prop_str("Argument")),
                        ks("description", "Direct argv execution bypassing shell parsing"),
                    ]),
                ),
                ("cwd", prop_str("Working directory override")),
                ("env", Value::Object(vec![ks("type", "object")])),
                (
                    "wait_until",
                    Value::Object(vec![
                        ks("type", "object"),
                        ks("description", "Wait condition ({regex?, port?, timeout_ms?})"),
                    ]),
                ),
            ],
            &[],
        ),
        tool_def(
            "sharky_job_poll",
            "Read incremental stdout/stderr deltas from a background job by byte cursor.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("job_id", prop_str("Job identifier from sharky_job_start")),
                ("stdout_cursor", prop_int("Previous stdout cursor offset")),
                ("stderr_cursor", prop_int("Previous stderr cursor offset")),
                ("wait_ms", prop_int("Max milliseconds to wait for new output or exit")),
            ],
            &["job_id"],
        ),
        tool_def(
            "sharky_job_send",
            "Write stdin data to a running background job and optionally close stdin.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("job_id", prop_str("Job identifier")),
                ("data", prop_str("UTF-8 input payload")),
                ("close_stdin", prop_bool("Close child stdin after writing")),
            ],
            &["job_id"],
        ),
        tool_def(
            "sharky_job_stop",
            "Stop and reap a background job process group and return remaining output.",
            vec![
                ("session_id", prop_str("Target session id (optional)")),
                ("job_id", prop_str("Job identifier")),
                ("signal", prop_str("Signal name: TERM (default), KILL, INT, or HUP")),
            ],
            &["job_id"],
        ),
    ];
    Value::Array(tools)
}
