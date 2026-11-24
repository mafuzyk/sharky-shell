//! Integration tests for Phase F (Native Agent Code Mode) in `sharky agent --stdio`.
//!
//! Each test spawns the real compiled `sharky` binary over pipes using
//! `common::agent::Session` and asserts exact wire contracts.

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use common::agent::{Session, json};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("sharky-codemode-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

fn ready(session: &mut Session) -> json::Value {
    let frame = session.json();
    assert_eq!(frame.get("type").and_then(json::Value::as_str), Some("ready"));
    frame
}

fn send_json(session: &mut Session, line: &str) -> json::Value {
    session.send(line);
    session.json()
}

fn text<'a>(value: &'a json::Value, key: &str) -> &'a str {
    value
        .get(key)
        .and_then(json::Value::as_str)
        .unwrap_or_else(|| panic!("expected string key {key:?} in {value:?}"))
}

fn int(value: &json::Value, key: &str) -> i64 {
    value
        .get(key)
        .and_then(json::Value::as_i64)
        .unwrap_or_else(|| panic!("expected integer key {key:?} in {value:?}"))
}

#[test]
fn argv_passes_literal_metacharacters_without_shell_interpolation() {
    let mut session = Session::spawn(&["--no-config"]);
    let ready_frame = ready(&mut session);
    let features = ready_frame
        .get("protocol")
        .and_then(|p| p.get("features"))
        .and_then(json::Value::as_array)
        .expect("features array");
    assert!(
        features.iter().any(|f| f.as_str() == Some("argv")),
        "missing argv feature: {features:?}"
    );

    // Run typed argv on `exec` with shell metacharacters and whitespace.
    let res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"e1","op":"exec","argv":["printf","%s|%s|%s|%s|%s\n","$HOME","$(echo pwned)","`uname`","a b; echo injected","-n"]}"#,
    );
    assert_eq!(res.get("type").and_then(json::Value::as_str), Some("result"), "{res:?}");
    assert_eq!(int(&res, "status"), 0, "{res:?}");
    assert_eq!(text(&res, "termination"), "exited");
    assert_eq!(
        text(&res, "stdout"),
        "$HOME|$(echo pwned)|`uname`|a b; echo injected|-n\n"
    );

    // Builtins must NOT be invoked by `argv`: `cd` is a shell builtin, so
    // looking it up externally returns 127.
    let builtin_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"e2","op":"exec","argv":["cd","/tmp"]}"#,
    );
    assert_eq!(int(&builtin_res, "status"), 127, "{builtin_res:?}");

    // Same typed argv contract in `parallel` and `workflow`.
    let par = session.parallel(
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","argv":["printf","%s","$NO_EXPAND"]}]}"#,
    );
    assert_eq!(int(&par, "status"), 0, "{par:?}");
    let t1 = &par.get("tasks").and_then(json::Value::as_array).unwrap()[0];
    assert_eq!(text(t1, "stdout"), "$NO_EXPAND");

    let wf = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"w1","op":"workflow","tasks":[{"id":"n1","argv":["printf","%s","$(echo nope)"]},{"id":"n2","depends_on":["n1"],"condition":{"task":"n1","stdout_equals":"$(echo nope)"},"argv":["printf","ok"]}]}"#,
    );
    assert_eq!(wf.get("type").and_then(json::Value::as_str), Some("workflow"), "{wf:?}");
    assert_eq!(int(&wf, "status"), 0, "{wf:?}");
    let n2 = &wf.get("tasks").and_then(json::Value::as_array).unwrap()[1];
    assert_eq!(text(n2, "stdout"), "ok");

    let alive = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"e-done","op":"exec","argv":["true"]}"#,
    );
    assert_eq!(int(&alive, "status"), 0, "{alive:?}");

    let (status, leftovers, errors) = session.finish();
    assert!(status.success(), "leftovers={leftovers:?} errors={errors:?}");
    assert!(leftovers.is_empty());
    assert!(errors.is_empty());
}

