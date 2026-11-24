//! End-to-end tests for `sharky agent --stdio`.
//!
//! Every case drives the real binary over pipes, exactly as a client would. The
//! gate is design §10.1 rows 1–9 minus the cancellation rows, plus the §10.2
//! P0 rows; each test name names the row it covers. The deadlock discipline is
//! one write per read, and the helper's line reader polls, so a missing
//! response fails instead of hanging.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::agent::{Session, json};

const NO_CONFIG: &str = "/nonexistent/sharky-agent-session-test";

// -------------------------------------------------------------------- helpers

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-agent-session-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
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

fn flag(value: &json::Value) -> bool {
    value
        .as_bool()
        .unwrap_or_else(|| panic!("not a boolean: {value:?}"))
}

fn array(value: &json::Value) -> &[json::Value] {
    value
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {value:?}"))
}

/// The first line of every session: `ready`.
fn ready(session: &mut Session) -> json::Value {
    let response = session.json();
    assert_eq!(text(get(&response, "type")), "ready", "{response:?}");
    response
}

fn expect_type<'a>(response: &'a json::Value, kind: &str) -> &'a json::Value {
    assert_eq!(text(get(response, "type")), kind, "{response:?}");
    response
}

fn error_code(response: &json::Value) -> &str {
    expect_type(response, "error");
    text(get(get(response, "error"), "code"))
}

fn error_is_fatal(response: &json::Value) -> bool {
    flag(get(get(response, "error"), "fatal"))
}

fn stdout(result: &json::Value) -> String {
    assert_eq!(
        text(get(result, "stdout_encoding")),
        "utf8",
        "expected a UTF-8 stdout stream: {result:?}"
    );
    text(get(result, "stdout")).to_string()
}

fn stderr(result: &json::Value) -> String {
    assert_eq!(
        text(get(result, "stderr_encoding")),
        "utf8",
        "expected a UTF-8 stderr stream: {result:?}"
    );
    text(get(result, "stderr")).to_string()
}

fn status(result: &json::Value) -> i64 {
    int(get(result, "status"))
}

fn exec_request(id: &str, command: &str) -> String {
    format!(
        r#"{{"schema_version":1,"id":"{id}","op":"exec","command":{}}}"#,
        json::quote(command)
    )
}

fn exec(session: &mut Session, id: &str, command: &str) -> json::Value {
    session.send(&exec_request(id, command));
    let response = session.json();
    expect_type(&response, "result");
    assert_eq!(text(get(&response, "id")), id);
    response
}

fn exec_with_timeout(id: &str, command: &str, timeout_ms: i64) -> String {
    format!(
        r#"{{"schema_version":1,"id":"{id}","op":"exec","command":{},"timeout_ms":{timeout_ms}}}"#,
        json::quote(command)
    )
}

fn cancel_request(id: &str, target: &str, grace_ms: Option<i64>) -> String {
    match grace_ms {
        Some(grace) => format!(
            r#"{{"schema_version":1,"id":"{id}","op":"cancel","target":"{target}","grace_ms":{grace}}}"#
        ),
        None => format!(
            r#"{{"schema_version":1,"id":"{id}","op":"cancel","target":"{target}"}}"#
        ),
    }
}

/// The result of a `cancel`/`close` ack: the protocol's accepted vocabulary.
fn ack(session: &mut Session, op: &str) -> json::Value {
    let response = session.json();
    expect_type(&response, "ack");
    assert_eq!(text(get(&response, "op")), op);
    assert_eq!(text(get(&response, "outcome")), "delivered");
    response
}

/// `kill(pid, 0)` plus the zombie state, so a reaped-but-not-yet-collected
/// child counts as gone. A failed `/proc` read means the pid left the table.
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

fn gone_within(pid: i32, ms: i64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms as u64);
    while Instant::now() < deadline {
        if process_gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    process_gone(pid)
}

fn sharky(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::null())
        .output()
        .expect("run sharky")
}

// ----------------------------------------------------------------- ready frame

