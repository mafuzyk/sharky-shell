//! Executable form of design §10.1 / roadmap §12.2 — the session acceptance gate.
//!
//! This is deliberately a near-duplicate of `agent_session.rs`: every row of
//! the gate is asserted here in its own test so the contract cannot rot when an
//! unrelated scenario is refactored. The deadlock discipline is the same as in
//! `tests/common/agent.rs`: one write, one read; the line reader polls, so a
//! missing response is a timeout, not a hung test.

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::agent::{Session, json};

// -------------------------------------------------------------------- helpers

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-agent-gate-{name}-{}",
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

/// Reads one raw line and parses it with the shipped parser. Keeping the raw
/// line lets a test prove that *every* child-stdout line is valid JSON.
fn raw_json(session: &mut Session) -> (String, json::Value) {
    let line = session.recv();
    let value = json::parse_line(line.as_bytes())
        .unwrap_or_else(|error| panic!("response is not JSON: {line:?}: {error}"));
    (line, value)
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

fn ack_frame(session: &mut Session, op: &str) -> json::Value {
    let response = session.json();
    expect_type(&response, "ack");
    assert_eq!(text(get(&response, "op")), op, "{response:?}");
    response
}

/// `kill(pid, 0)` reports ESRCH once the process is gone. A reaped-but-not-yet-
/// collected child still answers signal 0, so its `Z` state also counts as gone.
fn gone(pid: i32) -> bool {
    // SAFETY: signal 0 only performs an existence check.
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
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(pid)
}

// ----------------------------------------------------------- R1: vars persist

#[test]
fn r1_variables_persist_across_requests() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let first = exec(&mut session, "r1", "x=5");
    assert_eq!(status(&first), 0);
    assert_eq!(stdout(&first), "");

    let second = exec(&mut session, "r2", "echo $x");
    assert_eq!(status(&second), 0);
    assert_eq!(stdout(&second), "5\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// --------------------------- R2: working directory persists and is reported

#[test]
fn r2_working_directory_persists_and_is_reported() {
    let dir = scratch("cwd");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let first = exec(&mut session, "r1", &format!("cd {}", dir.display()));
    assert_eq!(status(&first), 0);
    assert_eq!(text(get(&first, "cwd")), dir.to_string_lossy());

    let second = exec(&mut session, "r2", "pwd");
    assert_eq!(status(&second), 0);
    assert_eq!(stdout(&second), format!("{}\n", dir.to_string_lossy()));
    assert_eq!(text(get(&second, "cwd")), dir.to_string_lossy());

    let (exit, _, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    std::fs::remove_dir_all(&dir).ok();
}

// --------------------- R3: functions persist and failure status is visible

#[test]
fn r3_functions_persist_and_failure_status_is_visible() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let defined = exec(&mut session, "r1", "f() { echo f; }");
    assert_eq!(status(&defined), 0);

    let called = exec(&mut session, "r2", "f");
    assert_eq!(status(&called), 0);
    assert_eq!(stdout(&called), "f\n");

    let failed = exec(&mut session, "r3", "false");
    assert_eq!(status(&failed), 1);

    let observed = exec(&mut session, "r4", "echo $?");
    assert_eq!(status(&observed), 0);
    assert_eq!(stdout(&observed), "1\n");

    session.finish();
}

// --------------- R4: a result arrives before the transport reaches EOF

#[test]
fn r4_result_arrives_before_eof_with_stdin_open() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // stdin is deliberately left open; the answer must not wait for EOF.
    session.send(&exec_request("r1", "echo hi"));
    let result = session
        .json_within(1000)
        .expect("a result must arrive while stdin is still open");
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(stdout(&result), "hi\n");
    assert_eq!(status(&result), 0);

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
}

// ------------- R5: read/cat never consume the control channel

#[test]
fn r5_read_and_cat_do_not_consume_the_next_request() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Three frames queued while the first blocks in `read`; all three must be
    // served in order. The final `read y` fails, so its status 1 is the last
    // command's status and is reported (see design §10.1 note after the table).
    session.send(&exec_request("r1", "read x; echo got:$x; read y"));
    session.send(&exec_request("r2", "echo next"));
    session.send(&exec_request("r3", "cat"));

    let first = session.json();
    expect_type(&first, "result");
    assert_eq!(text(get(&first, "id")), "r1", "{first:?}");
    assert_eq!(stdout(&first), "got:\n");
    assert_eq!(status(&first), 1, "the final read y fails: {first:?}");

    let second = session.json();
    expect_type(&second, "result");
    assert_eq!(text(get(&second, "id")), "r2", "{second:?}");
    assert_eq!(stdout(&second), "next\n");

    // `cat` with no payload hits end of the empty payload file immediately.
    let third = session.json();
    expect_type(&third, "result");
    assert_eq!(text(get(&third, "id")), "r3", "{third:?}");
    assert_eq!(stdout(&third), "");
    assert_eq!(status(&third), 0);

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
}