#[test]
fn argv_validation_and_exec_error_statuses() {
    let dir = temp_dir("argv-err");
    let non_exec = dir.join("not-exec.txt");
    fs::write(&non_exec, "#!/bin/sh\necho hi\n").expect("write non-exec file");
    let mut perms = fs::metadata(&non_exec).unwrap().permissions();
    perms.set_mode(0o644);
    fs::set_permissions(&non_exec, perms).unwrap();

    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    for (idx, bad) in [
        r#"{"schema_version":1,"id":"b1","op":"exec","command":"echo a","argv":["echo","b"]}"#,
        r#"{"schema_version":1,"id":"b2","op":"exec"}"#,
        r#"{"schema_version":1,"id":"b3","op":"exec","argv":[]}"#,
        r#"{"schema_version":1,"id":"b4","op":"exec","argv":[""]}"#,
        r#"{"schema_version":1,"id":"b5","op":"exec","argv":["echo",42]}"#,
        r#"{"schema_version":1,"id":"b6","op":"exec","argv":["echo","a\u0000b"]}"#,
        r#"{"schema_version":1,"id":"b7","op":"parallel","tasks":[{"id":"t1","command":"true","argv":["true"]}]}"#,
        r#"{"schema_version":1,"id":"b8","op":"workflow","tasks":[{"id":"t1","argv":[]}]}"#,
    ]
    .iter()
    .enumerate()
    {
        let err = send_json(&mut session, bad);
        assert_eq!(
            err.get("type").and_then(json::Value::as_str),
            Some("error"),
            "case {idx}: {bad} -> {err:?}"
        );
        assert_eq!(
            text(err.get("error").unwrap(), "code"),
            "INVALID_REQUEST",
            "case {idx}: {bad} -> {err:?}"
        );
    }

    let enoent = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"m1","op":"exec","argv":["definitely_missing_sharky_bin_xyz"]}"#,
    );
    assert_eq!(int(&enoent, "status"), 127, "{enoent:?}");

    let eacces_req = format!(
        r#"{{"schema_version":1,"id":"m2","op":"exec","argv":[{}]}}"#,
        json::Value::String(non_exec.to_string_lossy().into_owned()).encode()
    );
    let eacces = send_json(&mut session, &eacces_req);
    assert_eq!(int(&eacces, "status"), 126, "{eacces:?}");

    let alive = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"m-done","op":"exec","command":"true"}"#,
    );
    assert_eq!(int(&alive, "status"), 0, "{alive:?}");

    let _ = fs::remove_dir_all(&dir);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn argv_honors_stdin_timeout_and_cancel() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let cat = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"c1","op":"exec","argv":["cat"],"stdin":"hello-argv\n"}"#,
    );
    assert_eq!(int(&cat, "status"), 0);
    assert_eq!(text(&cat, "stdout"), "hello-argv\n");

    let timed = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"c2","op":"exec","argv":["sh","-c","trap '' TERM; sleep 30"],"timeout_ms":80}"#,
    );
    assert_eq!(int(&timed, "status"), 124, "{timed:?}");
    assert_eq!(text(&timed, "termination"), "timeout");

    session.send(r#"{"schema_version":1,"id":"c3","op":"exec","argv":["sleep","30"]}"#);
    std::thread::sleep(std::time::Duration::from_millis(40));
    session.send(r#"{"schema_version":1,"id":"k1","op":"cancel","target":"c3","grace_ms":50}"#);
    let first = session.json();
    let second = session.json();
    let (ack, cancelled) = if first.get("type").and_then(json::Value::as_str) == Some("ack") {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(text(&ack, "outcome"), "delivered");
    assert_eq!(text(&cancelled, "termination"), "cancelled");

    let alive = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"c-done","op":"exec","command":"true"}"#,
    );
    assert_eq!(int(&alive, "status"), 0, "{alive:?}");

    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn exec_isolated_preserves_parent_session_state_and_handles_overrides() {
    let dir = temp_dir("iso");
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    let dir_str = dir.to_string_lossy().into_owned();
    let sub_str = sub.to_string_lossy().into_owned();

    let mut session = Session::spawn(&["--no-config"]);
    let ready_frame = ready(&mut session);
    let features = ready_frame
        .get("protocol")
        .and_then(|p| p.get("features"))
        .and_then(json::Value::as_array)
        .expect("features");
    assert!(
        features.iter().any(|f| f.as_str() == Some("exec_isolated")),
        "missing exec_isolated feature: {features:?}"
    );

    // Reject cwd/env without isolated: true.
    let bad = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"bad1","op":"exec","command":"pwd","cwd":"/"}"#,
    );
    assert_eq!(
        text(bad.get("error").unwrap(), "code"),
        "INVALID_REQUEST",
        "{bad:?}"
    );

    // Establish parent state: cwd = dir, EXPORTED_VAR = parent, last_status = 7.
    let setup = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"s1","op":"exec","command":"cd {} && export EXPORTED_VAR=parent && (exit 7)"}}"#,
            json::Value::String(dir_str.clone()).encode().trim_matches('"')
        ),
    );
    assert_eq!(int(&setup, "status"), 7);

    // Isolated execution mutates everything and exits 9: parent must stay intact.
    let iso = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"i1","op":"exec","isolated":true,"command":"cd /; export LEAK=from_iso EXPORTED_VAR=mutated; set -e; exit 9"}"#,
    );
    assert_eq!(int(&iso, "status"), 9, "{iso:?}");
    assert_eq!(text(&iso, "termination"), "exited");
    assert_eq!(text(&iso, "cwd"), "/");
    let delta = iso.get("env_delta").unwrap();
    assert_eq!(text(delta, "LEAK"), "from_iso");
    assert_eq!(text(delta, "EXPORTED_VAR"), "mutated");

    // Verify parent session still has cwd == dir, last_status == 7, errexit == false,
    // EXPORTED_VAR == "parent", and no LEAK.
    let state = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"st1","op":"state","include":["env"]}"#,
    );
    assert_eq!(text(&state, "cwd"), dir_str);
    assert_eq!(int(&state, "last_status"), 7);
    assert_eq!(
        state.get("options").unwrap().get("errexit").and_then(json::Value::as_bool),
        Some(false)
    );
    let env_map = state.get("env").unwrap();
    assert_eq!(text(env_map, "EXPORTED_VAR"), "parent");
    assert!(env_map.get("LEAK").is_none());

    // Isolated exec with cwd + env overrides (including unset) and argv.
    let ov_req = format!(
        r#"{{"schema_version":1,"id":"i2","op":"exec","isolated":true,"cwd":{},"env":{{"CUSTOM":"val","EXPORTED_VAR":null}},"argv":["sh","-c","pwd; printf '%s|%s\n' \"$CUSTOM\" \"${{EXPORTED_VAR:-unset}}\""]}}"#,
        json::Value::String(sub_str.clone()).encode()
    );
    let ov = send_json(&mut session, &ov_req);
    assert_eq!(int(&ov, "status"), 0, "{ov:?}");
    assert_eq!(text(&ov, "cwd"), sub_str);
    assert_eq!(text(&ov, "stdout"), format!("{sub_str}\nval|unset\n"));

    // Bad cwd inside isolated exec -> 126 + spawn_error without killing session.
    let bad_cwd = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"i3","op":"exec","isolated":true,"cwd":"/nonexistent/sharky-iso-missing-dir","command":"true"}"#,
    );
    assert_eq!(int(&bad_cwd, "status"), 126, "{bad_cwd:?}");
    assert_eq!(text(&bad_cwd, "termination"), "spawn_error");

    // Timeout works on isolated exec.
    let timed = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"i4","op":"exec","isolated":true,"command":"sleep 30","timeout_ms":60}"#,
    );
    assert_eq!(int(&timed, "status"), 124, "{timed:?}");
    assert_eq!(text(&timed, "termination"), "timeout");

    // Session is still completely healthy after all isolated runs.
    let after = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"ok1","op":"exec","command":"pwd; echo \"$EXPORTED_VAR\""}"#,
    );
    assert_eq!(text(&after, "stdout"), format!("{dir_str}\nparent\n"));

    let _ = fs::remove_dir_all(&dir);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn redact_env_masks_sensitive_vars_across_ops() {
    let mut session = Session::spawn(&["--no-config", "--redact-env"]);
    let ready_frame = ready(&mut session);
    let features = ready_frame
        .get("protocol")
        .and_then(|p| p.get("features"))
        .and_then(json::Value::as_array)
        .expect("features");
    assert!(
        features.iter().any(|f| f.as_str() == Some("env_redaction")),
        "missing env_redaction feature: {features:?}"
    );

    let set_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"r1","op":"exec","command":"export GITHUB_TOKEN=ghp_123 MY_API_KEY=k_456 NORMAL_VAR=visible; SECRET_ARR=(s1 '' s2); declare -A META=([api_key]=sec [name]=public); readonly SECRET_UNSET"}"#,
    );
    assert_eq!(int(&set_res, "status"), 0, "{set_res:?}");
    let delta = set_res.get("env_delta").unwrap();
    assert_eq!(text(delta, "GITHUB_TOKEN"), "<redacted>");
    assert_eq!(text(delta, "MY_API_KEY"), "<redacted>");
    assert_eq!(text(delta, "NORMAL_VAR"), "visible");

    let state = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"r2","op":"state","include":["env","variables"]}"#,
    );
    let env_map = state.get("env").unwrap();
    assert_eq!(text(env_map, "GITHUB_TOKEN"), "<redacted>");
    assert_eq!(text(env_map, "MY_API_KEY"), "<redacted>");
    assert_eq!(text(env_map, "NORMAL_VAR"), "visible");

    let vars = state.get("variables").unwrap();
    let scalars = vars.get("scalars").unwrap();
    assert_eq!(text(scalars.get("GITHUB_TOKEN").unwrap(), "value"), "<redacted>");
    assert!(scalars.get("SECRET_UNSET").unwrap().get("value").unwrap().is_null());
    assert_eq!(text(scalars.get("NORMAL_VAR").unwrap(), "value"), "visible");

    let indexed = vars.get("indexed").unwrap().get("SECRET_ARR").unwrap().as_array().unwrap();
    assert_eq!(indexed[0], json::Value::String("<redacted>".to_string()));
    assert_eq!(indexed[2], json::Value::String("<redacted>".to_string()));

    let assoc = vars.get("assoc").unwrap().get("META").unwrap();
    assert_eq!(text(assoc, "api_key"), "<redacted>");
    assert_eq!(text(assoc, "name"), "public");

    // Unset keeps null in env_delta.
    let unset_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"r3","op":"exec","command":"unset GITHUB_TOKEN"}"#,
    );
    assert!(unset_res.get("env_delta").unwrap().get("GITHUB_TOKEN").unwrap().is_null());

    // Isolated exec, parallel, and workflow redact sensitive env_delta entries too.
    let iso = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"r4","op":"exec","isolated":true,"command":"export DB_PASSWORD=pw OK_VAL=yes"}"#,
    );
    let iso_delta = iso.get("env_delta").unwrap();
    assert_eq!(text(iso_delta, "DB_PASSWORD"), "<redacted>");
    assert_eq!(text(iso_delta, "OK_VAL"), "yes");

    let par = session.parallel(
        r#"{"schema_version":1,"id":"r5","op":"parallel","tasks":[{"id":"t1","command":"export APP_SECRET=topsecret SAFE=ok","env_delta":true}]}"#,
    );
    let p_delta = par.get("tasks").unwrap().as_array().unwrap()[0].get("env_delta").unwrap();
    assert_eq!(text(p_delta, "APP_SECRET"), "<redacted>");
    assert_eq!(text(p_delta, "SAFE"), "ok");

    let wf = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"r6","op":"workflow","tasks":[{"id":"n1","command":"export AUTH_HEADER=bearer SAFE2=ok2","env_delta":true}]}"#,
    );
    let w_delta = wf.get("tasks").unwrap().as_array().unwrap()[0].get("env_delta").unwrap();
    assert_eq!(text(w_delta, "AUTH_HEADER"), "<redacted>");
    assert_eq!(text(w_delta, "SAFE2"), "ok2");

    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn output_projection_filters_ansi_grep_head_tail_and_preserves_failure_and_capture_id() {
    let mut session = Session::spawn(&["--no-config"]);
    let ready_frame = ready(&mut session);
    let protocol = ready_frame.get("protocol").unwrap();
    let ops = protocol.get("ops").and_then(json::Value::as_array).unwrap();
    let features = protocol.get("features").and_then(json::Value::as_array).unwrap();
    let limits = protocol.get("limits").unwrap();
    assert!(ops.iter().any(|o| o.as_str() == Some("read_capture")));
    assert!(features.iter().any(|f| f.as_str() == Some("output_projection")));
    assert!(features.iter().any(|f| f.as_str() == Some("capture_store")));
    assert_eq!(int(limits, "max_capture_store_entries"), 32);
    assert_eq!(int(limits, "max_capture_store_bytes"), 16 * 1024 * 1024);

    // Validation rejections are non-fatal INVALID_REQUEST.
    for bad in [
        r#"{"schema_version":1,"id":"bad-re","op":"exec","command":"true","output":{"grep":"["}}"#,
        r#"{"schema_version":1,"id":"bad-ctx","op":"exec","command":"true","output":{"context":2}}"#,
        r#"{"schema_version":1,"id":"bad-rc","op":"read_capture","capture_id":"cap-1","byte_offset":0,"line_start":1}"#,
    ] {
        let err = send_json(&mut session, bad);
        assert_eq!(
            text(err.get("error").unwrap(), "code"),
            "INVALID_REQUEST",
            "{bad} -> {err:?}"
        );
    }

    // Failing command with ANSI color noise and a single failure line.
    let res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"p1","op":"exec","command":"i=1; while [ $i -le 199 ]; do printf '\\033[32mok %03d\\033[0m\\n' $i; i=$((i+1)); done; printf '\\033[31mFAILED test_boom\\033[0m\\n'; printf 'aftermath detail\\n'; printf '\\033[33mwarn\\033[0m\\n' >&2; false","output":{"strip_ansi":true,"grep":"FAILED|panicked at","context":1,"max_bytes":16384}}"#,
    );
    assert_eq!(int(&res, "status"), 1, "{res:?}");
    assert_eq!(text(&res, "termination"), "exited");
    let projected_out = text(&res, "stdout");
    assert!(!projected_out.contains('\u{1b}'), "{projected_out:?}");
    assert_eq!(
        projected_out,
        "ok 199\nFAILED test_boom\naftermath detail\n"
    );
    let projected_err = text(&res, "stderr");
    assert!(!projected_err.contains('\u{1b}'), "{projected_err:?}");
    assert!(int(&res, "stdout_bytes") > projected_out.len() as i64);

    let proj = res.get("projection").expect("projection summary present");
    assert_eq!(int(proj, "stdout_total_lines"), 201);
    assert_eq!(int(proj, "stdout_returned_lines"), 3);
    assert_eq!(int(proj, "stdout_matched_lines"), 1);

    let cap_id = text(&res, "capture_id").to_string();
    assert!(cap_id.starts_with("cap-"), "{cap_id:?}");

    // Read raw ANSI-preserved lines from capture store without re-running.
    let raw = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"rc1","op":"read_capture","capture_id":{},"stream":"stdout","line_start":1,"line_count":3,"strip_ansi":false}}"#,
            json::Value::String(cap_id.clone()).encode()
        ),
    );
    assert_eq!(raw.get("type").and_then(json::Value::as_str), Some("capture"), "{raw:?}");
    assert_eq!(text(&raw, "encoding"), "utf8");
    assert!(text(&raw, "data").contains("\u{1b}[32mok 001\u{1b}[0m"));
    assert_eq!(int(&raw, "total_lines"), 201);
    assert_eq!(int(&raw, "returned_lines"), 3);

    // head_lines + tail_lines inserts one omission marker line.
    let ht = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"p2","op":"exec","command":"i=1; while [ $i -le 10 ]; do echo L$i; i=$((i+1)); done","output":{"head_lines":2,"tail_lines":2}}"#,
    );
    assert_eq!(
        text(&ht, "stdout"),
        "L1\nL2\n... (6 lines omitted)\nL9\nL10\n"
    );

    // UTF-8 safe max_bytes boundary truncation.
    let utf8 = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"p3","op":"exec","command":"printf 'aaééé'","output":{"max_bytes":5}}"#,
    );
    assert_eq!(text(&utf8, "stdout"), "aaé");
    assert_eq!(int(&utf8, "stdout_bytes"), 8);

    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn output_projection_and_capture_store_work_across_parallel_and_workflow_retries() {
    let dir = temp_dir("wf-cap");
    let count_file = dir.join("attempts");
    let count_str = count_file.to_string_lossy().into_owned();

    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let par = session.parallel(
        r#"{"schema_version":1,"id":"par1","op":"parallel","tasks":[{"id":"t1","command":"printf 'a\nb\nc\nd\n'","output":{"tail_lines":2}}]}"#,
    );
    assert_eq!(int(&par, "status"), 0, "{par:?}");
    let t1 = &par.get("tasks").unwrap().as_array().unwrap()[0];
    assert_eq!(text(t1, "stdout"), "c\nd\n");
    assert_eq!(int(t1, "stdout_bytes"), 8);
    let par_cap = text(t1, "capture_id").to_string();

    let par_raw = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"rc-par","op":"read_capture","capture_id":{},"line_start":1,"line_count":2}}"#,
            json::Value::String(par_cap).encode()
        ),
    );
    assert_eq!(text(&par_raw, "data"), "a\nb\n");

    let wf_cmd = format!(
        "echo x >> {}; n=$(wc -l < {}); if [ \"$n\" -eq 1 ]; then echo attempt-1-fail; exit 1; fi; echo attempt-2-pass",
        count_str, count_str
    );
    let wf_req = format!(
        r#"{{"schema_version":1,"id":"wf1","op":"workflow","tasks":[{{"id":"flaky","command":{},"retry":{{"max_attempts":2,"on":["failure"],"backoff_ms":0}}}}]}}"#,
        json::Value::String(wf_cmd).encode()
    );
    let wf = send_json(&mut session, &wf_req);
    assert_eq!(int(&wf, "status"), 0, "{wf:?}");
    let flaky = &wf.get("tasks").unwrap().as_array().unwrap()[0];
    assert_eq!(int(flaky, "attempts"), 2);
    // Only final committed attempt is recorded in capture store (`cap-1` was par1.t1, so this is `cap-2`).
    let wf_cap = text(flaky, "capture_id").to_string();
    assert_eq!(wf_cap, "cap-2", "{flaky:?}");

    let wf_raw = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"rc-wf","op":"read_capture","capture_id":{}}}"#,
            json::Value::String(wf_cap).encode()
        ),
    );
    assert_eq!(text(&wf_raw, "data"), "attempt-2-pass\n");

    let _ = fs::remove_dir_all(&dir);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn read_capture_pages_past_truncation_and_evicts_fifo_without_fd_leaks() {
    let mut session = Session::spawn(&["--no-config", "--max-capture-bytes", "64"]);
    ready(&mut session);

    let first = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"big1","op":"exec","command":"i=1; while [ $i -le 20 ]; do printf 'line-%02d\\n' $i; i=$((i+1)); done"}"#,
    );
    assert_eq!(int(&first, "status"), 0);
    assert_eq!(
        first.get("stdout_truncated").and_then(json::Value::as_bool),
        Some(true)
    );
    assert_eq!(int(&first, "stdout_bytes"), 160);
    let first_cap = text(&first, "capture_id").to_string();
    assert_eq!(first_cap, "cap-1");

    // Page past the 64-byte truncation limit without re-executing.
    let page = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"pg1","op":"read_capture","capture_id":"cap-1","line_start":15,"line_count":6}"#,
    );
    assert_eq!(
        text(&page, "data"),
        "line-15\nline-16\nline-17\nline-18\nline-19\nline-20\n"
    );
    assert_eq!(int(&page, "total_lines"), 20);
    assert_eq!(int(&page, "returned_lines"), 6);

    let grepped = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"gr1","op":"read_capture","capture_id":"cap-1","grep":"line-17"}"#,
    );
    assert_eq!(text(&grepped, "data"), "line-17\n");
    assert_eq!(int(&grepped, "matched_lines"), 1);

    // Measure open FDs in session before and after 40 stored captures (exceeds capacity 32).
    let fd_dir = format!("/proc/{}/fd", session.pid());
    let fd_before = fs::read_dir(&fd_dir).expect("read /proc fd dir").count();
    let mut last_cap = String::new();
    for i in 0..40 {
        let req = format!(
            r#"{{"schema_version":1,"id":"fill-{i}","op":"exec","command":"echo fill-{i}"}}"#
        );
        let r = send_json(&mut session, &req);
        last_cap = text(&r, "capture_id").to_string();
    }
    let fd_after = fs::read_dir(&fd_dir).expect("read /proc fd dir").count();
    assert!(
        fd_after <= fd_before,
        "capture store leaked FDs: before={fd_before}, after={fd_after}"
    );

    // Oldest capture (cap-1) was evicted FIFO -> non-fatal CAPTURE_NOT_FOUND.
    let evicted = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"ev1","op":"read_capture","capture_id":"cap-1"}"#,
    );
    assert_eq!(evicted.get("type").and_then(json::Value::as_str), Some("error"));
    let err_obj = evicted.get("error").unwrap();
    assert_eq!(text(err_obj, "code"), "CAPTURE_NOT_FOUND");
    assert_eq!(err_obj.get("fatal").and_then(json::Value::as_bool), Some(false));

    // Latest capture (cap-41) is still present.
    let live = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"lv1","op":"read_capture","capture_id":{}}}"#,
            json::Value::String(last_cap).encode()
        ),
    );
    assert_eq!(live.get("type").and_then(json::Value::as_str), Some("capture"), "{live:?}");
    assert_eq!(text(&live, "data"), "fill-39\n");

    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn binary_stream_is_safe_under_output_projection_and_read_capture() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let bin = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"b1","op":"exec","command":"printf '\\377\\376\\000abc\\377'","output":{"strip_ansi":true,"grep":"abc"}}"#,
    );
    assert_eq!(int(&bin, "status"), 0, "{bin:?}");
    assert_eq!(text(&bin, "stdout_encoding"), "base64");
    assert_eq!(
        json::base64_decode(text(&bin, "stdout")).expect("valid base64"),
        b"\xff\xfe\x00abc\xff"
    );
    let cap_id = text(&bin, "capture_id").to_string();

    // Byte slicing works cleanly on binary captures.
    let slice = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"b2","op":"read_capture","capture_id":{},"byte_offset":1,"max_bytes":3}}"#,
            json::Value::String(cap_id.clone()).encode()
        ),
    );
    assert_eq!(text(&slice, "encoding"), "base64");
    assert_eq!(
        json::base64_decode(text(&slice, "data")).expect("valid slice base64"),
        b"\xfe\x00a"
    );
    assert_eq!(
        slice.get("truncated").and_then(json::Value::as_bool),
        Some(true)
    );

    // Line-oriented filters on binary data return INVALID_REQUEST without ending session.
    let bad_line = send_json(
        &mut session,
        &format!(
            r#"{{"schema_version":1,"id":"b3","op":"read_capture","capture_id":{},"line_start":1}}"#,
            json::Value::String(cap_id).encode()
        ),
    );
    assert_eq!(
        text(bad_line.get("error").unwrap(), "code"),
        "INVALID_REQUEST"
    );

    let (status, _, _) = session.finish();
    assert!(status.success());
}