#[test]
fn ready_announces_only_the_ops_and_features_this_build_honors() {
    let mut session = Session::spawn(&[]);
    let response = ready(&mut session);
    assert_eq!(int(get(&response, "schema_version")), 1);
    assert!(
        response.get("id").is_none(),
        "ready must not carry an id: {response:?}"
    );
    assert_eq!(int(get(&response, "pid")), i64::from(session.pid()));
    assert!(text(get(&response, "session")).starts_with("s-"));

    let protocol = get(&response, "protocol");
    assert_eq!(int(&array(get(protocol, "versions"))[0]), 1);
    let ops = array(get(protocol, "ops"))
        .iter()
        .map(text)
        .collect::<Vec<_>>();
    assert_eq!(
        ops,
        vec![
            "exec",
            "state",
            "cancel",
            "close",
            "parallel",
            "workflow",
            "read_capture",
            "code",
        ]
    );
    let features = array(get(protocol, "features"))
        .iter()
        .map(text)
        .collect::<Vec<_>>();
    for feature in ["stdin_payload", "env_delta", "binary_base64"] {
        assert!(features.contains(&feature), "missing feature {feature}");
    }
    for feature in ["cancel", "timeout", "process_groups"] {
        assert!(features.contains(&feature), "missing feature {feature}");
    }
    // Execution exists, so the op and its env-delta feature are advertised;
    // the supervisor honors per-task cancel, so that feature is advertised too.
    for feature in [
        "parallel",
        "parallel_env_delta",
        "parallel_task_cancel",
        "workflow",
        "workflow_retry",
        "workflow_condition",
        "workflow_lock",
        "workflow_task_cancel",
        "argv",
        "exec_isolated",
        "env_redaction",
        "output_projection",
        "capture_store",
        "code_mode",
        "pty",
        "streaming_output",
        "cross_session_locks",
        "opt_in_sandbox",
        "session_resume",
        "mcp",
    ] {
        assert!(features.contains(&feature), "missing feature {feature}");
    }
    assert_eq!(int(get(protocol, "cancel_grace_ms")), 2000);

    let limits = get(protocol, "limits");
    assert_eq!(int(get(limits, "max_line_bytes")), 1024 * 1024);
    assert_eq!(int(get(limits, "max_stdin_bytes")), 1024 * 1024);
    assert_eq!(int(get(limits, "max_capture_bytes")), 4 * 1024 * 1024);
    assert_eq!(int(get(limits, "max_id_bytes")), 256);
    assert_eq!(int(get(limits, "max_workflow_attempts")), 8);
    assert_eq!(int(get(limits, "max_workflow_backoff_ms")), 60_000);

    let (status, leftovers, errors) = session.finish();
    assert_eq!(status.code(), Some(0));
    assert!(leftovers.is_empty());
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ------------------------------------------------- §10.1 row 1: state persists

#[test]
fn assignment_persists_across_requests() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let first = exec(&mut session, "r1", "x=5");
    assert_eq!(status(&first), 0);
    assert_eq!(stdout(&first), "");
    let second = exec(&mut session, "r2", "echo $x");
    assert_eq!(stdout(&second), "5\n");
    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty());
}

#[test]
fn working_directory_persists_and_is_reported() {
    let dir = scratch("cwd");
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let first = exec(&mut session, "r1", &format!("cd {}", dir.display()));
    assert_eq!(status(&first), 0);
    assert_eq!(text(get(&first, "cwd")), dir.to_string_lossy());
    let second = exec(&mut session, "r2", "pwd");
    assert_eq!(stdout(&second), format!("{}\n", dir.to_string_lossy()));
    assert_eq!(text(get(&second, "cwd")), dir.to_string_lossy());
    session.finish();
}

// ------------------------------------- §10.1 row 3: functions and statuses

#[test]
fn functions_and_failure_statuses_persist() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let defined = exec(&mut session, "r1", "f() { echo f; }");
    assert_eq!(status(&defined), 0);
    let called = exec(&mut session, "r2", "f");
    assert_eq!(stdout(&called), "f\n");
    let failed = exec(&mut session, "r3", "false");
    assert_eq!(status(&failed), 1);
    let observed = exec(&mut session, "r4", "echo $?");
    assert_eq!(stdout(&observed), "1\n");
    session.finish();
}

// ------------------------------- §10.1 row 4: a response before the channel ends

#[test]
fn a_result_arrives_before_the_transport_ends() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    // stdin stays open: the answer must not wait for EOF.
    let result = exec(&mut session, "r1", "echo hi");
    assert_eq!(stdout(&result), "hi\n");
    assert_eq!(status(&result), 0);
    let (exit, _, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
}

// --------------------------- §10.1 row 5: read/cat never eat control frames

#[test]
fn read_and_cat_do_not_consume_the_control_channel() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Three frames are already queued in the pipe while the first request
    // blocks in `read`; all three must still be answered in order.
    session.send(&exec_request("r1", "read x; echo got:$x; read y"));
    session.send(&exec_request("r2", "echo next"));
    session.send(&exec_request("r3", "cat"));

    let first = session.json();
    assert_eq!(text(get(&first, "id")), "r1", "{first:?}");
    assert_eq!(stdout(&first), "got:\n");
    // The `read` fails at end of input; the last `read y` keeps that status.
    assert_eq!(status(&first), 1);

    let second = session.json();
    assert_eq!(text(get(&second, "id")), "r2", "{second:?}");
    assert_eq!(stdout(&second), "next\n");

    // `cat` with no payload reaches end of the empty payload file at once.
    let third = session.json();
    assert_eq!(text(get(&third, "id")), "r3", "{third:?}");
    assert_eq!(stdout(&third), "");
    assert_eq!(status(&third), 0);

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty());
}

// ---------------------- §10.1 row 6: nothing is printed outside the framing

