//! End-to-end tests for `op:"workflow"` (`docs/workflow-executor-design.md` §11).

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::agent::{Session, json};

// -------------------------------------------------------------------- helpers

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-agent-workflow-{name}-{}",
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

fn ready(session: &mut Session) -> json::Value {
    let response = session.json();
    assert_eq!(
        response.get("type").and_then(json::Value::as_str),
        Some("ready"),
        "{response:?}"
    );
    response
}

fn task_by_id<'a>(response: &'a json::Value, id: &str) -> &'a json::Value {
    array(get(response, "tasks"))
        .iter()
        .find(|task| text(get(task, "id")) == id)
        .unwrap_or_else(|| panic!("no task {id} in {response:?}"))
}

fn workflow_line(id: &str, fields: &str, tasks: &[String]) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"workflow",{fields}"tasks":[{}]}}"#,
        json::quote(id),
        tasks.join(",")
    )
}

fn cancel_line(id: &str, target: &str, task: Option<&str>, grace_ms: Option<i64>) -> String {
    let mut fields = format!(
        r#""id":{},"op":"cancel","target":{}"#,
        json::quote(id),
        json::quote(target)
    );
    if let Some(task) = task {
        fields.push_str(&format!(r#","task":{}"#, json::quote(task)));
    }
    if let Some(grace) = grace_ms {
        fields.push_str(&format!(r#","grace_ms":{grace}"#));
    }
    format!(r#"{{"schema_version":1,{fields}}}"#)
}

fn exec(session: &mut Session, id: &str, command: &str) -> json::Value {
    session.send(&format!(
        r#"{{"schema_version":1,"id":{},"op":"exec","command":{}}}"#,
        json::quote(id),
        json::quote(command)
    ));
    let response = session.json();
    assert_eq!(
        response.get("type").and_then(json::Value::as_str),
        Some("result"),
        "{response:?}"
    );
    response
}

fn run_workflow(session: &mut Session, line: &str) -> json::Value {
    session.send(line);
    let response = session.json_within(15_000).expect("workflow response");
    assert_eq!(text(get(&response, "type")), "workflow", "{response:?}");
    response
}

fn fd_count(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("read /proc/<pid>/fd")
        .count()
}

fn zombie_children(pid: u32) -> Vec<i32> {
    let mut zombies = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return zombies;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let Ok(child_pid) = name.parse::<i32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some(rparen) = stat.rfind(')') else {
            continue;
        };
        let fields: Vec<&str> = stat[rparen + 2..].split_whitespace().collect();
        if let (Some(&state), Some(&ppid)) = (fields.first(), fields.get(1))
            && state == "Z"
            && ppid.parse::<u32>() == Ok(pid)
        {
            zombies.push(child_pid);
        }
    }
    zombies
}

fn gone(pid: i64) -> bool {
    if pid <= 0 {
        return true;
    }
    let pid = pid as i32;
    // SAFETY: signal 0 only checks process existence.
    if unsafe { nix::libc::kill(pid, 0) } != 0 {
        return true;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => matches!(stat.split_whitespace().nth(2), Some("Z") | None),
        Err(_) => true,
    }
}

fn gone_within(pid: i64, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if gone(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(pid)
}

fn pgid_gone_within(pgid: i64, ms: u64) -> bool {
    if pgid <= 0 {
        return true;
    }
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        // SAFETY: signal 0 checks whether any member remains in the process group.
        if unsafe { nix::libc::killpg(pgid as i32, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    unsafe { nix::libc::killpg(pgid as i32, 0) != 0 }
}

fn read_pid_within(path: &std::path::Path, ms: u64) -> Option<i64> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if let Ok(raw) = std::fs::read_to_string(path)
            && let Ok(pid) = raw.trim().parse::<i64>()
        {
            return Some(pid);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
}

// ------------------------------------------------ rows 1-3: DAG validation

#[test]
fn row01_cycle_is_rejected_with_path_without_running_anything() {
    let dir = scratch("cycle");
    let marker = dir.join("ran");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let cmd = format!("touch {}", marker.display());
    let tasks = vec![
        format!(r#"{{"id":"a","command":{},"depends_on":["c"]}}"#, json::quote(&cmd)),
        format!(r#"{{"id":"b","command":{},"depends_on":["a"]}}"#, json::quote(&cmd)),
        format!(r#"{{"id":"c","command":{},"depends_on":["b"]}}"#, json::quote(&cmd)),
        format!(r#"{{"id":"indep","command":{}}}"#, json::quote(&cmd)),
    ];
    session.send(&workflow_line("w-cycle", "", &tasks));
    let response = session.json();
    assert_eq!(text(get(&response, "type")), "error", "{response:?}");
    let err = get(&response, "error");
    assert_eq!(text(get(err, "code")), "WORKFLOW_CYCLE", "{response:?}");
    assert!(!flag(get(err, "fatal")), "{response:?}");
    let message = text(get(err, "message"));
    assert!(
        message.contains("a ->") && message.contains("-> a"),
        "expected cycle path in error message: {message}"
    );
    assert!(!marker.exists(), "independent node must not run when DAG is invalid");

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row02_dependency_errors_reject_the_workflow_without_running_anything() {
    let dir = scratch("dep-errors");
    let marker = dir.join("ran");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let cmd = format!("touch {}", marker.display());
    for (id, tasks_json) in [
        ("w-missing", format!(r#"{{"id":"a","command":{},"depends_on":["ghost"]}}"#, json::quote(&cmd))),
        ("w-self", format!(r#"{{"id":"a","command":{},"depends_on":["a"]}}"#, json::quote(&cmd))),
        (
            "w-dup",
            format!(
                r#"{{"id":"a","command":"true"}},{{"id":"b","command":{},"depends_on":["a","a"]}}"#,
                json::quote(&cmd)
            ),
        ),
    ] {
        session.send(&workflow_line(id, "", &[tasks_json]));
        let response = session.json();
        assert_eq!(text(get(&response, "type")), "error", "{response:?}");
        assert_eq!(
            text(get(get(&response, "error"), "code")),
            "INVALID_DEPENDENCY",
            "{response:?}"
        );
        assert!(!flag(get(get(&response, "error"), "fatal")));
        assert!(!marker.exists(), "no task may run when {id} fails validation");
    }

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row03_empty_duplicate_oversized_and_malformed_workflows_are_rejected() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&workflow_line("w-empty", "", &[]));
    let empty = session.json();
    assert_eq!(text(get(get(&empty, "error"), "code")), "INVALID_REQUEST");

    session.send(&workflow_line(
        "w-dup",
        "",
        &[
            r#"{"id":"x","command":"true"}"#.to_string(),
            r#"{"id":"x","command":"true"}"#.to_string(),
        ],
    ));
    let dup = session.json();
    assert_eq!(text(get(get(&dup, "error"), "code")), "DUPLICATE_TASK_ID");

    let many: Vec<String> = (0..129)
        .map(|i| format!(r#"{{"id":"n{i}","command":"true"}}"#))
        .collect();
    session.send(&workflow_line("w-129", "", &many));
    let too_many = session.json();
    assert_eq!(text(get(get(&too_many, "error"), "code")), "TOO_MANY_TASKS");

    session.send(&workflow_line(
        "w-strdep",
        "",
        &[r#"{"id":"a","command":"true","depends_on":"a"}"#.to_string()],
    ));
    let strdep = session.json();
    assert_eq!(text(get(get(&strdep, "error"), "code")), "INVALID_DEPENDENCY");

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

// ------------------------------------------------ core DAG execution (U2b)

#[test]
fn diamond_runs_in_dependency_order_and_skips_dependents_of_failure() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Put `d` first in request order to prove `tasks[]` keeps request order
    // while `order[]` is topological (`a` before `b`/`c` before `d`).
    let tasks = vec![
        r#"{"id":"d","command":"echo D_RAN","depends_on":["b","c"]}"#.to_string(),
        r#"{"id":"c","command":"echo C_OK","depends_on":["a"]}"#.to_string(),
        r#"{"id":"b","command":"echo B_FAIL >&2; exit 7","depends_on":["a"]}"#.to_string(),
        r#"{"id":"a","command":"echo A_OK"}"#.to_string(),
    ];
    let response = run_workflow(&mut session, &workflow_line("w-diamond", "", &tasks));
    assert_eq!(text(get(&response, "op")), "workflow");
    assert_eq!(text(get(&response, "state")), "completed", "{response:?}");
    assert_eq!(int(get(&response, "status")), 7, "{response:?}");
    assert_eq!(int(get(&response, "started_tasks")), 3, "{response:?}");
    assert_eq!(int(get(&response, "attempts_total")), 3, "{response:?}");
    assert_eq!(int(get(&response, "skipped_tasks")), 1, "{response:?}");
    assert_eq!(int(get(&response, "blocked_tasks")), 0, "{response:?}");

    let req_ids: Vec<&str> = array(get(&response, "tasks"))
        .iter()
        .map(|t| text(get(t, "id")))
        .collect();
    assert_eq!(req_ids, vec!["d", "c", "b", "a"]);

    let topo: Vec<&str> = array(get(&response, "order")).iter().map(text).collect();
    assert_eq!(topo, vec!["a", "c", "b", "d"]);

    let a = task_by_id(&response, "a");
    assert_eq!(text(get(a, "state")), "completed");
    assert_eq!(text(get(a, "stdout")), "A_OK\n");
    assert_eq!(int(get(a, "attempts")), 1);
    assert_eq!(array(get(a, "attempts_detail")).len(), 1);

    let c = task_by_id(&response, "c");
    assert_eq!(text(get(c, "state")), "completed");
    assert_eq!(text(get(c, "stdout")), "C_OK\n");

    let b = task_by_id(&response, "b");
    assert_eq!(text(get(b, "state")), "completed");
    assert_eq!(int(get(b, "status")), 7);
    assert_eq!(text(get(b, "stderr")), "B_FAIL\n");

    let d = task_by_id(&response, "d");
    assert_eq!(text(get(d, "state")), "skipped");
    assert_eq!(text(get(d, "termination")), "skipped");
    assert_eq!(int(get(d, "status")), 125);
    assert_eq!(text(get(d, "reason")), "dependency_failed");
    assert_eq!(int(get(d, "attempts")), 0);
    assert!(array(get(d, "attempts_detail")).is_empty());

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

// ------------------------------------------------ rows 18, 20, 22

#[test]
fn row18_a_hundred_node_workflow_finishes_cleanly_without_fd_or_zombie_leaks() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let fds_before = fd_count(session.pid());

    // 4 chains of 24 nodes (96) + 1 fan-in `summary` depending on all 4 tails +
    // 3 fan-out nodes depending on `summary` = 100 nodes (max depth 26).
    let mut tasks = Vec::with_capacity(100);
    let mut tails = Vec::new();
    for chain in 0..4 {
        for step in 0..24 {
            let id = format!("c{chain}_{step}");
            if step == 0 {
                tasks.push(format!(r#"{{"id":{},"command":"true"}}"#, json::quote(&id)));
            } else {
                let prev = format!("c{chain}_{}", step - 1);
                tasks.push(format!(
                    r#"{{"id":{},"command":"true","depends_on":[{}]}}"#,
                    json::quote(&id),
                    json::quote(&prev)
                ));
            }
            if step == 23 {
                tails.push(id);
            }
        }
    }
    let deps_json: Vec<String> = tails.iter().map(|id| json::quote(id)).collect();
    tasks.push(format!(
        r#"{{"id":"summary","command":"true","depends_on":[{}]}}"#,
        deps_json.join(",")
    ));
    for i in 0..3 {
        let id = format!("leaf_{i}");
        tasks.push(format!(
            r#"{{"id":{},"command":"true","depends_on":["summary"]}}"#,
            json::quote(&id)
        ));
    }
    assert_eq!(tasks.len(), 100);

    let started = Instant::now();
    let response = run_workflow(
        &mut session,
        &workflow_line("w-100", r#""max_concurrency":8,"#, &tasks),
    );
    let elapsed = started.elapsed();

    assert_eq!(text(get(&response, "state")), "completed", "{response:?}");
    assert_eq!(int(get(&response, "status")), 0, "{response:?}");
    assert_eq!(int(get(&response, "started_tasks")), 100, "{response:?}");
    assert_eq!(array(get(&response, "tasks")).len(), 100);
    assert_eq!(array(get(&response, "order")).len(), 100);
    assert!(
        elapsed < Duration::from_secs(5),
        "100-node workflow took too long: {elapsed:?}"
    );

    let alive = exec(&mut session, "alive", "echo ok");
    assert_eq!(text(get(&alive, "stdout")), "ok\n");
    assert_eq!(
        fd_count(session.pid()),
        fds_before,
        "session leaked descriptors across 100 workflow nodes"
    );
    assert!(
        zombie_children(session.pid()).is_empty(),
        "session left zombie children after 100-node workflow"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row20_workflow_nodes_cannot_mutate_session_state() {
    let dir = scratch("isolation");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let setup = exec(
        &mut session,
        "setup",
        &format!("X=1; cd {}; echo ready", dir.display()),
    );
    assert_eq!(text(get(&setup, "stdout")), "ready\n");

    let tasks = vec![
        r#"{"id":"mutate","command":"X=2; cd /; export Y=3; exit 9"}"#.to_string(),
        r#"{"id":"after_mut","command":"echo $X"}"#.to_string(),
    ];
    let wf = run_workflow(&mut session, &workflow_line("w-iso", "", &tasks));
    assert_eq!(int(get(&wf, "status")), 9, "{wf:?}");
    assert_eq!(int(get(task_by_id(&wf, "mutate"), "status")), 9);
    assert_eq!(text(get(task_by_id(&wf, "after_mut"), "stdout")), "1\n");

    let check = exec(&mut session, "check", "echo \"$X $PWD ${Y-unset}\"");
    let expected = format!("1 {} unset\n", dir.display());
    assert_eq!(text(get(&check, "stdout")), expected);

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row22_workflow_answers_once_and_keeps_node_output_out_of_protocol() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![
        r#"{"id":"a","command":"printf '{\"fake\":\"protocol\"}\\n'"}"#.to_string(),
        r#"{"id":"b","command":"echo second >&2","depends_on":["a"]}"#.to_string(),
    ];
    let response = run_workflow(&mut session, &workflow_line("w-frame", "", &tasks));
    assert_eq!(text(get(&response, "id")), "w-frame");
    assert_eq!(
        text(get(task_by_id(&response, "a"), "stdout")),
        "{\"fake\":\"protocol\"}\n"
    );
    assert_eq!(
        text(get(task_by_id(&response, "b"), "stderr")),
        "second\n"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

// -------------------------------- rows 4-10, 16-17: policies, retries, conditions

#[test]
fn row04_to_06_failure_policy_controls_dependents_and_running_siblings() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Row 4: explicit `skip`.
    let diamond = vec![
        r#"{"id":"a","command":"true"}"#.to_string(),
        r#"{"id":"b","command":"exit 7","depends_on":["a"]}"#.to_string(),
        r#"{"id":"c","command":"echo C","depends_on":["a"]}"#.to_string(),
        r#"{"id":"d","command":"echo D","depends_on":["b","c"]}"#.to_string(),
    ];
    let skip_resp = run_workflow(
        &mut session,
        &workflow_line("w-skip", r#""failure_policy":"skip","#, &diamond),
    );
    assert_eq!(text(get(&skip_resp, "state")), "completed");
    assert_eq!(int(get(&skip_resp, "status")), 7);
    assert_eq!(int(get(&skip_resp, "skipped_tasks")), 1);
    assert_eq!(
        text(get(task_by_id(&skip_resp, "c"), "state")),
        "completed"
    );
    assert_eq!(text(get(task_by_id(&skip_resp, "d"), "state")), "skipped");
    assert_eq!(
        text(get(task_by_id(&skip_resp, "d"), "termination")),
        "skipped"
    );
    assert_eq!(int(get(task_by_id(&skip_resp, "d"), "status")), 125);
    assert_eq!(
        text(get(task_by_id(&skip_resp, "d"), "reason")),
        "dependency_failed"
    );

    // Row 5: `continue` runs `d` anyway while keeping `b`'s failure status.
    let cont_resp = run_workflow(
        &mut session,
        &workflow_line("w-continue", r#""failure_policy":"continue","#, &diamond),
    );
    assert_eq!(text(get(&cont_resp, "state")), "completed");
    assert_eq!(int(get(&cont_resp, "status")), 7);
    assert_eq!(int(get(&cont_resp, "skipped_tasks")), 0);
    assert_eq!(
        text(get(task_by_id(&cont_resp, "d"), "state")),
        "completed"
    );
    assert_eq!(text(get(task_by_id(&cont_resp, "d"), "stdout")), "D\n");

    // Row 6: `fail` stops running siblings and blocks downstream nodes promptly.
    let fail_diamond = vec![
        r#"{"id":"a","command":"true"}"#.to_string(),
        r#"{"id":"b","command":"exit 7","depends_on":["a"]}"#.to_string(),
        r#"{"id":"c","command":"sleep 2; echo C","depends_on":["a"]}"#.to_string(),
        r#"{"id":"d","command":"echo D","depends_on":["b","c"]}"#.to_string(),
    ];
    let started = Instant::now();
    let fail_resp = run_workflow(
        &mut session,
        &workflow_line("w-fail", r#""failure_policy":"fail","#, &fail_diamond),
    );
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "failure_policy:fail did not cancel slow sibling promptly: {:?}",
        started.elapsed()
    );
    assert_eq!(text(get(&fail_resp, "state")), "failed", "{fail_resp:?}");
    assert_eq!(int(get(&fail_resp, "status")), 7);
    assert_eq!(int(get(&fail_resp, "blocked_tasks")), 1);
    let c = task_by_id(&fail_resp, "c");
    assert_eq!(text(get(c, "state")), "cancelled", "{c:?}");
    assert_eq!(text(get(c, "reason")), "failure_policy", "{c:?}");
    let d = task_by_id(&fail_resp, "d");
    assert_eq!(text(get(d, "state")), "blocked", "{d:?}");
    assert_eq!(int(get(d, "status")), 130);
    assert_eq!(text(get(d, "reason")), "failure_policy", "{d:?}");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row07_fail_fast_vs_failure_policy_fail_on_an_independent_leaf() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let wide = vec![
        r#"{"id":"bad","command":"exit 3"}"#.to_string(),
        r#"{"id":"slow","command":"sleep 0.5; echo SLOW"}"#.to_string(),
        r#"{"id":"queued","command":"echo QUEUED"}"#.to_string(),
    ];

    let ff = run_workflow(
        &mut session,
        &workflow_line("w-ff", r#""policy":"fail-fast","max_concurrency":2,"#, &wide),
    );
    assert_eq!(text(get(&ff, "state")), "failed", "{ff:?}");
    assert_eq!(int(get(&ff, "status")), 3);
    assert_eq!(
        text(get(task_by_id(&ff, "slow"), "state")),
        "cancelled",
        "{ff:?}"
    );
    assert_eq!(text(get(task_by_id(&ff, "slow"), "reason")), "fail_fast");
    assert_eq!(
        text(get(task_by_id(&ff, "queued"), "state")),
        "blocked",
        "{ff:?}"
    );
    assert_eq!(text(get(task_by_id(&ff, "queued"), "reason")), "fail_fast");

    // `bad` has no dependents, so `wait-all` + `failure_policy:"fail"` lets
    // every other independent leaf finish while still preserving `status: 3`.
    let fp = run_workflow(
        &mut session,
        &workflow_line(
            "w-fp",
            r#""policy":"wait-all","failure_policy":"fail","max_concurrency":2,"#,
            &wide,
        ),
    );
    assert_eq!(text(get(&fp, "state")), "completed", "{fp:?}");
    assert_eq!(int(get(&fp, "status")), 3);
    assert_eq!(
        text(get(task_by_id(&fp, "slow"), "state")),
        "completed",
        "{fp:?}"
    );
    assert_eq!(
        text(get(task_by_id(&fp, "queued"), "state")),
        "completed",
        "{fp:?}"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row08_retries_respect_fixed_and_exponential_backoff_without_blocking() {
    let dir = scratch("retry-backoff");
    let count_fixed = dir.join("count_fixed");
    let count_exp = dir.join("count_exp");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let fixed_cmd = format!(
        "n=0; [ -f {} ] && read n < {}; n=$((n + 1)); echo $n > {}; echo try_$n; [ $n -ge 3 ]",
        count_fixed.display(),
        count_fixed.display(),
        count_fixed.display()
    );
    let fixed_wf = workflow_line(
        "w-fixed",
        "",
        &[format!(
            r#"{{"id":"flaky","command":{},"retry":{{"max_attempts":3,"on":["failure"],"backoff_ms":150,"backoff":"fixed"}}}}"#,
            json::quote(&fixed_cmd)
        )],
    );
    let fixed = run_workflow(&mut session, &fixed_wf);
    assert_eq!(text(get(&fixed, "state")), "completed");
    assert_eq!(int(get(&fixed, "status")), 0);
    assert_eq!(int(get(&fixed, "started_tasks")), 1);
    assert_eq!(int(get(&fixed, "attempts_total")), 3);

    let flaky = task_by_id(&fixed, "flaky");
    assert_eq!(int(get(flaky, "attempts")), 3);
    assert_eq!(text(get(flaky, "stdout")), "try_3\n");
    let details = array(get(flaky, "attempts_detail"));
    assert_eq!(details.len(), 3);
    assert_eq!(int(get(&details[0], "status")), 1);
    assert!(int(get(&details[0], "stdout_bytes")) > 0);
    assert_eq!(int(get(&details[1], "status")), 1);
    assert!(int(get(&details[1], "stdout_bytes")) > 0);
    assert_eq!(int(get(&details[2], "status")), 0);
    let s0 = int(get(&details[0], "started_ms"));
    let s1 = int(get(&details[1], "started_ms"));
    let s2 = int(get(&details[2], "started_ms"));
    assert!(s1 - s0 >= 120, "gap1 too short: {s0} -> {s1}");
    assert!(s2 - s1 >= 120, "gap2 too short: {s1} -> {s2}");
    assert!(int(get(&fixed, "duration_ms")) >= 250);
    assert!(
        int(get(flaky, "duration_ms")) < int(get(&fixed, "duration_ms")),
        "node duration must exclude backoff wait: {fixed:?}"
    );

    let exp_cmd = format!(
        "n=0; [ -f {} ] && read n < {}; n=$((n + 1)); echo $n > {}; [ $n -ge 3 ]",
        count_exp.display(),
        count_exp.display(),
        count_exp.display()
    );
    let exp_wf = workflow_line(
        "w-exp",
        "",
        &[format!(
            r#"{{"id":"exp","command":{},"retry":{{"max_attempts":3,"on":["failure"],"backoff_ms":100,"backoff":"exponential"}}}}"#,
            json::quote(&exp_cmd)
        )],
    );
    let exp = run_workflow(&mut session, &exp_wf);
    assert_eq!(int(get(&exp, "status")), 0);
    let edetails = array(get(task_by_id(&exp, "exp"), "attempts_detail"));
    assert_eq!(edetails.len(), 3);
    let es0 = int(get(&edetails[0], "started_ms"));
    let es1 = int(get(&edetails[1], "started_ms"));
    let es2 = int(get(&edetails[2], "started_ms"));
    assert!(es1 - es0 >= 80, "exp gap1 too short: {es0} -> {es1}");
    assert!(es2 - es1 >= 160, "exp gap2 too short: {es1} -> {es2}");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row09_retry_defaults_exclude_plain_failures_and_retry_timeouts() {
    let dir = scratch("retry-defaults");
    let count_to = dir.join("count_to");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let to_cmd = format!(
        "n=0; [ -f {} ] && read n < {}; n=$((n + 1)); echo $n > {}; if [ $n -eq 1 ]; then sleep 2; else echo RECOVERED; fi",
        count_to.display(),
        count_to.display(),
        count_to.display()
    );
    let wf = workflow_line(
        "w-defaults",
        "",
        &[
            r#"{"id":"no_retry","command":"exit 1"}"#.to_string(),
            r#"{"id":"default_on","command":"exit 1","retry":{"max_attempts":3}}"#.to_string(),
            r#"{"id":"timeout_only","command":"exit 1","retry":{"max_attempts":3,"on":["timeout"]}}"#
                .to_string(),
            format!(
                r#"{{"id":"retry_timeout","command":{},"timeout_ms":150,"retry":{{"max_attempts":2,"on":["timeout"],"backoff_ms":50}}}}"#,
                json::quote(&to_cmd)
            ),
        ],
    );
    let response = run_workflow(&mut session, &wf);
    assert_eq!(int(get(task_by_id(&response, "no_retry"), "attempts")), 1);
    assert_eq!(int(get(task_by_id(&response, "default_on"), "attempts")), 1);
    assert_eq!(
        int(get(task_by_id(&response, "timeout_only"), "attempts")),
        1
    );

    let recovered = task_by_id(&response, "retry_timeout");
    assert_eq!(text(get(recovered, "state")), "completed", "{recovered:?}");
    assert_eq!(int(get(recovered, "status")), 0);
    assert_eq!(int(get(recovered, "attempts")), 2);
    assert_eq!(text(get(recovered, "stdout")), "RECOVERED\n");
    let details = array(get(recovered, "attempts_detail"));
    assert_eq!(text(get(&details[0], "termination")), "timeout");
    assert_eq!(text(get(&details[1], "termination")), "exited");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row10_cancelling_a_node_during_long_backoff_aborts_promptly_without_respawn() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let wf = workflow_line(
        "w-backoff-cancel",
        "",
        &[
            r#"{"id":"flaky","command":"echo TRY1; exit 1","retry":{"max_attempts":3,"on":["failure"],"backoff_ms":5000}}"#
                .to_string(),
        ],
    );
    let started = Instant::now();
    session.send(&wf);
    std::thread::sleep(Duration::from_millis(180));
    session.send(&cancel_line(
        "c-backoff",
        "w-backoff-cancel",
        Some("flaky"),
        None,
    ));

    let ack = session.json_within(2000).expect("cancel ack");
    assert_eq!(text(get(&ack, "type")), "ack", "{ack:?}");

    let response = session.json_within(2500).expect("cancelled workflow");
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "cancel during 5000ms backoff blocked too long: {:?}",
        started.elapsed()
    );
    assert_eq!(text(get(&response, "type")), "workflow");
    let flaky = task_by_id(&response, "flaky");
    assert_eq!(text(get(flaky, "state")), "cancelled", "{flaky:?}");
    assert_eq!(text(get(flaky, "termination")), "cancelled");
    assert_eq!(int(get(flaky, "status")), 130);
    assert_eq!(text(get(flaky, "reason")), "task_cancel");
    assert_eq!(int(get(flaky, "attempts")), 1);
    assert_eq!(array(get(flaky, "attempts_detail")).len(), 1);

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row16_conditions_gate_nodes_and_reject_invalid_predicates() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Invalid conditions must be rejected before running.
    for invalid_cond in [
        r#"{}"#,
        r#"{"task":"a"}"#,
        r#"{"task":"ghost","exit_code":{"eq":0}}"#,
    ] {
        session.send(&workflow_line(
            "w-badcond",
            "",
            &[
                r#"{"id":"a","command":"true"}"#.to_string(),
                format!(r#"{{"id":"b","command":"true","depends_on":["a"],"condition":{invalid_cond}}}"#),
            ],
        ));
        let err = session.json();
        assert_eq!(
            text(get(get(&err, "error"), "code")),
            "INVALID_CONDITION",
            "{err:?}"
        );
    }

    let wf_skip = workflow_line(
        "w-cond-skip",
        "",
        &[
            r#"{"id":"ok","command":"printf 'hello\\n'; printf 'warn\\n' >&2"}"#.to_string(),
            r#"{"id":"g_true","command":"echo TRUE_RAN","depends_on":["ok"],"condition":{"task":"ok","exit_code":{"eq":0},"stdout_contains":"ell","stdout_equals":"hello\n","stderr_contains":"war"}}"#.to_string(),
            r#"{"id":"g_false","command":"echo FALSE_RAN","depends_on":["ok"],"condition":{"task":"ok","exit_code":{"eq":0},"stdout_equals":"hello"}}"#.to_string(),
            r#"{"id":"after_false","command":"echo AFTER_RAN","depends_on":["g_false"]}"#.to_string(),
        ],
    );
    let r_skip = run_workflow(&mut session, &wf_skip);
    let gt = task_by_id(&r_skip, "g_true");
    assert_eq!(text(get(gt, "state")), "completed");
    assert_eq!(text(get(gt, "stdout")), "TRUE_RAN\n");
    assert_eq!(text(get(get(gt, "condition"), "outcome")), "true");

    let gf = task_by_id(&r_skip, "g_false");
    assert_eq!(text(get(gf, "state")), "skipped");
    assert_eq!(text(get(gf, "termination")), "skipped");
    assert_eq!(int(get(gf, "status")), 125);
    assert_eq!(text(get(gf, "reason")), "condition_false");
    assert_eq!(text(get(get(gf, "condition"), "outcome")), "false");

    let af = task_by_id(&r_skip, "after_false");
    assert_eq!(text(get(af, "state")), "skipped");
    assert_eq!(text(get(af, "reason")), "dependency_failed");

    let wf_cont = workflow_line(
        "w-cond-cont",
        r#""failure_policy":"continue","#,
        &[
            r#"{"id":"exit2","command":"exit 2"}"#.to_string(),
            r#"{"id":"g_ne","command":"echo NE_OK","depends_on":["exit2"],"condition":{"task":"exit2","exit_code":{"ne":0}}}"#.to_string(),
            r#"{"id":"g_in","command":"echo IN_OK","depends_on":["exit2"],"condition":{"task":"exit2","exit_code":{"in":[0,2]}}}"#.to_string(),
            r#"{"id":"g_miss","command":"echo MISS","depends_on":["exit2"],"condition":{"task":"exit2","exit_code":{"in":[0,1]}}}"#.to_string(),
        ],
    );
    let r_cont = run_workflow(&mut session, &wf_cont);
    assert_eq!(
        text(get(task_by_id(&r_cont, "g_ne"), "stdout")),
        "NE_OK\n"
    );
    assert_eq!(
        text(get(task_by_id(&r_cont, "g_in"), "stdout")),
        "IN_OK\n"
    );
    assert_eq!(
        text(get(task_by_id(&r_cont, "g_miss"), "state")),
        "skipped"
    );
    assert_eq!(
        text(get(task_by_id(&r_cont, "g_miss"), "reason")),
        "condition_false"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row17_conditions_evaluate_false_on_binary_or_truncated_streams() {
    let mut bin_session = Session::spawn(&[]);
    ready(&mut bin_session);

    let bin_wf = workflow_line(
        "w-bin",
        "",
        &[
            r#"{"id":"bin","command":"printf '\\000\\377hello'"}"#.to_string(),
            r#"{"id":"check","command":"echo RAN","depends_on":["bin"],"condition":{"task":"bin","stdout_contains":"hello"}}"#
                .to_string(),
        ],
    );
    let bin_resp = run_workflow(&mut bin_session, &bin_wf);
    let bin_node = task_by_id(&bin_resp, "bin");
    assert_eq!(text(get(bin_node, "stdout_encoding")), "base64");
    let check_node = task_by_id(&bin_resp, "check");
    assert_eq!(text(get(check_node, "state")), "skipped");
    assert_eq!(text(get(check_node, "reason")), "condition_false");
    assert_eq!(text(get(get(check_node, "condition"), "outcome")), "false");
    let (exit, leftovers, errors) = bin_session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");

    let mut trunc_session = Session::spawn(&["--max-capture-bytes", "16"]);
    ready(&mut trunc_session);

    let trunc_wf = workflow_line(
        "w-trunc",
        "",
        &[
            r#"{"id":"gen","command":"printf 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaNEEDLE'"}"#.to_string(),
            r#"{"id":"miss","command":"echo MISS","depends_on":["gen"],"condition":{"task":"gen","stdout_contains":"NEEDLE"}}"#
                .to_string(),
            r#"{"id":"hit","command":"echo HIT","depends_on":["gen"],"condition":{"task":"gen","stdout_contains":"aaaa"}}"#
                .to_string(),
        ],
    );
    let trunc_resp = run_workflow(&mut trunc_session, &trunc_wf);
    assert!(flag(get(task_by_id(&trunc_resp, "gen"), "stdout_truncated")));
    let miss = task_by_id(&trunc_resp, "miss");
    assert_eq!(text(get(miss, "state")), "skipped");
    assert_eq!(text(get(miss, "reason")), "condition_false");
    assert_eq!(text(get(get(miss, "condition"), "outcome")), "false");
    let hit = task_by_id(&trunc_resp, "hit");
    assert_eq!(text(get(hit, "state")), "completed");
    assert_eq!(text(get(hit, "stdout")), "HIT\n");
    assert_eq!(text(get(get(hit, "condition"), "outcome")), "true");

    let (exit, leftovers, errors) = trunc_session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

// -------------------------------- rows 13-15: no FS sandbox & resource locks

#[test]
fn row13_no_fs_sandbox_independent_and_locked_nodes_share_filesystem() {
    let dir = scratch("no-sandbox");
    let shared = dir.join("shared.log");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let cmd_a = format!("printf 'A1\\nA2\\n' >> {}", shared.display());
    let cmd_b = format!("printf 'B1\\nB2\\n' >> {}", shared.display());

    let unlocked = run_workflow(
        &mut session,
        &workflow_line(
            "w-fs-unlocked",
            "",
            &[
                format!(r#"{{"id":"a","command":{}}}"#, json::quote(&cmd_a)),
                format!(r#"{{"id":"b","command":{}}}"#, json::quote(&cmd_b)),
            ],
        ),
    );
    assert_eq!(int(get(&unlocked, "status")), 0);
    let content = std::fs::read_to_string(&shared).expect("read shared file");
    let mut lines: Vec<&str> = content.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, vec!["A1", "A2", "B1", "B2"], "{content:?}");

    let _ = std::fs::remove_file(&shared);
    let locked = run_workflow(
        &mut session,
        &workflow_line(
            "w-fs-locked",
            "",
            &[
                format!(
                    r#"{{"id":"a","command":{},"lock":"fs-test"}}"#,
                    json::quote(&cmd_a)
                ),
                format!(
                    r#"{{"id":"b","command":{},"lock":"fs-test"}}"#,
                    json::quote(&cmd_b)
                ),
            ],
        ),
    );
    assert_eq!(int(get(&locked, "status")), 0);
    let content = std::fs::read_to_string(&shared).expect("read locked shared file");
    let mut lines: Vec<&str> = content.lines().collect();
    lines.sort_unstable();
    assert_eq!(lines, vec!["A1", "A2", "B1", "B2"], "{content:?}");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row14_resource_locks_serialize_without_consuming_window_slots_and_release_on_spawn_error() {
    let dir = scratch("locks");
    let log = dir.join("events.log");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Part A: 3 locked + 3 unlocked nodes under `max_concurrency:4`.
    let locked_cmd = |id: &str| {
        format!(
            "printf 'START {id}\\n' >> {path}; sleep 0.1; printf 'END {id}\\n' >> {path}",
            path = log.display()
        )
    };
    let tasks = vec![
        format!(
            r#"{{"id":"l1","command":{},"lock":"L"}}"#,
            json::quote(&locked_cmd("l1"))
        ),
        format!(
            r#"{{"id":"l2","command":{},"lock":"L"}}"#,
            json::quote(&locked_cmd("l2"))
        ),
        format!(
            r#"{{"id":"l3","command":{},"lock":"L"}}"#,
            json::quote(&locked_cmd("l3"))
        ),
        r#"{"id":"u1","command":"sleep 0.1"}"#.to_string(),
        r#"{"id":"u2","command":"sleep 0.1"}"#.to_string(),
        r#"{"id":"u3","command":"sleep 0.1"}"#.to_string(),
    ];
    let resp = run_workflow(
        &mut session,
        &workflow_line("w-locks-a", r#""max_concurrency":4,"#, &tasks),
    );
    assert_eq!(int(get(&resp, "status")), 0, "{resp:?}");
    assert_eq!(int(get(&resp, "started_tasks")), 6);

    let events: Vec<String> = std::fs::read_to_string(&log)
        .expect("read lock log")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(events.len(), 6, "{events:?}");
    for pair in events.chunks_exact(2) {
        let start_id = pair[0].strip_prefix("START ").expect("START line");
        let end_id = pair[1].strip_prefix("END ").expect("END line");
        assert_eq!(
            start_id, end_id,
            "locked nodes overlapped in {events:?}"
        );
    }

    // Part B: lock wait must not occupy a window slot (`max_concurrency:2`).
    let no_starve = run_workflow(
        &mut session,
        &workflow_line(
            "w-locks-b",
            r#""max_concurrency":2,"#,
            &[
                r#"{"id":"A","command":"sleep 0.35","lock":"L"}"#.to_string(),
                r#"{"id":"B","command":"echo B","lock":"L"}"#.to_string(),
                r#"{"id":"C","command":"echo C"}"#.to_string(),
                r#"{"id":"D","command":"echo D"}"#.to_string(),
            ],
        ),
    );
    assert_eq!(int(get(&no_starve, "status")), 0);
    let detail_a = &array(get(task_by_id(&no_starve, "A"), "attempts_detail"))[0];
    let detail_b = &array(get(task_by_id(&no_starve, "B"), "attempts_detail"))[0];
    let detail_c = &array(get(task_by_id(&no_starve, "C"), "attempts_detail"))[0];
    let detail_d = &array(get(task_by_id(&no_starve, "D"), "attempts_detail"))[0];
    let a_end = int(get(detail_a, "started_ms")) + int(get(detail_a, "duration_ms"));
    let b_start = int(get(detail_b, "started_ms"));
    let c_end = int(get(detail_c, "started_ms")) + int(get(detail_c, "duration_ms"));
    let d_end = int(get(detail_d, "started_ms")) + int(get(detail_d, "duration_ms"));
    assert!(
        a_end >= 300 && b_start >= 300,
        "B started before locked A finished: a={detail_a:?} b={detail_b:?}"
    );
    assert!(
        c_end < 250 && d_end < 250,
        "unlocked C/D starved while B waited for lock L: a={detail_a:?} c={detail_c:?} d={detail_d:?}"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");

    // Part C: spawn failure releases the acquired lock.
    let mut err_session =
        Session::spawn_with_env(&[], &[("SHARKY_WORKFLOW_TEST_SPAWN_FAILURE", "bad_holder")]);
    ready(&mut err_session);
    let spawn_err_resp = run_workflow(
        &mut err_session,
        &workflow_line(
            "w-locks-spawnerr",
            "",
            &[
                r#"{"id":"bad_holder","command":"echo NEVER","lock":"L"}"#.to_string(),
                r#"{"id":"next_holder","command":"echo OK","lock":"L"}"#.to_string(),
            ],
        ),
    );
    let bad = task_by_id(&spawn_err_resp, "bad_holder");
    assert_eq!(text(get(bad, "state")), "spawn_error", "{bad:?}");
    assert_eq!(int(get(bad, "status")), 126);
    let next = task_by_id(&spawn_err_resp, "next_holder");
    assert_eq!(text(get(next, "state")), "completed", "{next:?}");
    assert_eq!(int(get(next, "status")), 0);
    assert_eq!(text(get(next, "stdout")), "OK\n");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = err_session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row15_lock_is_held_across_retry_backoff_and_released_on_backoff_cancel() {
    let dir = scratch("lock-retry");
    let count = dir.join("count");
    let log = dir.join("order.log");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let first_cmd = format!(
        "n=0; [ -f {cnt} ] && read n < {cnt}; n=$((n + 1)); echo $n > {cnt}; echo first_$n >> {log}; [ $n -ge 2 ]",
        cnt = count.display(),
        log = log.display()
    );
    let second_cmd = format!("echo second >> {}", log.display());
    let resp = run_workflow(
        &mut session,
        &workflow_line(
            "w-lock-retry",
            "",
            &[
                format!(
                    r#"{{"id":"first","command":{},"lock":"L","retry":{{"max_attempts":2,"on":["failure"],"backoff_ms":200}}}}"#,
                    json::quote(&first_cmd)
                ),
                format!(
                    r#"{{"id":"second","command":{},"lock":"L"}}"#,
                    json::quote(&second_cmd)
                ),
            ],
        ),
    );
    assert_eq!(int(get(&resp, "status")), 0, "{resp:?}");
    let recorded: Vec<String> = std::fs::read_to_string(&log)
        .expect("read order log")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        recorded,
        vec!["first_1", "first_2", "second"],
        "lock L was released during retry backoff: {recorded:?}"
    );

    // Part B: cancelling a retrying lock holder during backoff releases the lock promptly.
    let started = Instant::now();
    session.send(&workflow_line(
        "w-lock-cancel",
        "",
        &[
            r#"{"id":"holder","command":"exit 1","lock":"L","retry":{"max_attempts":2,"on":["failure"],"backoff_ms":5000}}"#
                .to_string(),
            r#"{"id":"waiter","command":"echo WAITER_OK","lock":"L"}"#.to_string(),
        ],
    ));
    std::thread::sleep(Duration::from_millis(180));
    session.send(&cancel_line(
        "c-holder",
        "w-lock-cancel",
        Some("holder"),
        None,
    ));

    let ack = session.json_within(2000).expect("cancel ack");
    assert_eq!(text(get(&ack, "type")), "ack");
    let cancel_resp = session.json_within(2500).expect("workflow after backoff cancel");
    assert!(
        started.elapsed() < Duration::from_millis(2500),
        "cancel took too long: {:?}",
        started.elapsed()
    );
    assert_eq!(
        text(get(task_by_id(&cancel_resp, "holder"), "state")),
        "cancelled"
    );
    assert_eq!(
        text(get(task_by_id(&cancel_resp, "holder"), "reason")),
        "task_cancel"
    );
    let waiter = task_by_id(&cancel_resp, "waiter");
    assert_eq!(text(get(waiter, "state")), "completed", "{waiter:?}");
    assert_eq!(text(get(waiter, "stdout")), "WAITER_OK\n");

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

// ------------------------- rows 11, 12, 19, 21: cancel, orphans, budget, edges

#[test]
fn row11_whole_workflow_cancel_stops_running_blocks_rest_and_reaps_pgids() {
    let dir = scratch("cancel-all");
    let pid_a = dir.join("a1.pid");
    let pid_b = dir.join("b1.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let wf = workflow_line(
        "w-cancel-all",
        r#""max_concurrency":2,"#,
        &[
            format!(
                r#"{{"id":"a1","command":"sh -c 'echo $$ > {}; exec sleep 30'"}}"#,
                pid_a.display()
            ),
            r#"{"id":"a2","command":"echo A2","depends_on":["a1"]}"#.to_string(),
            r#"{"id":"a3","command":"echo A3","depends_on":["a2"]}"#.to_string(),
            format!(
                r#"{{"id":"b1","command":"sh -c 'echo $$ > {}; exec sleep 30'"}}"#,
                pid_b.display()
            ),
            r#"{"id":"b2","command":"echo B2","depends_on":["b1"]}"#.to_string(),
            r#"{"id":"b3","command":"echo B3","depends_on":["b2"]}"#.to_string(),
        ],
    );
    session.send(&wf);

    let pa = read_pid_within(&pid_a, 3000);
    let pb = read_pid_within(&pid_b, 3000);
    session.send(&cancel_line("c-all", "w-cancel-all", None, Some(200)));

    let ack = session.json_within(2000).expect("cancel ack");
    assert_eq!(text(get(&ack, "type")), "ack");
    let resp = session.json_within(4000).expect("cancelled workflow");

    let pa_gone = pa.is_some_and(|p| gone_within(p, 2000));
    let pb_gone = pb.is_some_and(|p| gone_within(p, 2000));
    for p in [pa, pb].into_iter().flatten() {
        if !gone(p) {
            unsafe {
                nix::libc::kill(p as i32, nix::libc::SIGKILL);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(pa_gone, "a1 leaked after workflow cancel");
    assert!(pb_gone, "b1 leaked after workflow cancel");
    assert_eq!(text(get(&resp, "state")), "cancelled", "{resp:?}");
    assert_eq!(int(get(&resp, "status")), 143, "{resp:?}");
    assert_eq!(int(get(&resp, "blocked_tasks")), 4, "{resp:?}");

    for id in ["a1", "b1"] {
        let entry = task_by_id(&resp, id);
        assert_eq!(text(get(entry, "state")), "cancelled", "{entry:?}");
        assert_eq!(int(get(entry, "status")), 143);
        assert_eq!(text(get(entry, "reason")), "group_cancel");
        let pgid = int(get(entry, "pgid"));
        assert!(pgid_gone_within(pgid, 2000), "{id} pgid {pgid} survived");
    }
    for id in ["a2", "a3", "b2", "b3"] {
        let entry = task_by_id(&resp, id);
        assert_eq!(text(get(entry, "state")), "blocked", "{entry:?}");
        assert_eq!(text(get(entry, "termination")), "cancelled");
        assert_eq!(int(get(entry, "status")), 130);
        assert_eq!(text(get(entry, "reason")), "group_cancel");
    }

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row12_cancel_escalates_sigkill_to_term_ignoring_and_background_descendants() {
    let dir = scratch("orphans");
    let pid_stubborn = dir.join("stubborn.pid");
    let pid_bg = dir.join("bg.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let wf = workflow_line(
        "w-esc",
        r#""max_concurrency":2,"#,
        &[
            format!(
                r#"{{"id":"stubborn","command":"sh -c 'trap \"\" TERM; echo $$ > {}; exec sleep 30'"}}"#,
                pid_stubborn.display()
            ),
            format!(
                r#"{{"id":"bg","command":"sh -c 'sleep 30 & echo $! > {}; wait'"}}"#,
                pid_bg.display()
            ),
        ],
    );
    let started = Instant::now();
    session.send(&wf);

    let p_stubborn = read_pid_within(&pid_stubborn, 3000);
    let p_bg = read_pid_within(&pid_bg, 3000);
    session.send(&cancel_line("c-esc", "w-esc", None, Some(250)));

    let ack = session.json_within(2000).expect("cancel ack");
    assert_eq!(text(get(&ack, "type")), "ack");
    let resp = session.json_within(4000).expect("escalated cancel workflow");
    let elapsed = started.elapsed();

    let s_gone = p_stubborn.is_some_and(|p| gone_within(p, 2000));
    let b_gone = p_bg.is_some_and(|p| gone_within(p, 2000));
    for p in [p_stubborn, p_bg].into_iter().flatten() {
        if !gone(p) {
            unsafe {
                nix::libc::kill(p as i32, nix::libc::SIGKILL);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(elapsed < Duration::from_millis(3500), "{elapsed:?}");
    assert!(s_gone, "TERM-ignoring task {p_stubborn:?} survived");
    assert!(b_gone, "background descendant {p_bg:?} survived");
    assert_eq!(text(get(&resp, "state")), "cancelled", "{resp:?}");
    for id in ["stubborn", "bg"] {
        let entry = task_by_id(&resp, id);
        let pgid = int(get(entry, "pgid"));
        assert!(pgid_gone_within(pgid, 1000), "{id} pgid {pgid} survived");
    }

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row19_output_budget_truncates_explicitly_and_superseded_retries_do_not_spend_budget() {
    // Part A: total output exceeds 4096-byte budget across 3 nodes.
    let mut session = Session::spawn(&["--max-parallel-output-bytes", "4096"]);
    ready(&mut session);

    let emit_8k = "p=xxxxxxxxxxxxxxxx; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; printf '%s%s' \"$p\" \"$p\"";
    let resp = run_workflow(
        &mut session,
        &workflow_line(
            "w-budget",
            r#""max_concurrency":1,"#,
            &[
                format!(r#"{{"id":"n1","command":{}}}"#, json::quote(emit_8k)),
                format!(r#"{{"id":"n2","command":{}}}"#, json::quote(emit_8k)),
                format!(r#"{{"id":"n3","command":{}}}"#, json::quote(emit_8k)),
            ],
        ),
    );
    assert!(flag(get(&resp, "output_truncated")), "{resp:?}");
    let total_rendered: usize = ["n1", "n2", "n3"]
        .iter()
        .map(|id| {
            let entry = task_by_id(&resp, id);
            assert_eq!(int(get(entry, "stdout_bytes")), 8192, "{entry:?}");
            text(get(entry, "stdout")).len()
        })
        .sum();
    assert!(
        total_rendered <= 4096,
        "budget exceeded: {total_rendered} > 4096"
    );
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");

    // Part B: a retried attempt's 4096-byte stdout is superseded and does not
    // consume the group's shared budget.
    let dir = scratch("retry-budget");
    let count = dir.join("count");
    let mut retry_session = Session::spawn(&["--max-parallel-output-bytes", "4096"]);
    ready(&mut retry_session);

    let retry_cmd = format!(
        "n=0; [ -f {cnt} ] && read n < {cnt}; n=$((n + 1)); echo $n > {cnt}; if [ $n -eq 1 ]; then p=xxxxxxxxxxxxxxxx; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; printf '%s' \"$p\"; exit 1; else echo ok; fi",
        cnt = count.display()
    );
    let emit_2k = "p=xxxxxxxxxxxxxxxx; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; p=\"$p$p$p$p\"; printf '%s%s' \"$p\" \"$p\"";
    let r2 = run_workflow(
        &mut retry_session,
        &workflow_line(
            "w-retry-budget",
            "",
            &[
                format!(
                    r#"{{"id":"retried","command":{},"retry":{{"max_attempts":2,"on":["failure"]}}}}"#,
                    json::quote(&retry_cmd)
                ),
                format!(
                    r#"{{"id":"after","command":{},"depends_on":["retried"]}}"#,
                    json::quote(emit_2k)
                ),
            ],
        ),
    );
    assert!(!flag(get(&r2, "output_truncated")), "{r2:?}");
    let retried = task_by_id(&r2, "retried");
    assert_eq!(int(get(retried, "attempts")), 2);
    assert_eq!(
        int(get(&array(get(retried, "attempts_detail"))[0], "stdout_bytes")),
        4096
    );
    assert_eq!(text(get(retried, "stdout")), "ok\n");
    let after = task_by_id(&r2, "after");
    assert_eq!(text(get(after, "stdout")).len(), 2048);

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = retry_session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}

#[test]
fn row21_protocol_edges_cancel_taxonomy_unknown_keys_and_queued_workflow_cancel() {
    let dir = scratch("edges");
    let pid_a = dir.join("a.pid");
    let b_ran = dir.join("b.ran");
    let q_ran = dir.join("q.ran");

    let mut session = Session::spawn(&[]);
    let r = ready(&mut session);
    let protocol = get(&r, "protocol");
    let ops: Vec<&str> = array(get(protocol, "ops")).iter().map(text).collect();
    assert!(ops.contains(&"workflow"), "{ops:?}");
    let features: Vec<&str> = array(get(protocol, "features")).iter().map(text).collect();
    for feat in [
        "workflow",
        "workflow_retry",
        "workflow_condition",
        "workflow_lock",
        "workflow_task_cancel",
    ] {
        assert!(features.contains(&feat), "missing {feat} in {features:?}");
    }
    let limits = get(protocol, "limits");
    assert_eq!(int(get(limits, "max_workflow_attempts")), 8);
    assert_eq!(int(get(limits, "max_workflow_backoff_ms")), 60_000);

    // Unknown top-level request keys on workflow are ignored for forward compat.
    let unknown_keys = run_workflow(
        &mut session,
        r#"{"schema_version":1,"id":"w-compat","op":"workflow","stream":true,"future_key":42,"tasks":[{"id":"t","command":"echo compat"}]}"#,
    );
    assert_eq!(text(get(task_by_id(&unknown_keys, "t"), "stdout")), "compat\n");

    // `cancel` with `task` on a running `exec` returns `INVALID_REQUEST`.
    session.send(r#"{"schema_version":1,"id":"e1","op":"exec","command":"sleep 0.3"}"#);
    std::thread::sleep(Duration::from_millis(60));
    session.send(&cancel_line("c-bad", "e1", Some("t"), None));
    let bad = session.json_within(2000).expect("bad cancel");
    assert_eq!(text(get(get(&bad, "error"), "code")), "INVALID_REQUEST");
    let e1 = session.json_within(2000).expect("e1 result");
    assert_eq!(text(get(&e1, "type")), "result");

    // Single-task cancel cascade + unknown task id while running + post-finish CANCEL_TOO_LATE.
    session.send(&workflow_line(
        "w-edge",
        r#""max_concurrency":2,"#,
        &[
            format!(
                r#"{{"id":"a","command":"sh -c 'echo $$ > {}; exec sleep 30'"}}"#,
                pid_a.display()
            ),
            format!(
                r#"{{"id":"b","command":"touch {}","depends_on":["a"]}}"#,
                b_ran.display()
            ),
            r#"{"id":"c","command":"sleep 0.25; echo C_OK"}"#.to_string(),
        ],
    ));
    let pa = read_pid_within(&pid_a, 3000);
    session.send(&cancel_line("c-ghost", "w-edge", Some("ghost"), None));
    let ghost = session.json_within(2000).expect("unknown task cancel");
    assert_eq!(
        text(get(get(&ghost, "error"), "code")),
        "CANCEL_UNKNOWN_ID"
    );

    session.send(&cancel_line("c-a", "w-edge", Some("a"), Some(150)));
    let ack = session.json_within(2000).expect("cancel a ack");
    assert_eq!(text(get(&ack, "type")), "ack");
    let edge = session.json_within(4000).expect("w-edge response");
    let pa_gone = pa.is_some_and(|p| gone_within(p, 2000));
    if let Some(p) = pa
        && !gone(p)
    {
        unsafe {
            nix::libc::kill(p as i32, nix::libc::SIGKILL);
        }
    }
    assert!(pa_gone, "cancelled task a leaked");
    assert!(!b_ran.exists(), "dependent of cancelled task ran");
    assert_eq!(text(get(&edge, "state")), "completed", "{edge:?}");
    assert_eq!(int(get(&edge, "status")), 143, "{edge:?}");
    assert_eq!(text(get(task_by_id(&edge, "a"), "state")), "cancelled");
    assert_eq!(text(get(task_by_id(&edge, "a"), "reason")), "task_cancel");
    assert_eq!(text(get(task_by_id(&edge, "b"), "state")), "skipped");
    assert_eq!(
        text(get(task_by_id(&edge, "b"), "reason")),
        "dependency_failed"
    );
    assert_eq!(text(get(task_by_id(&edge, "c"), "stdout")), "C_OK\n");

    session.send(&cancel_line("c-late", "w-edge", Some("a"), None));
    let late = session.json();
    assert_eq!(text(get(get(&late, "error"), "code")), "CANCEL_TOO_LATE");

    // Queued workflow cancelled before it starts never spawns.
    session.send(r#"{"schema_version":1,"id":"e-slow","op":"exec","command":"sleep 0.35; echo SLOW"}"#);
    std::thread::sleep(Duration::from_millis(60));
    session.send(&workflow_line(
        "w-queued",
        "",
        &[
            format!(r#"{{"id":"q1","command":"touch {}"}}"#, q_ran.display()),
            r#"{"id":"q2","command":"true","depends_on":["q1"]}"#.to_string(),
        ],
    ));
    session.send(&cancel_line("c-queued", "w-queued", None, None));
    let q_ack = session.json_within(2000).expect("queued cancel ack");
    assert_eq!(text(get(&q_ack, "type")), "ack");
    let e_slow = session.json_within(2000).expect("e-slow");
    assert_eq!(text(get(&e_slow, "stdout")), "SLOW\n");
    let q_wf = session.json_within(2000).expect("w-queued");
    assert_eq!(text(get(&q_wf, "type")), "workflow", "{q_wf:?}");
    assert_eq!(text(get(&q_wf, "state")), "cancelled");
    assert_eq!(int(get(&q_wf, "status")), 130);
    assert_eq!(int(get(&q_wf, "started_tasks")), 0);
    assert_eq!(int(get(&q_wf, "blocked_tasks")), 2);
    assert!(!q_ran.exists(), "queued cancelled workflow spawned a node");
    for id in ["q1", "q2"] {
        let entry = task_by_id(&q_wf, id);
        assert_eq!(text(get(entry, "state")), "blocked");
        assert_eq!(int(get(entry, "status")), 130);
        assert_eq!(text(get(entry, "reason")), "group_cancel");
    }

    // Group timeout must not retry a timed-out node.
    let started = Instant::now();
    let gto = run_workflow(
        &mut session,
        &workflow_line(
            "w-gto",
            r#""timeout_ms":150,"#,
            &[r#"{"id":"t","command":"sleep 10","retry":{"max_attempts":3,"on":["timeout"]}}"#.to_string()],
        ),
    );
    assert!(started.elapsed() < Duration::from_millis(3000));
    let t = task_by_id(&gto, "t");
    assert_eq!(text(get(t, "state")), "timeout");
    assert_eq!(int(get(t, "status")), 124);
    assert_eq!(int(get(t, "attempts")), 1);

    let _ = std::fs::remove_dir_all(&dir);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(errors.is_empty(), "{errors}");
}
