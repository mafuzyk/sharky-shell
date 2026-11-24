//! Integration tests for Layer 3 native Code Mode (`op:"code"` and `sharky code`).

mod common;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use common::agent::{NO_CONFIG, Session, json};

fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("sharky-code-{name}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("create temp dir");
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

fn encode_str(s: &str) -> String {
    json::Value::String(s.to_string()).encode()
}

#[test]
fn code_mode_sequential_parallel_and_map_parallel_dynamic_fanout() {
    let dir = temp_dir("fanout");
    for (name, lines) in [("a.rs", "1\n"), ("b.rs", "1\n2\n"), ("c.rs", "1\n2\n3\n"), ("d.rs", "1\n2\n3\n4\n")] {
        fs::write(dir.join(name), lines).expect("write rs file");
    }

    let mut session = Session::spawn(&["--no-config"]);
    let ready_frame = ready(&mut session);
    let protocol = ready_frame.get("protocol").unwrap();
    let ops = protocol.get("ops").and_then(json::Value::as_array).unwrap();
    let features = protocol.get("features").and_then(json::Value::as_array).unwrap();
    let limits = protocol.get("limits").unwrap();
    assert!(ops.iter().any(|o| o.as_str() == Some("code")), "{ops:?}");
    assert!(features.iter().any(|f| f.as_str() == Some("code_mode")), "{features:?}");
    assert_eq!(int(limits, "max_code_steps"), 100_000);
    assert_eq!(int(limits, "max_code_calls"), 256);

    let program = r#"
        let found = run(["find", args.dir, "-maxdepth", "1", "-name", "*.rs"]);
        let files = found.stdout.trim().lines().sort();
        let results = map_parallel(files, 4, f => ({ argv: ["wc", "-l", f] }));
        let counts = results.map(r => r.stdout.trim().split(" ")[0]);
        return { count: files.len(), counts: counts };
    "#;
    let req = format!(
        r#"{{"schema_version":1,"id":"c1","op":"code","code":{},"args":{{"dir":{}}}}}"#,
        encode_str(program),
        encode_str(&dir.to_string_lossy())
    );
    let res = send_json(&mut session, &req);
    assert_eq!(text(&res, "type"), "code", "{res:?}");
    assert_eq!(int(&res, "status"), 0, "{res:?}");
    assert_eq!(text(&res, "termination"), "exited");

    let val = res.get("value").unwrap();
    assert_eq!(int(val, "count"), 4);
    assert_eq!(
        val.get("counts"),
        Some(&json::Value::Array(vec![
            json::Value::String("1".into()),
            json::Value::String("2".into()),
            json::Value::String("3".into()),
            json::Value::String("4".into()),
        ]))
    );

    let audit = res.get("audit").unwrap();
    assert_eq!(int(audit, "calls"), 5);
    assert_eq!(int(audit, "failed_calls"), 0);
    assert!(
        audit
            .get("captures")
            .and_then(json::Value::as_array)
            .is_some_and(|c| c.len() >= 5),
        "{audit:?}"
    );

    let _ = fs::remove_dir_all(&dir);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn code_mode_workflow_branching_and_read_capture() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let program = r#"
        let r = sh("i=1; while [ $i -le 20 ]; do echo L$i; i=$((i+1)); done", {
            output: { tail_lines: 2 }
        });
        let head = read_capture(r.capture_id, { line_start: 1, line_count: 2 });
        if head.data.contains("L1") {
            let wf = workflow({
                tasks: [
                    { id: "a", argv: ["printf", "step-a\n"] },
                    {
                        id: "b",
                        depends_on: ["a"],
                        condition: { task: "a", exit_code: { eq: 0 } },
                        argv: ["printf", "step-b\n"]
                    }
                ]
            });
            return {
                tail: r.stdout.trim(),
                head: head.data.trim(),
                wf_status: wf.status,
                b: wf.tasks[1].stdout.trim()
            };
        } else {
            return { unexpected: true };
        }
    "#;
    let req = format!(
        r#"{{"schema_version":1,"id":"c2","op":"code","code":{}}}"#,
        encode_str(program)
    );
    let res = send_json(&mut session, &req);
    assert_eq!(text(&res, "type"), "code", "{res:?}");
    assert_eq!(int(&res, "status"), 0, "{res:?}");
    let val = res.get("value").unwrap();
    assert_eq!(text(val, "tail"), "L19\nL20");
    assert_eq!(text(val, "head"), "L1\nL2");
    assert_eq!(int(val, "wf_status"), 0);
    assert_eq!(text(val, "b"), "step-b");

    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn code_mode_audit_cannot_hide_failures() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let program = r#"
        let a = run(["false"]);
        let b = sh("(exit 7)");
        return { ok: true, ignored: [a.status, b.status] };
    "#;
    let req = format!(
        r#"{{"schema_version":1,"id":"c3","op":"code","code":{}}}"#,
        encode_str(program)
    );
    let res = send_json(&mut session, &req);
    assert_eq!(text(&res, "type"), "code", "{res:?}");
    assert_eq!(int(&res, "status"), 0);
    let val = res.get("value").unwrap();
    assert_eq!(val.get("ok"), Some(&json::Value::Bool(true)));

    let audit = res.get("audit").unwrap();
    assert_eq!(int(audit, "calls"), 2);
    assert_eq!(int(audit, "failed_calls"), 2);
    assert_eq!(int(audit, "first_failure_status"), 1);

    let ok = send_json(&mut session, r#"{"schema_version":1,"id":"e1","op":"exec","command":"true"}"#);
    assert_eq!(int(&ok, "status"), 0);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn code_mode_aggregate_child_results_without_hiding_failures() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let program = r#"
        let sh_runs = [
            sh("printf ok"),
            sh("(printf fail; exit 3)"),
            sh("printf ok")
        ];
        let run_runs = [
            run(["false"]),
            run(["sh", "-c", "exit 9"])
        ];
        let all = sh_runs + run_runs;
        let by_status = all.group_by(r => r.status);
        let failed = all.filter(r => r.status != 0);
        let failure_count = failed.reduce(0, (acc, r) => acc + 1);
        let ranked = failed.sort_by(r => r.status);
        return {
            counts: { ok: by_status.get("0").len(), failed: by_status.get("3").len() },
            failure_count: failure_count,
            statuses: ranked.map(r => r.status),
            claimed_ok: true
        };
    "#;
    let req = format!(
        r#"{{"schema_version":1,"id":"c-agg","op":"code","code":{}}}"#,
        encode_str(program)
    );
    let res = send_json(&mut session, &req);
    assert_eq!(text(&res, "type"), "code", "{res:?}");
    assert_eq!(int(&res, "status"), 0, "{res:?}");

    let val = res.get("value").unwrap();
    let counts = val.get("counts").unwrap();
    assert_eq!(int(counts, "ok"), 2);
    assert_eq!(int(counts, "failed"), 1);
    assert_eq!(int(val, "failure_count"), 3);
    assert_eq!(
        val.get("statuses"),
        Some(&json::Value::Array(vec![
            json::Value::Int(1),
            json::Value::Int(3),
            json::Value::Int(9),
        ]))
    );
    assert_eq!(val.get("claimed_ok"), Some(&json::Value::Bool(true)));

    let audit = res.get("audit").unwrap();
    assert_eq!(int(audit, "calls"), 5, "{audit:?}");
    assert_eq!(int(audit, "failed_calls"), 3, "{audit:?}");
    assert_eq!(int(audit, "first_failure_status"), 3, "{audit:?}");

    let alive = send_json(&mut session, r#"{"schema_version":1,"id":"ok","op":"exec","command":"true"}"#);
    assert_eq!(int(&alive, "status"), 0);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn code_mode_sort_by_rejects_mixed_keys_with_truthful_audit() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    let program = r#"
        let ok = sh("printf ok");
        let bad = sh("(printf fail; exit 7)");
        [2, 10, "2"].sort_by(x => x)
    "#;
    let req = format!(
        r#"{{"schema_version":1,"id":"c-sort-by-mixed","op":"code","code":{}}}"#,
        encode_str(program)
    );
    let res = send_json(&mut session, &req);
    assert_eq!(text(&res, "type"), "code", "{res:?}");
    assert_eq!(int(&res, "status"), 1, "{res:?}");
    assert!(
        text(&res, "error").contains("sort_by keys must all be numbers or all strings"),
        "{res:?}"
    );

    let audit = res.get("audit").unwrap();
    assert_eq!(int(audit, "calls"), 2, "{audit:?}");
    assert_eq!(int(audit, "failed_calls"), 1, "{audit:?}");
    assert_eq!(int(audit, "first_failure_status"), 7, "{audit:?}");

    let alive = send_json(&mut session, r#"{"schema_version":1,"id":"ok","op":"exec","command":"true"}"#);
    assert_eq!(int(&alive, "status"), 0);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn code_mode_enforces_step_call_timeout_and_cancel_bounds() {
    let mut session = Session::spawn(&["--no-config"]);
    ready(&mut session);

    // Step limit stops infinite loops cleanly.
    let step_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"b1","op":"code","max_steps":200,"code":"let i = 0; while true { i = i + 1; }"}"#,
    );
    assert_eq!(text(&step_res, "type"), "code", "{step_res:?}");
    assert_eq!(int(&step_res, "status"), 1, "{step_res:?}");
    assert!(text(&step_res, "error").contains("max_steps"), "{step_res:?}");

    // Call limit stops unbounded child execution.
    let call_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"b2","op":"code","max_calls":2,"code":"for i in range(5) { run([\"true\"]); }"}"#,
    );
    assert_eq!(text(&call_res, "type"), "code", "{call_res:?}");
    assert_eq!(int(&call_res, "status"), 1, "{call_res:?}");
    assert!(text(&call_res, "error").contains("max_calls"), "{call_res:?}");
    assert_eq!(int(call_res.get("audit").unwrap(), "calls"), 2);

    // Syntax error returns status 2 in `type:"code"` without breaking session.
    let syn_res = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"b3","op":"code","code":"let = ;"}"#,
    );
    assert_eq!(text(&syn_res, "type"), "code", "{syn_res:?}");
    assert_eq!(int(&syn_res, "status"), 2, "{syn_res:?}");
    assert!(syn_res.get("error").is_some());

    // Timeout propagates to child process group even if child ignores SIGTERM.
    let timed = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"b4","op":"code","timeout_ms":80,"code":"sh(\"trap '' TERM; sleep 30\")"}"#,
    );
    assert_eq!(text(&timed, "type"), "code", "{timed:?}");
    assert_eq!(text(&timed, "termination"), "timeout", "{timed:?}");
    assert_eq!(int(&timed, "status"), 124, "{timed:?}");

    // `op:"cancel"` interrupts a running `op:"code"` and session survives.
    session.send(r#"{"schema_version":1,"id":"b5","op":"code","code":"sh(\"sleep 30\")"}"#);
    std::thread::sleep(std::time::Duration::from_millis(40));
    session.send(r#"{"schema_version":1,"id":"k1","op":"cancel","target":"b5","grace_ms":50}"#);
    let f1 = session.json();
    let f2 = session.json();
    let (ack, code_res) = if f1.get("type").and_then(json::Value::as_str) == Some("ack") {
        (f1, f2)
    } else {
        (f2, f1)
    };
    assert_eq!(text(&ack, "outcome"), "delivered");
    assert_eq!(text(&code_res, "type"), "code", "{code_res:?}");
    assert_eq!(text(&code_res, "termination"), "cancelled", "{code_res:?}");
    assert_eq!(int(&code_res, "status"), 143, "{code_res:?}");

    let alive = send_json(
        &mut session,
        r#"{"schema_version":1,"id":"ok","op":"exec","command":"true"}"#,
    );
    assert_eq!(int(&alive, "status"), 0);
    let (status, _, _) = session.finish();
    assert!(status.success());
}

#[test]
fn sharky_code_cli_executes_inline_file_stdin_and_timeout() {
    let dir = temp_dir("cli");
    let script = dir.join("prog.code");
    fs::write(&script, "let x = run([\"printf\", \"from-file\"]); x.stdout\n").unwrap();

    // Inline `-e` with noisy stdout inside child: only the clean JSON envelope reaches stdout!
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args([
            "code",
            "-e",
            "sh(\"echo noisy_stdout_ignored\"); let r = run([\"printf\", \"hello %s\", args.who]); { greeting: r.stdout, ok: r.status == 0 }",
            "--args",
            "{\"who\":\"sharky\"}",
            "--no-config",
        ])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("run sharky code -e");
    assert_eq!(out.status.code(), Some(0), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let parsed = json::parse_line(
        String::from_utf8(out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("valid single JSON line from sharky code");
    assert_eq!(text(&parsed, "type"), "code");
    assert_eq!(int(&parsed, "status"), 0);
    let val = parsed.get("value").unwrap();
    assert_eq!(text(val, "greeting"), "hello sharky");
    assert_eq!(val.get("ok"), Some(&json::Value::Bool(true)));

    // File path input.
    let file_out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["code", &script.to_string_lossy(), "--no-config"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("run sharky code <file>");
    assert_eq!(file_out.status.code(), Some(0));
    let file_json = json::parse_line(
        String::from_utf8(file_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("file JSON");
    assert_eq!(text(&file_json, "value"), "from-file");

    // Stdin `-` input.
    let mut stdin_child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["code", "-", "--no-config"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky code -");
    stdin_child
        .stdin
        .take()
        .unwrap()
        .write_all(b"run([\"printf\", \"from-stdin\"]).stdout")
        .unwrap();
    let stdin_out = stdin_child.wait_with_output().unwrap();
    assert_eq!(stdin_out.status.code(), Some(0));
    let stdin_json = json::parse_line(
        String::from_utf8(stdin_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("stdin JSON");
    assert_eq!(text(&stdin_json, "value"), "from-stdin");

    // CLI timeout returns exit 124 and `termination:"timeout"`.
    let timed_out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args([
            "code",
            "-e",
            "sh(\"sleep 30\")",
            "--timeout-ms",
            "80",
            "--no-config",
        ])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("run timed sharky code");
    assert_eq!(timed_out.status.code(), Some(124));
    let timed_json = json::parse_line(
        String::from_utf8(timed_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("timed JSON");
    assert_eq!(text(&timed_json, "termination"), "timeout");

    // `--help` matches `help code`.
    let help_flag = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["code", "--help"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .unwrap();
    let help_topic = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["help", "code"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .unwrap();
    assert_eq!(help_flag.status.code(), Some(0));
    assert_eq!(help_flag.stdout, help_topic.stdout);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn code_cli_fork_isolated_parallel_workflow_and_run_cwd_succeed() {
    let run_cli = |prog: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["code", "-e", prog, "--no-config"])
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("PATH", common::tool_path())
            .output()
            .expect("run sharky code");
        assert_eq!(
            out.status.code(),
            Some(0),
            "prog={prog}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        json::parse_line(
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end_matches('\n')
                .as_bytes(),
        )
        .expect("valid JSON line")
    };

    let p = run_cli(r#"let r = parallel([{ id: "p1", argv: ["echo", "one"] }]); r.tasks[0].stdout"#);
    assert_eq!(int(&p, "status"), 0, "{p:?}");
    assert_eq!(int(p.get("audit").unwrap(), "failed_calls"), 0, "{p:?}");
    assert_eq!(text(&p, "value"), "one\n");

    let r = run_cli(r#"run(["pwd"], { cwd: "/tmp" }).stdout.trim()"#);
    assert_eq!(int(&r, "status"), 0, "{r:?}");
    assert_eq!(int(r.get("audit").unwrap(), "failed_calls"), 0, "{r:?}");
    assert_eq!(text(&r, "value"), "/tmp");

    let w = run_cli(
        r#"let wf = workflow({ tasks: [{ id: "x", argv: ["echo", "x"] }] }); wf.tasks[0].stdout"#,
    );
    assert_eq!(int(&w, "status"), 0, "{w:?}");
    assert_eq!(int(w.get("audit").unwrap(), "failed_calls"), 0, "{w:?}");
    assert_eq!(text(&w, "value"), "x\n");
}

#[test]
fn code_sh_auto_isolates_with_cwd_and_env() {
    let run_cli = |prog: &str| {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["code", "-e", prog, "--no-config"])
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("PATH", common::tool_path())
            .output()
            .expect("run sharky code")
    };

    let pwd_out = run_cli(r#"sh("pwd", { cwd: "/tmp" }).stdout.trim()"#);
    assert_eq!(pwd_out.status.code(), Some(0), "{pwd_out:?}");
    let pwd_json = json::parse_line(
        String::from_utf8(pwd_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(int(&pwd_json, "status"), 0, "{pwd_json:?}");
    assert_eq!(int(pwd_json.get("audit").unwrap(), "failed_calls"), 0);
    assert_eq!(text(&pwd_json, "value"), "/tmp");

    let env_out = run_cli(r#"sh("printf %s \"$SH_ISO\"", { env: { SH_ISO: "ok" } }).stdout"#);
    assert_eq!(env_out.status.code(), Some(0), "{env_out:?}");
    let env_json = json::parse_line(
        String::from_utf8(env_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .unwrap();
    assert_eq!(int(&env_json, "status"), 0, "{env_json:?}");
    assert_eq!(int(env_json.get("audit").unwrap(), "failed_calls"), 0);
    assert_eq!(text(&env_json, "value"), "ok");

    let bad_out = run_cli(r#"sh("pwd", { cwd: "/tmp", isolated: false })"#);
    assert_eq!(bad_out.status.code(), Some(1));
    let bad_json = json::parse_line(
        String::from_utf8(bad_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .unwrap();
    assert!(
        text(&bad_json, "error").contains("cwd and env on exec require isolated: true"),
        "{bad_json:?}"
    );
}