#[test]
fn builtin_external_and_stderr_output_stay_inside_the_json_lines() {
    let echo = common::resolve("echo");
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(
        &mut session,
        "r1",
        &format!("echo a; echo b >&2; {} c", echo.display()),
    );
    assert_eq!(stdout(&result), "a\nc\n");
    assert_eq!(stderr(&result), "b\n");
    assert_eq!(status(&result), 0);

    // Closing gives the count its teeth: any byte the agent wrote outside a
    // response would appear here, and every response line parsed as JSON.
    session.send(r#"{"schema_version":1,"id":"r2","op":"close"}"#);
    let ack = session.json();
    expect_type(&ack, "ack");
    let closed = session.json();
    expect_type(&closed, "closed");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "lines after close: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ------------------- §10.1 row 7: protocol errors, then the session lives on

#[test]
fn protocol_errors_are_reported_without_killing_the_session() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send("{not json");
    let invalid = session.json();
    assert_eq!(error_code(&invalid), "INVALID_JSON");
    assert!(
        invalid.get("id").is_some(),
        "every error carries an id slot"
    );
    assert!(get(&invalid, "id").is_null());
    assert!(!error_is_fatal(&invalid));

    session.send(r#"{"schema_version":2,"id":"v1","op":"exec","command":"true"}"#);
    let version = session.json();
    assert_eq!(error_code(&version), "UNSUPPORTED_SCHEMA_VERSION");
    assert_eq!(text(get(&version, "id")), "v1");

    let first = exec(&mut session, "dup", "echo once");
    assert_eq!(stdout(&first), "once\n");
    session.send(&exec_request("dup", "echo twice"));
    let duplicate = session.json();
    assert_eq!(error_code(&duplicate), "DUPLICATE_ID");
    assert_eq!(text(get(&duplicate, "id")), "dup");

    session.send(r#"{"schema_version":1,"id":"r-cancel","op":"cancel","target":"dup"}"#);
    let late = session.json();
    assert_eq!(error_code(&late), "CANCEL_TOO_LATE");
    assert!(!error_is_fatal(&late));

    session.send(r#"{"schema_version":1,"id":"r-unknown","op":"bogus"}"#);
    let unknown = session.json();
    assert_eq!(error_code(&unknown), "UNKNOWN_OP");
    assert!(!error_is_fatal(&unknown));

    // `parallel` and `workflow` are honored now, so malformed ones are ordinary
    // request errors and the session keeps serving.
    session.send(r#"{"schema_version":1,"id":"r-parallel","op":"parallel"}"#);
    let invalid = session.json();
    assert_eq!(error_code(&invalid), "INVALID_REQUEST");
    assert_eq!(text(get(&invalid, "id")), "r-parallel");
    assert!(!error_is_fatal(&invalid));

    session.send(r#"{"schema_version":1,"id":"r-workflow","op":"workflow"}"#);
    let invalid_wf = session.json();
    assert_eq!(error_code(&invalid_wf), "INVALID_REQUEST");
    assert_eq!(text(get(&invalid_wf, "id")), "r-workflow");
    assert!(!error_is_fatal(&invalid_wf));

    // `timeout_ms` is honored, so this is a result and not an error any more.
    session.send(
        r#"{"schema_version":1,"id":"r-timeout","op":"exec","command":"true","timeout_ms":5}"#,
    );
    let timeout = session.json();
    expect_type(&timeout, "result");
    assert_eq!(status(&timeout), 0);

    let survived = exec(&mut session, "r-ok", "echo ok");
    assert_eq!(stdout(&survived), "ok\n");
    session.finish();
}

#[test]
fn an_incomplete_final_line_is_a_fatal_partial_request() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send_raw(br#"{"schema_version":1,"id":"r1","op":"exec","command":"tr"#);
    session.close_stdin();

    let response = session.json();
    assert_eq!(error_code(&response), "PARTIAL_REQUEST");
    assert!(error_is_fatal(&response));

    let (status, leftovers, _) = session.finish();
    assert_eq!(status.code(), Some(1), "PARTIAL_REQUEST exits 1");
    assert!(leftovers.is_empty());
}

#[test]
fn a_complete_final_line_without_a_newline_is_executed() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send_raw(br#"{"schema_version":1,"id":"r1","op":"exec","command":"echo final"}"#);
    session.close_stdin();

    let result = session.json();
    expect_type(&result, "result");
    assert_eq!(stdout(&result), "final\n");

    let (status, leftovers, _) = session.finish();
    assert_eq!(status.code(), Some(0));
    assert!(leftovers.is_empty());
}

#[test]
fn an_oversized_line_is_reported_and_the_session_keeps_serving() {
    let mut session = Session::spawn(&["--max-line-bytes", "256"]);
    let ready_frame = ready(&mut session);
    assert_eq!(
        int(get(get(&ready_frame, "protocol"), "limits")
            .get("max_line_bytes")
            .unwrap()),
        256
    );

    let oversized = format!("{}\n", "a".repeat(1000));
    session.send_raw(oversized.as_bytes());
    let response = session.json();
    assert_eq!(error_code(&response), "MESSAGE_TOO_LARGE");
    assert!(!error_is_fatal(&response));

    let survived = exec(&mut session, "r1", "echo ok");
    assert_eq!(stdout(&survived), "ok\n");
    session.finish();
}

// ---------------------- §10.1 row 8: exit, exec refusal and the EXIT trap

#[test]
fn exit_reports_the_result_then_closed_then_exits_with_the_status() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "exit 7");
    assert_eq!(status(&result), 7);
    assert_eq!(text(get(&result, "termination")), "exited");

    let closed = session.json();
    expect_type(&closed, "closed");
    assert_eq!(int(get(&closed, "status")), 7);
    assert!(get(&closed, "id").is_null());

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(7));
    assert!(leftovers.is_empty());
}

#[test]
fn a_session_terminal_exit_writes_one_closed_event_and_no_ack() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "exit 3");
    assert_eq!(status(&result), 3);

    // No ack precedes the terminal event: this was an `exec`, not a `close`.
    let closed = session.json();
    expect_type(&closed, "closed");
    assert!(
        get(&closed, "id").is_null(),
        "a session that ends by itself uses the null id: {closed:?}"
    );
    assert_eq!(int(get(&closed, "status")), 3);

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(3));
    assert!(
        leftovers.is_empty(),
        "expected exactly one closed event: {leftovers:?}"
    );
}