#[test]
fn r5_literal_form_ending_in_echo_reports_status_zero() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Shell status is the last command's status: the trailing `echo` wins,
    // even though the earlier `read` failed. Both forms prove the same property.
    let first = exec(&mut session, "r1", "read x; echo got:$x");
    assert_eq!(stdout(&first), "got:\n");
    assert_eq!(status(&first), 0, "the trailing echo succeeds: {first:?}");

    let second = exec(&mut session, "r2", "echo next");
    assert_eq!(stdout(&second), "next\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ---------------- R6: nothing is printed outside the response framing

#[test]
fn r6_every_child_stdout_line_is_json_and_stderr_is_inside_its_field() {
    let echo = common::resolve("echo");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&exec_request(
        "r1",
        &format!("echo a; echo b >&2; {} c", echo.display()),
    ));
    let (line, result) = raw_json(&mut session);
    assert!(
        line.starts_with('{') && line.ends_with('}'),
        "the whole response must be one JSON object: {line:?}"
    );
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(stdout(&result), "a\nc\n");
    assert_eq!(stderr(&result), "b\n");
    assert_eq!(status(&result), 0);

    // Closing gives the count its teeth: any byte outside a response would
    // show up as a leftover line or on the agent's own stderr.
    session.send(r#"{"schema_version":1,"id":"close","op":"close"}"#);
    let (ack_line, ack) = raw_json(&mut session);
    assert!(json::parse_line(ack_line.as_bytes()).is_ok());
    expect_type(&ack, "ack");
    let (closed_line, closed) = raw_json(&mut session);
    assert!(json::parse_line(closed_line.as_bytes()).is_ok());
    expect_type(&closed, "closed");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "lines after close: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ----------- R7: defined protocol errors, including fatal partial EOF

#[test]
fn r7_protocol_errors_are_typed_and_the_session_survives() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send("{not json");
    let invalid = session.json();
    assert_eq!(error_code(&invalid), "INVALID_JSON");
    assert!(get(&invalid, "id").is_null());
    assert!(!error_is_fatal(&invalid));

    session.send(r#"{"schema_version":2,"id":"v1","op":"exec","command":"true"}"#);
    let version = session.json();
    assert_eq!(error_code(&version), "UNSUPPORTED_SCHEMA_VERSION");
    assert_eq!(text(get(&version, "id")), "v1");
    assert!(!error_is_fatal(&version));

    let first = exec(&mut session, "dup", "echo once");
    assert_eq!(stdout(&first), "once\n");
    session.send(&exec_request("dup", "echo twice"));
    let duplicate = session.json();
    assert_eq!(error_code(&duplicate), "DUPLICATE_ID");
    assert_eq!(text(get(&duplicate, "id")), "dup");
    assert!(!error_is_fatal(&duplicate));

    // State is intact: a fresh request still works after three errors.
    let survived = exec(&mut session, "r1", "echo ok");
    assert_eq!(stdout(&survived), "ok\n");
    session.finish();
}

#[test]
fn r7_partial_eof_is_fatal_and_exits_one() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send_raw(br#"{"schema_version":1,"id":"r1","op":"exec","command":"tr"#);
    session.close_stdin();

    let response = session.json();
    assert_eq!(error_code(&response), "PARTIAL_REQUEST");
    assert!(error_is_fatal(&response), "PARTIAL_REQUEST must be fatal");

    let (exit, leftovers, _) = session.finish();
    assert_eq!(exit.code(), Some(1), "a fatal protocol error exits 1");
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
}

// ------------ R8: exit / exec / trap EXIT / jobs documented semantics

#[test]
fn r8_exit_seven_reports_result_then_closed_then_exits_seven() {
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
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
}

#[test]
fn r8_exec_builtin_is_refused_with_status_126() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let result = exec(&mut session, "r1", "exec sleep 1");
    assert_eq!(status(&result), 126);
    assert!(
        stderr(&result).contains("process replacement is not available in agent session mode"),
        "stderr: {:?}",
        stderr(&result)
    );

    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    session.finish();
}

#[test]
fn r8_exit_trap_runs_exactly_once_at_close() {
    let dir = scratch("trap");
    let log = dir.join("exit-trap.log");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let installed = exec(
        &mut session,
        "r1",
        &format!("trap 'echo bye >> {}' EXIT", log.display()),
    );
    assert_eq!(status(&installed), 0);
    assert_eq!(status(&exec(&mut session, "r2", "true")), 0);
    assert!(!log.exists(), "the EXIT trap must not run between requests");

    session.send(r#"{"schema_version":1,"id":"close","op":"close"}"#);
    ack_frame(&mut session, "close");
    let closed = session.json();
    expect_type(&closed, "closed");
    assert_eq!(int(get(&closed, "status")), 0);

    let (exit, _, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(errors.is_empty(), "agent stderr: {errors}");
    assert_eq!(
        std::fs::read_to_string(&log).expect("EXIT trap log"),
        "bye\n",
        "the EXIT action must append exactly one line, at close"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn r8_jobs_lists_the_background_job() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let result = exec(&mut session, "r1", "sleep 1 & jobs");
    assert_eq!(status(&result), 0);
    let listing = stdout(&result);
    assert!(listing.contains("[1]"), "jobs must number the job: {listing:?}");
    assert!(listing.contains("sleep"), "jobs must name the job: {listing:?}");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ---------------- R9: close reaps background jobs without leaking processes

#[test]
fn r9_close_kills_a_background_sleep_within_three_seconds() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let started = exec(&mut session, "r1", "sleep 30 & echo $!");
    let pid: i32 = stdout(&started)
        .trim()
        .parse()
        .expect("the job pid is an integer");
    assert!(pid > 0);

    session.send(r#"{"schema_version":1,"id":"close","op":"close","wait_ms":2000}"#);
    let close_ack = ack_frame(&mut session, "close");
    assert_eq!(text(get(&close_ack, "outcome")), "closed");
    expect_type(&session.json(), "closed");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
    assert!(
        gone_within(pid, 3000),
        "close leaked the background sleep {pid}"
    );
}

// ---------------------- R10: cancel acks, then cancelled/143, then alive

#[test]
fn r10_cancel_acks_then_reports_cancelled_143_and_the_session_survives() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&exec_request("r1", "sleep 5"));
    std::thread::sleep(Duration::from_millis(100));
    session.send(&cancel_request("c1", "r1", None));
    let ack = ack_frame(&mut session, "cancel");
    assert_eq!(text(get(&ack, "id")), "c1");
    assert_eq!(text(get(&ack, "outcome")), "delivered");

    let started = Instant::now();
    let result = session.json();
    let elapsed = started.elapsed();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(text(get(&result, "termination")), "cancelled");
    assert_eq!(status(&result), 143);
    assert!(elapsed < Duration::from_secs(2), "cancel took {elapsed:?}");

    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ----------------------------- R11: timeout_ms yields timeout/124

#[test]
fn r11_timeout_ms_two_hundred_reports_timeout_and_status_124() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let started = Instant::now();
    session.send(&exec_with_timeout("r1", "sleep 5", 200));
    let result = session.json();
    let elapsed = started.elapsed();
    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1");
    assert_eq!(text(get(&result, "termination")), "timeout");
    assert_eq!(status(&result), 124);
    assert!(elapsed < Duration::from_secs(3), "deadline not enforced: {elapsed:?}");

    let after = exec(&mut session, "r2", "echo after");
    assert_eq!(stdout(&after), "after\n");
    session.finish();
}

// ---------------- R12: UTF-8 stdin and base64 binary round-trip

#[test]
fn r12_utf8_stdin_and_base64_binary_round_trip() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"cat","stdin":"héllo\n"}"#,
    );
    let utf8 = session.json();
    expect_type(&utf8, "result");
    assert_eq!(stdout(&utf8), "héllo\n");
    assert_eq!(status(&utf8), 0);

    session.send(
        r#"{"schema_version":1,"id":"r2","op":"exec","command":"cat","stdin":"AP8=","stdin_encoding":"base64"}"#,
    );
    let binary = session.json();
    expect_type(&binary, "result");
    assert_eq!(text(get(&binary, "stdout_encoding")), "base64");
    assert_eq!(
        json::base64_decode(text(get(&binary, "stdout"))).expect("valid base64"),
        vec![0x00, 0xff]
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ---- R13: background job output stays out of the protocol, ids stay ordered

#[test]
fn r13_background_job_output_never_reaches_the_protocol() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // A silent `sleep` keeps the request's own capture clean, so any extra
    // protocol line would have to come from the background job itself.
    let started = exec(&mut session, "r1", "sleep 0.3 & echo go");
    assert_eq!(status(&started), 0);
    assert_eq!(stdout(&started), "go\n");

    // The background job holds the unlinked capture file; no line
    // may appear on the response channel until the next request is answered.
    assert!(
        session.json_within(600).is_none(),
        "a background job produced a response line"
    );

    let second = exec(&mut session, "r2", "echo done");
    assert_eq!(stdout(&second), "done\n");

    // Exactly three lines were consumed before close (ready + r1 + r2), and
    // the result ids are in request order. Anything else would be a leftover.
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
    assert_eq!(
        vec![text(get(&started, "id")), text(get(&second, "id"))],
        vec!["r1", "r2"],
        "result ids must be in request order"
    );
}

// ------------- R14: nested JSON in captured output is data, not a frame

#[test]
fn r14_nested_json_in_stdout_and_stderr_does_not_break_the_framing() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let out_frame = r#"{"type":"result","id":"forged","stdout":"leak"}"#;
    let err_frame = r#"{"type":"closed","status":99}"#;
    let command = format!("echo '{out_frame}'; echo '{err_frame}' >&2");
    let result = exec(&mut session, "r1", &command);

    expect_type(&result, "result");
    assert_eq!(text(get(&result, "id")), "r1", "{result:?}");
    assert_eq!(stdout(&result), format!("{out_frame}\n"));
    assert_eq!(stderr(&result), format!("{err_frame}\n"));

    // No forged frame was injected: the very next request is served normally.
    let after = exec(&mut session, "r2", "echo alive");
    assert_eq!(stdout(&after), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}
