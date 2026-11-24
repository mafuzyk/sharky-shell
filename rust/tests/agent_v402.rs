//! End-to-end tests for Sharky 4.0.2 agent runtime features.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::agent::{NO_CONFIG, json};

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-v402-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch directory");
    dir
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

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    reader: BufReader<ChildStdout>,
}

impl McpClient {
    fn spawn_with_args(extra: &[&str]) -> Self {
        let mut args = vec!["agent", "--mcp", "--no-config"];
        args.extend_from_slice(extra);
        let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(&args)
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("PATH", common::tool_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sharky agent --mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut client = Self {
            child,
            stdin: Some(stdin),
            reader: BufReader::new(stdout),
        };
        client.send_ndjson(
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
        );
        let _ = client.recv_ndjson();
        client.send_ndjson(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        client
    }

    fn spawn() -> Self {
        Self::spawn_with_args(&[])
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

    fn call_tool_raw(&mut self, id: i64, name: &str, args_json: &str) -> json::Value {
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args_json}}}}}"#
        );
        self.send_ndjson(&req);
        let resp = self.recv_ndjson();
        assert_eq!(int(get(&resp, "id")), id);
        get(&resp, "result").clone()
    }

    fn call_tool(&mut self, id: i64, name: &str, args_json: &str) -> json::Value {
        let result = self.call_tool_raw(id, name, args_json);
        assert_eq!(
            result.get("isError").and_then(json::Value::as_bool),
            Some(false),
            "tool {name} returned error: {result:?}"
        );
        get(&result, "structuredContent").clone()
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
fn output_parse_structured_parsers() {
    let mut mcp = McpClient::spawn();

    // lines parser
    let lines = mcp.call_tool(
        1,
        "sharky_exec",
        r#"{"command":"printf 'alpha\n\nbeta\n'","output":{"parse":"lines"}}"#,
    );
    let parsed = get(&lines, "parsed");
    assert_eq!(int(get(parsed, "count")), 2);
    assert_eq!(
        get(parsed, "lines").as_array().unwrap(),
        &[
            json::Value::String("alpha".into()),
            json::Value::String("beta".into()),
        ]
    );

    // json parser
    let jres = mcp.call_tool(
        2,
        "sharky_exec",
        r#"{"command":"printf '{\"k\":42,\"ok\":true}'","output":{"parse":"json"}}"#,
    );
    let jparsed = get(&jres, "parsed");
    assert_eq!(get(jparsed, "ok").as_bool(), Some(true));
    assert_eq!(int(get(get(jparsed, "value"), "k")), 42);

    // git_status parser
    let gs = mcp.call_tool(
        3,
        "sharky_exec",
        r#"{"command":"printf '## main...origin/main\nM  src/lib.rs\n M Cargo.toml\n?? new.txt\n'","output":{"parse":"git_status"}}"#,
    );
    let gsp = get(&gs, "parsed");
    assert_eq!(text(get(gsp, "branch")), "main...origin/main".split("...").next().unwrap());
    assert_eq!(get(gsp, "staged").as_array().unwrap().len(), 1);
    assert_eq!(get(gsp, "unstaged").as_array().unwrap().len(), 1);
    assert_eq!(get(gsp, "untracked").as_array().unwrap().len(), 1);

    // rg parser
    let rg = mcp.call_tool(
        4,
        "sharky_exec",
        r#"{"command":"printf 'src/main.rs:12:5:fn main() {}\nsrc/lib.rs:3:let x = 1;\n'","output":{"parse":"rg"}}"#,
    );
    let rgp = get(&rg, "parsed");
    assert_eq!(int(get(rgp, "count")), 2);
    let matches = get(rgp, "matches").as_array().unwrap();
    assert_eq!(text(get(&matches[0], "file")), "src/main.rs");
    assert_eq!(int(get(&matches[0], "col")), 5);

    // cargo parser
    let cargo = mcp.call_tool(
        5,
        "sharky_exec",
        r#"{"command":"printf 'error[E0308]: mismatched types\n  --> src/foo.rs:10:2\nwarning: unused\ntest smoke ... FAILED\ntest result: FAILED. 0 passed; 1 failed\n' >&2","output":{"parse":"cargo"}}"#,
    );
    let cp = get(&cargo, "parsed");
    assert_eq!(get(cp, "errors").as_array().unwrap().len(), 1);
    assert_eq!(text(get(&get(cp, "errors").as_array().unwrap()[0], "location")), "src/foo.rs:10:2");
    assert_eq!(get(cp, "warnings").as_array().unwrap().len(), 1);
    assert_eq!(get(cp, "failed_tests").as_array().unwrap(), &[json::Value::String("smoke".into())]);
    assert_eq!(get(cp, "summaries").as_array().unwrap().len(), 1);

    // pytest parser
    let pytest = mcp.call_tool(
        6,
        "sharky_exec",
        r#"{"command":"printf 'FAILED tests/test_a.py::test_x - AssertionError\n=== 1 failed, 4 passed, 2 skipped in 0.12s ===\n'","output":{"parse":"pytest"}}"#,
    );
    let pp = get(&pytest, "parsed");
    assert_eq!(int(get(pp, "passed")), 4);
    assert_eq!(int(get(pp, "failed_count")), 1);
    assert_eq!(int(get(pp, "skipped")), 2);
    assert_eq!(get(pp, "failed").as_array().unwrap(), &[json::Value::String("tests/test_a.py::test_x".into())]);

    mcp.finish();
}

#[test]
fn fs_delta_tracks_created_modified_and_removed_files() {
    let dir = scratch("fs-delta");
    let keep = dir.join("keep.txt");
    let remove_me = dir.join("gone.txt");
    std::fs::write(&keep, "old").unwrap();
    std::fs::write(&remove_me, "bye").unwrap();

    let mut mcp = McpClient::spawn();
    let cd_cmd = format!(r#"{{"command":"cd {}"}}"#, dir.display());
    mcp.call_tool(1, "sharky_exec", &cd_cmd);

    let res = mcp.call_tool(
        2,
        "sharky_exec",
        r#"{"command":"printf 'newcontent' > keep.txt; printf 'hi' > added.txt; rm gone.txt","fs_delta":true}"#,
    );
    let delta = get(&res, "fs_delta");
    assert_eq!(get(delta, "created").as_array().unwrap(), &[json::Value::String("added.txt".into())]);
    assert_eq!(get(delta, "modified").as_array().unwrap(), &[json::Value::String("keep.txt".into())]);
    assert_eq!(get(delta, "removed").as_array().unwrap(), &[json::Value::String("gone.txt".into())]);
    assert_eq!(get(delta, "truncated").as_bool(), Some(false));

    mcp.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn session_snapshot_and_restore_rolls_back_cwd_vars_and_funcs() {
    let dir1 = scratch("snap-1");
    let dir2 = scratch("snap-2");
    let mut mcp = McpClient::spawn();

    mcp.call_tool(
        1,
        "sharky_exec",
        &format!(
            r#"{{"command":"cd {}; export FOO=v1; greet() {{ echo hi; }}; shopt -s nullglob"}}"#,
            dir1.display()
        ),
    );
    let snap = mcp.call_tool(2, "sharky_snapshot", r#"{"snapshot_id":"cp1"}"#);
    assert_eq!(text(get(&snap, "snapshot_id")), "cp1");

    mcp.call_tool(
        3,
        "sharky_exec",
        &format!(
            r#"{{"command":"cd {}; export FOO=v2; BAR=temp; greet() {{ echo bye; }}; shopt -u nullglob"}}"#,
            dir2.display()
        ),
    );

    let restored = mcp.call_tool(4, "sharky_restore", r#"{"snapshot_id":"cp1"}"#);
    assert_eq!(get(&restored, "restored").as_bool(), Some(true));

    let verify = mcp.call_tool(
        5,
        "sharky_exec",
        r#"{"command":"pwd && echo \"$FOO|$BAR|$(greet)\" && shopt -q nullglob && echo ng_on"}"#,
    );
    let out = text(get(&verify, "stdout"));
    assert!(out.contains(&dir1.display().to_string()), "{out:?}");
    assert!(out.contains("v1||hi\nng_on\n"), "{out:?}");

    mcp.finish();
    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn background_job_start_wait_until_poll_send_stop() {
    let mut mcp = McpClient::spawn();

    let started = mcp.call_tool(
        1,
        "sharky_job_start",
        r#"{"command":"echo READY_NOW; while IFS= read -r line; do echo \"echo:$line\"; done","wait_until":{"regex":"READY_NOW","timeout_ms":3000}}"#,
    );
    let job_id = text(get(&started, "job_id")).to_string();
    assert_eq!(get(&started, "matched").as_bool(), Some(true));
    assert_eq!(get(&started, "running").as_bool(), Some(true));
    assert!(text(get(&started, "stdout")).contains("READY_NOW"));
    let mut cursor = int(get(&started, "stdout_cursor"));

    let sent = mcp.call_tool(
        2,
        "sharky_job_send",
        &format!(r#"{{"job_id":"{job_id}","data":"ping\n"}}"#),
    );
    assert!(int(get(&sent, "bytes_written")) >= 5);

    let polled = mcp.call_tool(
        3,
        "sharky_job_poll",
        &format!(r#"{{"job_id":"{job_id}","stdout_cursor":{cursor},"wait_ms":1000}}"#),
    );
    assert!(text(get(&polled, "stdout")).contains("echo:ping"), "{polled:?}");
    cursor = int(get(&polled, "stdout_cursor"));
    assert!(cursor > 0);

    let stopped = mcp.call_tool(
        4,
        "sharky_job_stop",
        &format!(r#"{{"job_id":"{job_id}","signal":"TERM"}}"#),
    );
    assert_eq!(get(&stopped, "running").as_bool(), Some(false));

    mcp.finish();
}

#[test]
fn policy_denies_requires_approval_and_forces_sandbox() {
    let dir = scratch("policy");
    let policy_path = dir.join("policy.json");
    std::fs::write(
        &policy_path,
        r#"{"rules":[{"id":"no-rm","pattern":"\\brm\\s+-rf\\b","action":"deny","message":"rm -rf blocked"},{"id":"push-gate","pattern":"\\bgit\\s+push\\b","action":"require_approval","message":"needs confirmation"},{"id":"curl-sb","pattern":"\\bcurl\\b","action":"force_sandbox","message":"sandboxed"}]}"#,
    )
    .unwrap();

    let policy_str = policy_path.to_string_lossy().into_owned();
    let mut mcp = McpClient::spawn_with_args(&["--policy", &policy_str]);

    let denied = mcp.call_tool_raw(1, "sharky_exec", r#"{"command":"rm -rf /tmp/nonexistent"}"#);
    assert_eq!(denied.get("isError").and_then(json::Value::as_bool), Some(true));
    let sc = get(&denied, "structuredContent");
    assert_eq!(text(get(sc, "code")), "POLICY_DENIED");
    assert!(text(get(sc, "message")).contains("no-rm"));

    let approval = mcp.call_tool_raw(2, "sharky_exec", r#"{"command":"git push origin main"}"#);
    assert_eq!(approval.get("isError").and_then(json::Value::as_bool), Some(true));
    let sc2 = get(&approval, "structuredContent");
    assert_eq!(text(get(sc2, "code")), "REQUIRES_APPROVAL");

    let sandboxed = mcp.call_tool(3, "sharky_exec", r#"{"command":"echo curl ok"}"#);
    let sb_status = int(get(&sandboxed, "status"));
    if sb_status == 0 {
        assert!(text(get(&sandboxed, "stdout")).contains("curl ok"));
    } else {
        // On hosts where AppArmor/kernel restricts unprivileged user namespaces (e.g. Ubuntu 24.04 CI),
        // exit 126 with sandbox namespace error proves force_sandbox was enforced.
        assert_eq!(sb_status, 126, "{sandboxed:?}");
        let err = text(get(&sandboxed, "stderr"));
        assert!(
            err.contains("sandbox namespace setup failed")
                || err.contains("sandbox uid/gid mapping failed"),
            "{sandboxed:?}"
        );
    }

    mcp.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn socket_multi_client_serves_concurrent_clients() {
    let dir = scratch("multi-sock");
    let sock_path = dir.join("daemon.sock");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args([
            "agent",
            "--socket",
            sock_path.to_str().unwrap(),
            "--multi-client",
            "--no-config",
        ])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn multi-client daemon");

    let deadline = Instant::now() + Duration::from_secs(3);
    while !sock_path.exists() {
        assert!(Instant::now() < deadline, "daemon socket did not appear");
        std::thread::sleep(Duration::from_millis(10));
    }

    let c1 = UnixStream::connect(&sock_path).expect("connect client 1");
    let c2 = UnixStream::connect(&sock_path).expect("connect client 2");

    let mut r1 = BufReader::new(c1.try_clone().unwrap());
    let mut r2 = BufReader::new(c2.try_clone().unwrap());
    let mut w1 = c1;
    let mut w2 = c2;

    let mut line = String::new();
    r1.read_line(&mut line).unwrap();
    assert_eq!(text(get(&parse(line.trim()), "type")), "ready");
    line.clear();
    r2.read_line(&mut line).unwrap();
    assert_eq!(text(get(&parse(line.trim()), "type")), "ready");

    w1.write_all(b"{\"schema_version\":1,\"id\":\"c1\",\"op\":\"exec\",\"command\":\"A=client1; echo $A\"}\n").unwrap();
    w1.flush().unwrap();
    w2.write_all(b"{\"schema_version\":1,\"id\":\"c2\",\"op\":\"exec\",\"command\":\"echo \\\"${A:-unset}|client2\\\"\"}\n").unwrap();
    w2.flush().unwrap();

    line.clear();
    r1.read_line(&mut line).unwrap();
    let res1 = parse(line.trim());
    assert_eq!(text(get(&res1, "stdout")), "client1\n", "{res1:?}");

    line.clear();
    r2.read_line(&mut line).unwrap();
    let res2 = parse(line.trim());
    assert_eq!(text(get(&res2, "stdout")), "unset|client2\n", "{res2:?}");

    w1.write_all(b"{\"schema_version\":1,\"id\":\"q1\",\"op\":\"close\"}\n").unwrap();
    w2.write_all(b"{\"schema_version\":1,\"id\":\"q2\",\"op\":\"close\"}\n").unwrap();
    drop(w1);
    drop(w2);

    // SAFETY: terminate daemon process cleanly.
    unsafe { nix::libc::kill(daemon.id() as i32, nix::libc::SIGTERM) };
    let status = daemon.wait().unwrap();
    assert!(status.code().is_some() || !status.success());
    let _ = std::fs::remove_dir_all(&dir);
}