#[test]
fn the_exec_builtin_is_refused_in_session_mode() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "exec true");
    assert_eq!(status(&result), 126);
    assert!(
        stderr(&result).contains("process replacement is not available in agent session mode"),
        "stderr: {:?}",
        stderr(&result)
    );
    // The session is still there, which is the whole point of the refusal.
    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    session.finish();
}

#[test]
fn the_exit_trap_runs_once_when_the_session_closes() {
    let dir = scratch("trap");
    let log = dir.join("exit-trap.log");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let install = exec(
        &mut session,
        "r1",
        &format!("trap 'echo bye >> {}' EXIT", log.display()),
    );
    assert_eq!(status(&install), 0);
    let other = exec(&mut session, "r2", "true");
    assert_eq!(status(&other), 0);
    assert!(!log.exists(), "the EXIT trap must not run between requests");

    session.send(r#"{"schema_version":1,"id":"r3","op":"close"}"#);
    let ack = session.json();
    expect_type(&ack, "ack");
    assert_eq!(text(get(&ack, "op")), "close");
    let closed = session.json();
    expect_type(&closed, "closed");
    assert_eq!(int(get(&closed, "status")), 0);

    let (exit, _, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert_eq!(
        std::fs::read_to_string(&log).expect("EXIT trap log"),
        "bye\n",
        "the EXIT action must run exactly once, at close"
    );
}

// ------------------------- §10.2: payload transport, binary data, truncation

#[test]
fn cat_receives_the_stdin_payload_and_the_next_frame_is_unaffected() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send(r#"{"schema_version":1,"id":"r1","op":"exec","command":"cat","stdin":"hello\n"}"#);
    let first = session.json();
    expect_type(&first, "result");
    assert_eq!(stdout(&first), "hello\n");
    assert_eq!(status(&first), 0);

    let second = exec(&mut session, "r2", "echo next");
    assert_eq!(stdout(&second), "next\n");
    session.finish();
}

#[test]
fn a_base64_payload_arrives_as_exact_bytes() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send(
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"cat","stdin":"AP8=","stdin_encoding":"base64"}"#,
    );
    let result = session.json();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "stdout_encoding")), "base64");
    assert_eq!(
        json::base64_decode(text(get(&result, "stdout"))).expect("valid base64"),
        vec![0x00, 0xff]
    );
    session.finish();
}

#[test]
fn binary_stdout_is_base64_with_the_exact_bytes() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "printf '\\x00\\xff'");
    assert_eq!(text(get(&result, "stdout_encoding")), "base64");
    assert_eq!(
        json::base64_decode(text(get(&result, "stdout"))).expect("valid base64"),
        vec![0x00, 0xff]
    );
    assert_eq!(int(get(&result, "stdout_bytes")), 2);
    session.finish();
}

#[test]
fn a_truncated_capture_reports_the_real_size() {
    let mut session = Session::spawn(&["--max-capture-bytes", "1024"]);
    let ready_frame = ready(&mut session);
    assert_eq!(
        int(get(get(&ready_frame, "protocol"), "limits")
            .get("max_capture_bytes")
            .unwrap()),
        1024
    );
    let result = exec(&mut session, "r1", "seq 1 200000");
    assert!(flag(get(&result, "stdout_truncated")));
    assert!(
        int(get(&result, "stdout_bytes")) > 1024,
        "the real size must survive truncation: {result:?}"
    );
    assert_eq!(stdout(&result).len(), 1024);
    session.finish();
}

