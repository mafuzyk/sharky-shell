//! End-to-end tests for 4.0.0-alpha agent discovery, Phase G closure, and `--mcp`.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::agent::{NO_CONFIG, Session, json};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-phase-g-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
}

fn sharky(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("run sharky")
}

fn parse(text: &str) -> json::Value {
    json::parse_line(text.as_bytes()).unwrap_or_else(|err| panic!("invalid JSON {text:?}: {err}"))
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

fn ready(session: &mut Session) -> json::Value {
    let frame = session.json();
    assert_eq!(text(get(&frame, "type")), "ready", "{frame:?}");
    frame
}

#[test]
fn agent_guide_json_and_plain_text_describe_shell_and_mcp() {
    let json_out = sharky(&["agent", "guide", "--json"]);
    assert_eq!(json_out.status.code(), Some(0));
    let stdout = String::from_utf8(json_out.stdout).expect("utf8 guide json");
    let doc = parse(stdout.trim());
    assert_eq!(int(get(&doc, "schema_version")), 1);
    assert_eq!(text(get(&doc, "kind")), "agent_guide");
    assert_eq!(text(get(&doc, "shell_version")), "0.1.2");
    let tools = get(&doc, "mcp_tools").as_array().expect("mcp_tools array");
    assert_eq!(tools.len(), 16);
    assert!(tools.contains(&json::Value::String("sharky_exec".to_string())));
    assert!(tools.contains(&json::Value::String("sharky_code".to_string())));

    let plain_out = sharky(&["agent", "guide"]);
    assert_eq!(plain_out.status.code(), Some(0));
    let plain = String::from_utf8(plain_out.stdout).expect("utf8 plain guide");
    assert!(plain.contains("sharky agent --mcp"));
    assert!(plain.contains("sharky capabilities --json"));
    assert!(plain.contains("op:\"code\""));

    let bad = sharky(&["agent", "guide", "--unknown"]);
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn completions_list_json_and_help_json_flags() {
    let comp = sharky(&["completions", "list", "--json"]);
    assert_eq!(comp.status.code(), Some(0));
    let comp_json = parse(String::from_utf8(comp.stdout).unwrap().trim());
    assert_eq!(int(get(&comp_json, "schema_version")), 1);
    let subs = get(&comp_json, "cli_subcommands").as_array().unwrap();
    assert!(subs.contains(&json::Value::String("agent".to_string())));
    assert!(subs.contains(&json::Value::String("completions".to_string())));
    let mcp_tools = get(&comp_json, "mcp_tools").as_array().unwrap();
    assert_eq!(mcp_tools.len(), 16);

    let help_agent = sharky(&["help", "agent", "--json"]);
    assert_eq!(help_agent.status.code(), Some(0));
    let help_json = parse(String::from_utf8(help_agent.stdout).unwrap().trim());
    let flags = get(&help_json, "flags").as_array().expect("structured flags");
    assert!(flags.iter().any(|f| {
        f.get("flag").and_then(json::Value::as_str) == Some("--mcp")
    }));
    assert!(flags.iter().any(|f| {
        f.get("flag").and_then(json::Value::as_str) == Some("--state-file")
            && f.get("takes_value").and_then(json::Value::as_bool) == Some(true)
    }));
}

#[test]
fn exec_with_tty_allocates_real_pty_and_preserves_session_state() {
    let dir = scratch("tty");
    let sub = dir.join("work");
    std::fs::create_dir_all(&sub).unwrap();

    let mut session = Session::spawn(&[]);
    let _ = ready(&mut session);

    session.send(&format!(
        r#"{{"schema_version":1,"id":"t1","op":"exec","tty":true,"command":"test -t 1 && echo TTY_OK && cd '{}' && export FROM_TTY=yes"}}"#,
        sub.display()
    ));
    let res = session.json();
    assert_eq!(text(get(&res, "type")), "result");
    assert_eq!(int(get(&res, "status")), 0, "{res:?}");
    let out = text(get(&res, "stdout"));
    assert!(out.contains("TTY_OK"), "missing TTY_OK in {out:?}");

    session.send(r#"{"schema_version":1,"id":"t2","op":"exec","command":"pwd && printf '%s' \"$FROM_TTY\""}"#);
    let follow = session.json();
    assert_eq!(int(get(&follow, "status")), 0);
    let follow_out = text(get(&follow, "stdout"));
    assert!(follow_out.contains("yes"));
    assert!(follow_out.contains("work"));

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let _ = session.json();
    let _ = session.json();
    let (status, _, _) = session.finish();
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exec_with_stream_emits_monotonic_chunks_before_result() {
    let mut session = Session::spawn(&[]);
    let _ = ready(&mut session);

    session.send(
        r#"{"schema_version":1,"id":"st1","op":"exec","stream":true,"command":"echo chunk_one; sleep 0.05; echo chunk_two >&2; sleep 0.05; echo chunk_three"}"#,
    );

    let mut chunks = Vec::new();
    let result = loop {
        let frame = session.json();
        match text(get(&frame, "type")) {
            "stdout_chunk" | "stderr_chunk" => chunks.push(frame),
            "result" => break frame,
            other => panic!("unexpected frame {other}: {frame:?}"),
        }
    };

    assert!(!chunks.is_empty(), "expected at least one stream chunk");
    let mut saw_out = String::new();
    let mut saw_err = String::new();
    for (expected_seq, chunk) in chunks.iter().enumerate() {
        assert_eq!(text(get(chunk, "id")), "st1");
        assert_eq!(int(get(chunk, "seq")), expected_seq as i64);
        let data = text(get(chunk, "data"));
        match text(get(chunk, "type")) {
            "stdout_chunk" => saw_out.push_str(data),
            "stderr_chunk" => saw_err.push_str(data),
            _ => unreachable!(),
        }
    }
    assert!(saw_out.contains("chunk_one"));
    assert!(saw_out.contains("chunk_three"));
    assert!(saw_err.contains("chunk_two"));
    assert_eq!(int(get(&result, "status")), 0);
    assert!(text(get(&result, "stdout")).contains("chunk_three"));

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let _ = session.json();
    let _ = session.json();
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn cross_session_lock_serializes_concurrent_agent_processes() {
    let dir = scratch("flock");
    let tmp = dir.join("tmp");
    let log = dir.join("events.log");
    std::fs::create_dir_all(&tmp).unwrap();
    let tmp_str = tmp.to_str().unwrap();

    let mut s1 = Session::spawn_with_env(&[], &[("TMPDIR", tmp_str)]);
    let _ = ready(&mut s1);
    let mut s2 = Session::spawn_with_env(&[], &[("TMPDIR", tmp_str)]);
    let _ = ready(&mut s2);

    // Session 1 acquires the lock and sleeps 0.3s before writing RELEASE_1.
    s1.send(&format!(
        r#"{{"schema_version":1,"id":"l1","op":"exec","lock":"build:shared","command":"echo ENTER_1 >> '{}'; sleep 0.30; echo LEAVE_1 >> '{}'"}}"#,
        log.display(),
        log.display()
    ));

    // Wait long enough for session 1 to hold the lock before session 2 asks for it.
    std::thread::sleep(Duration::from_millis(80));
    s2.send(&format!(
        r#"{{"schema_version":1,"id":"l2","op":"exec","lock":"build:shared","command":"echo ENTER_2 >> '{}'; echo LEAVE_2 >> '{}'"}}"#,
        log.display(),
        log.display()
    ));

    let r1 = s1.json();
    let r2 = s2.json();
    assert_eq!(int(get(&r1, "status")), 0, "{r1:?}");
    assert_eq!(int(get(&r2, "status")), 0, "{r2:?}");

    let contents = std::fs::read_to_string(&log).expect("read log");
    let lines: Vec<&str> = contents.lines().collect();
    assert_eq!(lines, vec!["ENTER_1", "LEAVE_1", "ENTER_2", "LEAVE_2"]);

    // Verify timeout while blocked by another process's held lock.
    s1.send(r#"{"schema_version":1,"id":"l3","op":"exec","lock":"build:timeout","command":"sleep 0.40"}"#);
    std::thread::sleep(Duration::from_millis(60));
    s2.send(r#"{"schema_version":1,"id":"l4","op":"exec","lock":"build:timeout","timeout_ms":100,"command":"echo SHOULD_NOT_RUN"}"#);

    let r2_timeout = s2.json();
    assert_eq!(text(get(&r2_timeout, "termination")), "timeout", "{r2_timeout:?}");
    assert_eq!(int(get(&r2_timeout, "status")), 124);
    let r1_done = s1.json();
    assert_eq!(int(get(&r1_done, "status")), 0);

    for mut sess in [s1, s2] {
        sess.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
        let _ = sess.json();
        let _ = sess.json();
        let (st, _, _) = sess.finish();
        assert!(st.success());
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sandbox_enforces_no_new_privs_and_rlimit_fsize_without_poisoning_session() {
    let dir = scratch("sandbox");
    let out_file = dir.join("big.bin");

    let mut session = Session::spawn(&[]);
    let _ = ready(&mut session);

    // Check PR_SET_NO_NEW_PRIVS inside sandbox child.
    session.send(
        r#"{"schema_version":1,"id":"sb1","op":"exec","sandbox":true,"command":"grep NoNewPrivs /proc/self/status"}"#,
    );
    let sb1 = session.json();
    let out1 = text(get(&sb1, "stdout"));
    assert!(out1.contains("NoNewPrivs:\t1"), "expected NoNewPrivs: 1 in {sb1:?}");

    // Enforce small max_file_bytes limit inside sandbox without persisting onto session.
    session.send(&format!(
        r#"{{"schema_version":1,"id":"sb2","op":"exec","sandbox":{{"max_file_bytes":1024}},"command":"dd if=/dev/zero of='{}' bs=4096 count=1 2>/dev/null; echo $?"}}"#,
        out_file.display()
    ));
    let sb2 = session.json();
    let status = int(get(&sb2, "status"));
    let termination = text(get(&sb2, "termination"));
    let meta_len = std::fs::metadata(&out_file).map(|m| m.len()).unwrap_or(0);
    assert!(
        termination == "signaled" || status != 0 || meta_len <= 1024,
        "RLIMIT_FSIZE failed to cap write: status={status} term={termination} len={meta_len} res={sb2:?}"
    );
    assert!(meta_len <= 1024, "file exceeded RLIMIT_FSIZE 1024: {meta_len}");

    // Subsequent unsandboxed command must NOT inherit NoNewPrivs or RLIMIT_FSIZE.
    let big_after = dir.join("normal.bin");
    session.send(&format!(
        r#"{{"schema_version":1,"id":"sb3","op":"exec","command":"printf '%02048d' 0 > '{}' && wc -c < '{}'"}}"#,
        big_after.display(),
        big_after.display()
    ));
    let sb3 = session.json();
    assert_eq!(int(get(&sb3, "status")), 0, "{sb3:?}");
    assert_eq!(text(get(&sb3, "stdout")).trim(), "2048");

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let _ = session.json();
    let _ = session.json();
    let (status, _, _) = session.finish();
    assert!(status.success());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn state_file_persists_and_restores_session_across_restarts() {
    let dir = scratch("resume");
    let state_path = dir.join("session.state.json");
    let work_dir = dir.join("project");
    std::fs::create_dir_all(&work_dir).unwrap();
    let state_arg = state_path.to_str().unwrap();

    // Session 1 mutates cwd, env, scalar variable, and alias, then closes cleanly.
    let mut s1 = Session::spawn(&["--state-file", state_arg]);
    let _ = ready(&mut s1);
    s1.send(&format!(
        r#"{{"schema_version":1,"id":"r1","op":"exec","command":"cd '{}' && export PERSIST_ENV=hello42 && LOCAL_VAR=local99 && alias greet='echo ALIAS_OK'"}}"#,
        work_dir.display()
    ));
    let r1 = s1.json();
    assert_eq!(int(get(&r1, "status")), 0, "{r1:?}");
    s1.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let _ = s1.json();
    let _ = s1.json();
    assert!(s1.finish().0.success());
    assert!(state_path.exists());

    // Session 2 resumes using `--resume` and verifies all four state domains.
    let mut s2 = Session::spawn(&["--resume", state_arg]);
    let _ = ready(&mut s2);
    s2.send(r#"{"schema_version":1,"id":"r2","op":"exec","command":"pwd && printf '%s|%s|' \"$PERSIST_ENV\" \"$LOCAL_VAR\" && greet"}"#);
    let r2 = s2.json();
    assert_eq!(int(get(&r2, "status")), 0, "{r2:?}");
    let out = text(get(&r2, "stdout"));
    assert!(out.contains("project"), "cwd not restored: {out:?}");
    assert!(out.contains("hello42|local99|"), "env/vars not restored: {out:?}");
    assert!(out.contains("ALIAS_OK"), "alias not restored: {out:?}");

    s2.send(r#"{"schema_version":1,"id":"c2","op":"close"}"#);
    let _ = s2.json();
    let _ = s2.json();
    assert!(s2.finish().0.success());
    let _ = std::fs::remove_dir_all(&dir);
}

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: BufReader<ChildStdout>,
}

impl McpClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["agent", "--mcp", "--no-config"])
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("PATH", common::tool_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sharky agent --mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        Self {
            child,
            stdin: Some(stdin),
            reader: BufReader::new(stdout),
        }
    }

    fn send_ndjson(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    fn recv_ndjson(&mut self) -> json::Value {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).expect("read ndjson response");
        assert!(n > 0, "unexpected EOF from --mcp");
        parse(line.trim())
    }

    fn call_tool(&mut self, id: i64, name: &str, args_json: &str) -> json::Value {
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args_json}}}}}"#
        );
        self.send_ndjson(&req);
        let resp = self.recv_ndjson();
        assert_eq!(int(get(&resp, "id")), id);
        let result = get(&resp, "result");
        assert_eq!(
            result.get("isError").and_then(json::Value::as_bool),
            Some(false),
            "tool {name} returned error: {resp:?}"
        );
        get(result, "structuredContent").clone()
    }

    fn finish(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "mcp server exited with {status}");
                return;
            }
            assert!(Instant::now() < deadline, "mcp server hung on EOF");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn native_mcp_server_ndjson_and_content_length_all_ten_tools() {
    // 1. NDJSON lifecycle across all 10 tools.
    let mut mcp = McpClient::spawn();
    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
    );
    let init = mcp.recv_ndjson();
    assert_eq!(text(get(get(&init, "result"), "protocolVersion")), "2024-11-05");
    mcp.send_ndjson(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    mcp.send_ndjson(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#);
    let ping = mcp.recv_ndjson();
    assert_eq!(int(get(&ping, "id")), 2);

    mcp.send_ndjson(r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#);
    let listed = mcp.recv_ndjson();
    let tools = get(get(&listed, "result"), "tools").as_array().unwrap();
    assert_eq!(tools.len(), 16);
    let exec_tool = tools
        .iter()
        .find(|t| t.get("name").and_then(json::Value::as_str) == Some("sharky_exec"))
        .expect("sharky_exec schema");
    let exec_props = get(get(exec_tool, "inputSchema"), "properties");
    assert!(
        get(get(exec_props, "env"), "description")
            .as_str()
            .is_some_and(|d| d.contains("requires isolated: true")),
        "{exec_props:?}"
    );
    assert!(
        get(get(exec_props, "output"), "description")
            .as_str()
            .is_some_and(|d| d.contains("strip_ansi")),
        "{exec_props:?}"
    );

    let caps = mcp.call_tool(10, "sharky_capabilities", "{}");
    assert!(get(&caps, "agent_transports").as_array().unwrap().contains(&json::Value::String("mcp".to_string())));

    let ex = mcp.call_tool(11, "sharky_exec", r#"{"command":"echo MCP_CAPTURE_LINE"}"#);
    assert_eq!(int(get(&ex, "status")), 0);
    let cap_id = text(get(&ex, "capture_id")).to_string();

    let cap = mcp.call_tool(
        12,
        "sharky_read_capture",
        &format!(r#"{{"capture_id":"{cap_id}","stream":"stdout","offset":0,"max_bytes":1024}}"#),
    );
    assert!(text(get(&cap, "data")).contains("MCP_CAPTURE_LINE"));

    let opened = mcp.call_tool(13, "sharky_session_open", r#"{"session_id":"work"}"#);
    assert_eq!(text(get(&opened, "session_id")), "work");

    let sex1 = mcp.call_tool(14, "sharky_session_exec", r#"{"session_id":"work","command":"export MCP_VAR=sticky"}"#);
    assert_eq!(int(get(&sex1, "status")), 0);
    let sex2 = mcp.call_tool(15, "sharky_session_exec", r#"{"session_id":"work","command":"printf '%s' \"$MCP_VAR\""}"#);
    assert_eq!(text(get(&sex2, "stdout")), "sticky");

    let par = mcp.call_tool(16, "sharky_parallel", r#"{"tasks":[{"id":"a","command":"echo PAR_A"},{"id":"b","command":"echo PAR_B"}]}"#);
    assert_eq!(text(get(&par, "state")), "completed");
    assert_eq!(int(get(&par, "status")), 0, "sharky_parallel failed: {par:?}");
    let par_tasks = get(&par, "tasks").as_array().expect("par tasks array");
    assert_eq!(int(get(&par_tasks[0], "status")), 0, "{par_tasks:?}");
    assert_eq!(text(get(&par_tasks[0], "stdout")), "PAR_A\n");
    assert_eq!(int(get(&par_tasks[1], "status")), 0, "{par_tasks:?}");
    assert_eq!(text(get(&par_tasks[1], "stdout")), "PAR_B\n");

    let wf = mcp.call_tool(17, "sharky_workflow", r#"{"tasks":[{"id":"one","command":"echo WF1"},{"id":"two","depends_on":["one"],"command":"echo WF2"}]}"#);
    assert_eq!(text(get(&wf, "state")), "completed");
    assert_eq!(int(get(&wf, "status")), 0, "sharky_workflow failed: {wf:?}");
    let wf_tasks = get(&wf, "tasks").as_array().expect("wf tasks array");
    assert_eq!(int(get(&wf_tasks[0], "status")), 0, "{wf_tasks:?}");
    assert_eq!(text(get(&wf_tasks[0], "stdout")), "WF1\n");
    assert_eq!(int(get(&wf_tasks[1], "status")), 0, "{wf_tasks:?}");
    assert_eq!(text(get(&wf_tasks[1], "stdout")), "WF2\n");

    let before_iso = mcp.call_tool(22, "sharky_exec", r#"{"command":"pwd"}"#);
    assert_eq!(int(get(&before_iso, "status")), 0);
    let orig_pwd = text(get(&before_iso, "stdout")).to_string();
    let iso = mcp.call_tool(
        23,
        "sharky_exec",
        r#"{"isolated":true,"cwd":"/tmp","env":{"ISO_K":"iso_v"},"command":"pwd; printf '%s' \"$ISO_K\""}"#,
    );
    assert_eq!(int(get(&iso, "status")), 0, "isolated sharky_exec failed: {iso:?}");
    assert_eq!(text(get(&iso, "stdout")), "/tmp\niso_v");
    let after_iso = mcp.call_tool(
        24,
        "sharky_exec",
        r#"{"command":"pwd; printf '%s' \"${ISO_K:-unset}\""}"#,
    );
    assert_eq!(int(get(&after_iso, "status")), 0);
    assert_eq!(
        text(get(&after_iso, "stdout")),
        format!("{orig_pwd}unset")
    );

    let s_iso = mcp.call_tool(
        25,
        "sharky_session_exec",
        r#"{"session_id":"work","isolated":true,"cwd":"/tmp","env":{"S_ISO":"ok"},"command":"pwd; printf '%s' \"$S_ISO\""}"#,
    );
    assert_eq!(int(get(&s_iso, "status")), 0, "isolated sharky_session_exec failed: {s_iso:?}");
    assert_eq!(text(get(&s_iso, "stdout")), "/tmp\nok");

    let cm = mcp.call_tool(
        18,
        "sharky_code",
        r#"{"code":"let p = parallel([{ id: \"p1\", argv: [\"printf\", \"MCP_CODE_PAR\"] }]); return { val: p.tasks[0].stdout, st: p.tasks[0].status };"}"#,
    );
    assert_eq!(int(get(&cm, "status")), 0, "sharky_code parallel failed: {cm:?}");
    assert_eq!(int(get(get(&cm, "audit"), "failed_calls")), 0, "{cm:?}");
    assert_eq!(text(get(get(&cm, "value"), "val")), "MCP_CODE_PAR");
    assert_eq!(int(get(get(&cm, "value"), "st")), 0);

    let _bg = mcp.call_tool(19, "sharky_session_exec", r#"{"session_id":"work","command":"sleep 30 &"}"#);
    let cancel = mcp.call_tool(20, "sharky_cancel", r#"{"session_id":"work"}"#);
    assert_eq!(get(&cancel, "cancelled").as_bool(), Some(true));

    let closed = mcp.call_tool(21, "sharky_session_close", r#"{"session_id":"work"}"#);
    assert_eq!(get(&closed, "closed").as_bool(), Some(true));

    mcp.finish();

    // 2. Content-Length framing support.
    let mut cl = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["agent", "--mcp", "--no-config"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sharky agent --mcp content-length");
    let body = r#"{"jsonrpc":"2.0","id":99,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
    let frame = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
    {
        let mut stdin = cl.stdin.take().unwrap();
        stdin.write_all(frame.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }
    let mut stdout_bytes = String::new();
    cl.stdout.take().unwrap().read_to_string(&mut stdout_bytes).unwrap();
    let status = cl.wait().unwrap();
    assert!(status.success());
    assert!(stdout_bytes.starts_with("Content-Length: "), "{stdout_bytes:?}");
    let (_, json_part) = stdout_bytes.split_once("\r\n\r\n").expect("header separator");
    let resp = parse(json_part);
    assert_eq!(int(get(&resp, "id")), 99);
    assert_eq!(text(get(get(get(&resp, "result"), "serverInfo"), "name")), "sharky");
}

#[test]
fn agent_guide_sandbox_recipe_executes_and_docs_match_protocol() {
    let guide_out = sharky(&["agent", "guide", "--json"]);
    assert_eq!(guide_out.status.code(), Some(0));
    let doc = parse(String::from_utf8(guide_out.stdout).unwrap().trim());

    let ops = get(&doc, "operations").as_array().expect("operations array");
    let state_op = ops
        .iter()
        .find(|op| op.get("op").and_then(json::Value::as_str) == Some("state"))
        .expect("state op doc");
    let state_summary = text(get(state_op, "summary"));
    assert!(
        state_summary.contains("always returned")
            && state_summary.contains("include accepts env, variables, functions, aliases, traps, jobs"),
        "misleading state summary: {state_summary}"
    );

    let r3 = text(get(get(&doc, "recipes"), "exec_pty_stream_sandbox"));
    let mut session = Session::spawn(&[]);
    let _ = ready(&mut session);
    session.send(r3);
    let result = loop {
        let frame = session.json();
        match frame.get("type").and_then(json::Value::as_str) {
            Some("stdout_chunk") | Some("stderr_chunk") => continue,
            Some("result") => break frame,
            other => panic!("unexpected frame while running recipe r3 ({r3}): {other:?} {frame:?}"),
        }
    };
    assert_eq!(int(get(&result, "status")), 0, "{result:?}");
    assert!(
        text(get(&result, "stdout")).contains("ready"),
        "{result:?}"
    );
    session.send(r#"{"schema_version":1,"id":"c-doc","op":"close"}"#);
    let _ = session.json();
    let _ = session.json();
    assert!(session.finish().0.success());

    let help_out = sharky(&["help", "agent"]);
    assert_eq!(help_out.status.code(), Some(0));
    let help_text = String::from_utf8(help_out.stdout).unwrap();
    for key in [
        "no_network",
        "read_only_root",
        "max_file_bytes",
        "max_procs",
        "cpu_seconds",
    ] {
        assert!(
            help_text.contains(key),
            "help agent missing sandbox key {key}: {help_text}"
        );
    }
}

#[test]
fn mcp_accepts_stringified_json_object_arguments() {
    let mut mcp = McpClient::spawn();
    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    let _ = mcp.recv_ndjson();

    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sharky_exec","arguments":"{\"command\":\"printf ok\"}"}}"#,
    );
    let resp = mcp.recv_ndjson();
    let structured = get(get(&resp, "result"), "structuredContent");
    assert_eq!(int(get(structured, "status")), 0, "{resp:?}");
    assert_eq!(text(get(structured, "stdout")), "ok");

    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"sharky_capabilities","arguments":"   "}}"#,
    );
    let caps_resp = mcp.recv_ndjson();
    let caps = get(get(&caps_resp, "result"), "structuredContent");
    assert!(get(caps, "agent_transports").as_array().is_some(), "{caps_resp:?}");

    mcp.finish();
}

#[test]
fn mcp_rejects_malformed_stringified_arguments_with_hint() {
    let mut mcp = McpClient::spawn();
    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    let _ = mcp.recv_ndjson();

    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"sharky_exec","arguments":"{broken"}}"#,
    );
    let resp = mcp.recv_ndjson();
    let err = get(&resp, "error");
    assert_eq!(int(get(err, "code")), -32602, "{resp:?}");
    assert!(
        text(get(err, "message")).contains("JSON object"),
        "{resp:?}"
    );

    mcp.finish();
}

#[test]
fn mcp_tools_list_array_properties_define_items() {
    let mut mcp = McpClient::spawn();
    mcp.send_ndjson(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#,
    );
    let _ = mcp.recv_ndjson();

    mcp.send_ndjson(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    let listed = mcp.recv_ndjson();
    let tools = get(get(&listed, "result"), "tools").as_array().unwrap();
    for tool in tools {
        let tool_name = text(get(tool, "name"));
        let props = get(get(tool, "inputSchema"), "properties")
            .as_object()
            .expect("properties object");
        for (prop_name, schema) in props {
            if schema.get("type").and_then(json::Value::as_str) == Some("array") {
                assert!(
                    schema.get("items").and_then(json::Value::as_object).is_some(),
                    "{tool_name}.{prop_name} missing items object: {schema:?}"
                );
            }
        }
    }

    mcp.finish();
}