#[test]
fn a_background_job_never_writes_into_the_protocol() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let started = exec(&mut session, "r1", "sleep 0.3 & echo go");
    assert_eq!(stdout(&started), "go\n");

    // The job keeps writing to the unlinked capture file of its request; the
    // protocol must stay silent for it.
    assert!(
        session.json_within(600).is_none(),
        "a background job produced a response line"
    );

    let after = exec(&mut session, "r2", "echo done");
    assert_eq!(stdout(&after), "done\n");

    session.send(r#"{"schema_version":1,"id":"r3","op":"close"}"#);
    expect_type(&session.json(), "ack");
    expect_type(&session.json(), "closed");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ------------------------------------------------- state and environment deltas

#[test]
fn state_reports_session_facts_and_gates_every_sensitive_section() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    exec(
        &mut session,
        "r1",
        "export SNAP_EXPORTED=1; readonly SNAP_RO=2; SNAP_PLAIN=3; \
         SNAP_LIST=(a); SNAP_LIST[2]=c; declare -A SNAP_MAP=([k]=v); \
         snapfn() { :; }; alias snapalias='echo hi'; trap 'echo t' USR1; \
         export SNAP_NOVAL",
    );

    // Default snapshot: facts only, no env, no variables.
    session.send(r#"{"schema_version":1,"id":"s0","op":"state"}"#);
    let plain = session.json();
    expect_type(&plain, "state");
    assert!(
        plain.get("env").is_none(),
        "env was not requested: {plain:?}"
    );
    assert!(plain.get("variables").is_none());
    assert!(array(get(&plain, "jobs")).is_empty());
    assert!(!flag(get(get(&plain, "options"), "errexit")));
    assert_eq!(int(get(&plain, "last_status")), 0);

    session.send(
        r#"{"schema_version":1,"id":"s1","op":"state","include":["env","variables","functions","aliases","traps"]}"#,
    );
    let full = session.json();
    expect_type(&full, "state");
    assert_eq!(int(get(&full, "schema_version")), 1);
    assert_eq!(int(get(&full, "pid")), i64::from(session.pid()));
    assert!(int(get(&full, "pid")) > 0);

    let env = get(&full, "env");
    assert_eq!(text(get(env, "SNAP_EXPORTED")), "1");

    let variables = get(&full, "variables");
    let scalars = get(variables, "scalars");
    let plain_var = get(scalars, "SNAP_PLAIN");
    assert_eq!(text(get(plain_var, "value")), "3");
    assert!(!flag(get(plain_var, "exported")));
    assert!(!flag(get(plain_var, "readonly")));
    assert!(flag(get(get(scalars, "SNAP_EXPORTED"), "exported")));
    assert!(flag(get(get(scalars, "SNAP_RO"), "readonly")));
    assert!(
        get(get(scalars, "SNAP_NOVAL"), "value").is_null(),
        "an exported name with no value must be null, not an empty string"
    );

    let list = array(get(get(variables, "indexed"), "SNAP_LIST"));
    assert_eq!(list.len(), 3);
    assert_eq!(text(&list[0]), "a");
    assert!(list[1].is_null(), "an unset element is a JSON null");
    assert_eq!(text(&list[2]), "c");
    assert_eq!(
        text(get(get(get(variables, "assoc"), "SNAP_MAP"), "k")),
        "v"
    );

    let functions = array(get(&full, "functions"))
        .iter()
        .map(text)
        .collect::<Vec<_>>();
    assert!(functions.contains(&"snapfn"), "functions: {functions:?}");
    assert_eq!(text(get(get(&full, "aliases"), "snapalias")), "echo hi");
    assert_eq!(text(get(get(&full, "traps"), "USR1")), "echo t");

    session.finish();
}

#[test]
fn state_reports_running_jobs_with_their_process_group() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let started = exec(&mut session, "r1", "sleep 0.4 &");
    assert_eq!(status(&started), 0);

    session.send(r#"{"schema_version":1,"id":"s1","op":"state"}"#);
    let state = session.json();
    expect_type(&state, "state");
    let jobs = array(get(&state, "jobs"));
    assert_eq!(jobs.len(), 1, "jobs: {jobs:?}");
    let job = &jobs[0];
    assert!(int(get(job, "id")) >= 1);
    assert!(int(get(job, "pgid")) > 0);
    assert!(!array(get(job, "pids")).is_empty());
    assert_eq!(text(get(job, "command")), "sleep 0.4");
    assert_eq!(text(get(job, "state")), "running");
    session.finish();
}

#[test]
fn env_delta_reports_changes_and_can_be_suppressed() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let added = exec(&mut session, "r1", "SNAP_DELTA=bar");
    let delta = get(&added, "env_delta");
    assert_eq!(text(get(delta, "SNAP_DELTA")), "bar");
    assert!(
        delta.get("_").is_none(),
        "the `_` variable changes on every command and is excluded: {delta:?}"
    );

    session.send(
        r#"{"schema_version":1,"id":"r2","op":"exec","command":"SNAP_DELTA=baz","env_delta":false}"#,
    );
    let quiet = session.json();
    expect_type(&quiet, "result");
    assert!(
        quiet.get("env_delta").is_none(),
        "env_delta:false must omit the field: {quiet:?}"
    );

    let removed = exec(&mut session, "r3", "unset SNAP_DELTA");
    assert!(
        get(get(&removed, "env_delta"), "SNAP_DELTA").is_null(),
        "a removed variable is null: {removed:?}"
    );
    session.finish();
}

// ------------------------------------------------------------- CLI rejections

#[test]
fn agent_without_stdio_is_a_usage_error() {
    let output = sharky(&["agent"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("a transport is required"));
}

#[test]
fn login_mode_is_refused() {
    for flag in ["--login", "-l"] {
        let output = sharky(&["agent", "--stdio", flag]);
        assert_eq!(output.status.code(), Some(2), "{flag}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("login mode"),
            "{flag}: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn unknown_agent_options_are_usage_errors() {
    let output = sharky(&["agent", "--stdio", "--unknown-flag"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected option"));
}

#[test]
fn agent_help_flags_print_the_agent_topic_and_exit_zero() {
    let topic = sharky(&["help", "agent"]);
    assert_eq!(topic.status.code(), Some(0));
    let topic_text = String::from_utf8_lossy(&topic.stdout).into_owned();
    assert!(
        topic_text.contains("sharky agent --stdio"),
        "help agent topic: {topic_text:?}"
    );

    for flag in ["--help", "-h"] {
        let output = sharky(&["agent", flag]);
        assert_eq!(output.status.code(), Some(0), "{flag}");
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(
            printed.contains("sharky agent --stdio"),
            "{flag}: {printed:?}"
        );
        assert_eq!(printed, topic_text, "{flag} must print the same topic text");
        assert!(
            output.stderr.is_empty(),
            "{flag}: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn a_terminal_on_standard_input_is_refused() {
    let pty = nix::pty::openpty(None, None).expect("open a pty");
    let output = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["agent", "--stdio"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::from(pty.slave))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run sharky with a tty stdin");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("terminal"),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(pty.master);
}

// --------------------------------- §10.1 row 8: errexit abort and empty requests

#[test]
fn errexit_abort_ends_the_session_after_the_result() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "set -e; false; echo unreachable");
    assert_eq!(status(&result), 1);
    assert_eq!(stdout(&result), "");

    let closed = session.json();
    expect_type(&closed, "closed");
    assert_eq!(int(get(&closed, "status")), 1);

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(1));
    assert!(leftovers.is_empty());
}

#[test]
fn an_empty_command_is_a_valid_noop() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "   ");
    assert_eq!(status(&result), 0);
    assert_eq!(int(get(&result, "duration_ms")), 0);
    assert_eq!(stdout(&result), "");
    assert_eq!(stderr(&result), "");
    // It is a result, not a protocol error, and the session keeps its state.
    assert_eq!(int(get(&result, "duration_us")), 0);
    exec(&mut session, "r2", "x=9");
    let after = exec(&mut session, "r3", "echo $x");
    assert_eq!(stdout(&after), "9\n");
    session.finish();
}

#[test]
fn every_result_carries_microsecond_resolution_alongside_duration_ms() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // A trivial command rounds duration_ms down to zero; duration_us must not.
    let trivial = exec(&mut session, "r1", "true");
    let ms = int(get(&trivial, "duration_ms"));
    let us = int(get(&trivial, "duration_us"));
    assert!(ms >= 0 && us >= 0, "durations must be non-negative");
    assert!(us >= ms * 1000, "{us}us < {ms}ms * 1000");

    // A 50 ms sleep proves the field has real sub-millisecond resolution.
    let slow = exec(&mut session, "r2", "sleep 0.05");
    let slow_ms = int(get(&slow, "duration_ms"));
    let slow_us = int(get(&slow, "duration_us"));
    assert!(
        slow_us >= 45_000,
        "duration_us is too coarse for profiling: {slow_us}"
    );
    assert!(slow_us >= slow_ms * 1000, "{slow_us}us < {slow_ms}ms * 1000");

    let closed = {
        session.send(r#"{"schema_version":1,"id":"c","op":"close"}"#);
        let ack = session.json();
        expect_type(&ack, "ack");
        session.json()
    };
    expect_type(&closed, "closed");
    session.finish();
}

// ------------------------------------- §10.2: the payload file cannot stall

#[test]
fn a_payload_larger_than_a_pipe_capacity_is_delivered_completely() {
    let payload = "a".repeat(200_000);
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send(&format!(
        r#"{{"schema_version":1,"id":"r1","op":"exec","command":"wc -c","stdin":{}}}"#,
        json::quote(&payload)
    ));
    let result = session.json();
    expect_type(&result, "result");
    assert_eq!(stdout(&result), "200000\n");
    assert_eq!(status(&result), 0);

    let after = exec(&mut session, "r2", "echo next");
    assert_eq!(stdout(&after), "next\n");
    session.finish();
}

// ------------------------------------- §3.6/§8.3: a client that stops reading

#[test]
fn a_client_that_stops_reading_takes_the_cleanup_path_instead_of_sigpipe() {
    use std::io::{BufRead, BufReader, Write};
    use std::time::{Duration, Instant};

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["agent", "--stdio"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky agent");
    let mut stdin = child.stdin.take().expect("agent stdin");
    let stdout = child.stdout.take().expect("agent stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read ready");
    assert!(line.contains("\"type\":\"ready\""), "{line}");
    // No reader is left on the response pipe: the next write gets EPIPE.
    drop(reader);

    // A concurrent `Command::spawn` in another test thread inherits this
    // process's descriptors between fork and exec, so for a few microseconds a
    // write can still find reading end. Each attempt therefore sends a fresh
    // request: once no transient holder exists, the result write hits EPIPE
    // and the agent must take the cleanup path instead of dying from SIGPIPE.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut attempt = 0;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the agent did not leave the cleanup path after EPIPE");
        }
        if attempt < 64 {
            let request = format!(
                "{{\"schema_version\":1,\"id\":\"epipe-{attempt}\",\"op\":\"exec\",\"command\":\"true\"}}\n"
            );
            let _ = stdin.write_all(request.as_bytes());
            let _ = stdin.flush();
            attempt += 1;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        status.code(),
        Some(0),
        "a broken pipe must not kill the agent: {status:?}"
    );
}

#[test]
fn a_parse_error_is_a_status_two_result_with_a_diagnostic() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let result = exec(&mut session, "r1", "if");
    assert_eq!(status(&result), 2);
    assert_eq!(stdout(&result), "");
    assert!(
        stderr(&result).starts_with("sharky: "),
        "stderr: {:?}",
        stderr(&result)
    );
    // No shell code from the request ran, and the session survives it.
    let after = exec(&mut session, "r2", "echo after");
    assert_eq!(stdout(&after), "after\n");
    session.finish();
}

// --------------------- §10.3: cancellation, deadlines, process-group cleanup

#[test]
fn cancel_stops_an_in_flight_command_and_the_session_survives() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&exec_request("r1", "sleep 10"));
    std::thread::sleep(Duration::from_millis(100));
    session.send(&cancel_request("c1", "r1", None));
    let ack = ack(&mut session, "cancel");
    assert_eq!(text(get(&ack, "id")), "c1");

    let started = Instant::now();
    let result = session.json();
    let elapsed = started.elapsed();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(text(get(&result, "termination")), "cancelled");
    assert_eq!(status(&result), 143);
    assert!(
        elapsed < Duration::from_millis(2000),
        "the cancel took {elapsed:?}"
    );

    // The session is still usable: cancellation is request-scoped, never a
    // signal to the agent's own group.
    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn a_term_ignoring_child_is_killed_after_the_grace() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // The ignored disposition survives the exec, so SIGTERM alone cannot end
    // either the shell or its sleep: only the escalation can.
    session.send(&exec_request("r1", r#"sh -c 'trap "" TERM; sleep 10'"#));
    std::thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    session.send(&cancel_request("c1", "r1", Some(200)));
    ack(&mut session, "cancel");
    // A second cancel for the same in-flight target is idempotent.
    session.send(&cancel_request("c2", "r1", Some(200)));
    ack(&mut session, "cancel");

    let result = session.json();
    let elapsed = started.elapsed();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "termination")), "cancelled");
    assert_eq!(status(&result), 143);
    assert!(
        elapsed >= Duration::from_millis(150),
        "SIGTERM should have been ignored before the escalation: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(3000),
        "escalation was not bounded by the grace: {elapsed:?}"
    );
    session.finish();
}

#[test]
fn timeout_ms_reports_timeout_and_status_124() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let started = Instant::now();
    session.send(&exec_with_timeout("r1", "sleep 5", 100));
    let result = session.json();
    let elapsed = started.elapsed();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "termination")), "timeout");
    assert_eq!(status(&result), 124);
    assert!(
        elapsed < Duration::from_millis(3000),
        "the deadline was not enforced: {elapsed:?}"
    );

    let after = exec(&mut session, "r2", "echo after");
    assert_eq!(stdout(&after), "after\n");
    session.finish();
}

#[test]
fn a_grandchild_dies_with_the_cancelled_group() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&exec_request("r1", "sh -c 'sleep 10 & echo $!; wait'"));
    std::thread::sleep(Duration::from_millis(100));
    session.send(&cancel_request("c1", "r1", None));
    ack(&mut session, "cancel");
    let result = session.json();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "termination")), "cancelled");

    let grandchild: i32 = stdout(&result)
        .lines()
        .next()
        .expect("the grandchild pid was printed")
        .trim()
        .parse()
        .expect("the grandchild pid is an integer");
    assert!(grandchild > 0);
    assert!(
        gone_within(grandchild, 3000),
        "the grandchild {grandchild} outlived the group cancel"
    );
    session.finish();
}

#[test]
fn close_answers_ack_then_exactly_one_closed_with_the_echoed_id() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(r#"{"schema_version":1,"id":"c1","op":"close"}"#);
    let ack = session.json();
    expect_type(&ack, "ack");
    assert_eq!(text(get(&ack, "id")), "c1");
    assert_eq!(text(get(&ack, "op")), "close");
    assert_eq!(text(get(&ack, "outcome")), "closed");

    let closed = session.json();
    expect_type(&closed, "closed");
    assert_eq!(
        text(get(&closed, "id")),
        "c1",
        "the closed event echoes the close request id"
    );
    assert_eq!(int(get(&closed, "status")), 0);

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(
        leftovers.is_empty(),
        "close wrote more than one terminal event: {leftovers:?}"
    );
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn close_kills_background_jobs_started_in_a_request() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let started = exec(&mut session, "r1", "sleep 30 & echo $!");
    let pid: i32 = stdout(&started)
        .trim()
        .parse()
        .expect("the job pid is an integer");
    assert!(pid > 0);

    session.send(r#"{"schema_version":1,"id":"c","op":"close","wait_ms":2000}"#);
    let close_ack = session.json();
    expect_type(&close_ack, "ack");
    assert_eq!(text(get(&close_ack, "op")), "close");
    assert_eq!(text(get(&close_ack, "outcome")), "closed");
    let closed = session.json();
    expect_type(&closed, "closed");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
    assert!(
        gone_within(pid, 3000),
        "close leaked the background job {pid}"
    );
}

#[test]
fn cancel_of_a_finished_or_unknown_request_is_a_typed_error() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    exec(&mut session, "r1", "true");

    session.send(&cancel_request("c1", "r1", None));
    let late = session.json();
    assert_eq!(error_code(&late), "CANCEL_TOO_LATE");
    assert!(!error_is_fatal(&late));

    session.send(&cancel_request("c2", "never-seen", None));
    let unknown = session.json();
    assert_eq!(error_code(&unknown), "CANCEL_UNKNOWN_ID");
    assert!(!error_is_fatal(&unknown));

    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    session.finish();
}

#[test]
fn an_idle_self_targeting_cancel_returns_cancel_unknown_id() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&cancel_request("c1", "c1", None));
    let response = session.json();
    assert_eq!(error_code(&response), "CANCEL_UNKNOWN_ID", "{response:?}");
    assert!(!error_is_fatal(&response));

    session.send(&cancel_request("c1", "other", None));
    let dup = session.json();
    assert_eq!(error_code(&dup), "DUPLICATE_ID", "{dup:?}");
    assert!(!error_is_fatal(&dup));

    let after = exec(&mut session, "r1", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    session.finish();
}

/// Extra `/dev/null` descriptors beyond isolated stdio in a live agent at the
/// ready frame. fd 0, 1 and 2 are `dup2` targets and must also point at null;
/// the shared setup owns exactly one additional null fd for those `dup2` calls
/// and future captures. A second one is a transport-startup leak.
fn extra_null_fds(pid: u32) -> Vec<i32> {
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
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let nulls = extra_null_fds(session.pid());
    assert_eq!(
        nulls.len(),
        1,
        "stdio startup must leave exactly one extra /dev/null descriptor: {nulls:?}"
    );

    session.finish();
}

/// Where `fd` of the agent process points, or `None` when it is closed.
///
/// The agent serves one request at a time and blocks on the control channel
/// between them, so the descriptor table of its idle process is a stable place
/// to observe what the per-request restore left behind.
fn fd_target(pid: u32, fd: i32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).ok()
}

#[test]
fn a_user_redirect_survives_across_requests_and_still_captures() {
    let dir = scratch("persistent-redirect");
    let log = dir.join("log");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let first = exec(
        &mut session,
        "redirect",
        &format!("exec > {}; echo first", log.display()),
    );
    assert_eq!(stdout(&first), "", "{first:?}");
    assert_eq!(
        fd_target(session.pid(), 1).as_deref(),
        Some(log.as_path()),
        "the user's redirect persists between requests"
    );

    // bash would send this line to the log, and the response still reports it.
    let second = exec(&mut session, "still-redirected", "echo second");
    assert_eq!(stdout(&second), "second\n", "{second:?}");

    let text = std::fs::read_to_string(&log).expect("read the user's log");
    assert_eq!(
        text, "first\nsecond\n",
        "the request's output must reach the destination the user chose"
    );

    let (status, _, errors) = session.finish();
    assert!(status.success(), "{status:?}");
    assert!(errors.is_empty(), "{errors}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn output_written_without_a_newline_stays_in_its_own_request() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let first = exec(&mut session, "no-newline", "echo -n pending");
    assert_eq!(stdout(&first), "pending", "{first:?}");

    let second = exec(&mut session, "next-request", "echo next");
    assert_eq!(
        stdout(&second),
        "next\n",
        "the previous request's bytes must not leak into this one: {second:?}"
    );

    let (status, _, errors) = session.finish();
    assert!(status.success(), "{status:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn merging_stderr_into_stdout_does_not_outlive_the_request() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // `exec 2>&1` points fd 2 at the request's stdout capture, which is
    // unlinked. Both streams answer in that response.
    let merged = exec(&mut session, "merge", "exec 2>&1; echo out; echo err >&2");
    assert_eq!(stdout(&merged), "out\nerr\n", "{merged:?}");
    assert_eq!(stderr(&merged), "", "{merged:?}");

    // The next request captures both streams again instead of writing into the
    // unlinked file the previous request left on fd 2.
    let next = exec(&mut session, "after-merge", "echo out2; echo err2 >&2");
    assert_eq!(stdout(&next), "out2\n", "{next:?}");
    assert_eq!(stderr(&next), "err2\n", "{next:?}");

    let (status, _, errors) = session.finish();
    assert!(status.success(), "{status:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn a_hundred_short_requests_leave_no_descriptor_or_zombie() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let fd_dir = format!("/proc/{}/fd", session.pid());
    let before = std::fs::read_dir(&fd_dir)
        .expect("the agent's descriptor table is readable")
        .count();

    for index in 0..100 {
        // An external `true` exercises the wait hook on every request; a
        // builtin would not fork and would not prove the fd accounting.
        let result = exec(&mut session, &format!("r{index}"), "command true");
        assert_eq!(status(&result), 0, "request {index}: {result:?}");
    }

    let after = std::fs::read_dir(&fd_dir)
        .expect("the agent's descriptor table is readable")
        .count();
    assert!(
        after <= before + 4,
        "descriptor growth: {before} -> {after}"
    );

    let children = format!("/proc/{}/task/{}/children", session.pid(), session.pid());
    if let Ok(list) = std::fs::read_to_string(&children) {
        for pid in list.split_whitespace().filter_map(|pid| pid.parse::<i32>().ok()) {
            assert!(
                process_gone(pid),
                "zombie child {pid} was left behind: {list:?}"
            );
        }
    }

    let alive = exec(&mut session, "r100", "echo alive");
    assert_eq!(stdout(&alive), "alive\n");
    session.finish();
}

#[test]
fn an_unwritable_persistent_redirect_does_not_crash_the_session() {
    let dir = scratch("unwritable-redirect");
    let ro = dir.join("readonly.txt");
    std::fs::write(&ro, "seed\n").expect("write readonly file");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // `exec 1<file` opens fd 1 read-only persistently across requests.
    let redir = exec(&mut session, "r1", &format!("exec 1<{}", ro.display()));
    assert_eq!(status(&redir), 0, "{redir:?}");

    // Replay into the read-only fd 1 gets EBADF, which must be tolerated instead
    // of tearing down the session with a fatal INTERNAL error.
    let second = exec(&mut session, "r2", "echo hi");
    expect_type(&second, "result");
    assert_eq!(status(&second), 0, "{second:?}");
    assert_eq!(stdout(&second), "hi\n", "{second:?}");

    let alive = exec(&mut session, "r3", "echo alive");
    expect_type(&alive, "result");
    assert_eq!(stdout(&alive), "alive\n", "{alive:?}");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}
