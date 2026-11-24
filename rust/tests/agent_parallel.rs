//! End-to-end tests for `op:"parallel"` (design §9).
//!
//! The gate is the design's table rows owned by the wait-all executor: 1
//! (request order), 2 (window never exceeded), 3 (one failure does not corrupt
//! the others), 4 (wait-all completes everything), 9 (binary and truncation),
//! 10 (100 tasks), 12 (parent isolation), 13 (no filesystem sandbox) and 14
//! (per-task cwd/env), plus the aggregate status rule and the wire rules of
//! §2.3/§2.4/§6.
//!
//! Every task is `echo`, `printf`, `seq`, `sleep`, `true` or `env` work. No test
//! runs Cargo inside a task: a shared target directory serializes on the Cargo
//! lock, and the design explicitly refuses to advertise that as parallelism
//! (§8). Deadlock discipline is the existing one: one write per read.

mod common;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use common::agent::{Session, json};

// -------------------------------------------------------------------- helpers

fn scratch(name: &str) -> PathBuf {
    let dir = std::path::Path::new("/tmp/opencode").join(format!(
        "sharky-agent-parallel-{name}-{}",
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

fn ready(session: &mut Session) {
    let response = session.json();
    assert_eq!(
        response.get("type").and_then(json::Value::as_str),
        Some("ready"),
        "{response:?}"
    );
}

/// `{"id":...,"command":...}` — the minimum a task needs.
fn task(id: &str, command: &str) -> String {
    format!(
        r#"{{"id":{},"command":{}}}"#,
        json::quote(id),
        json::quote(command)
    )
}

/// A `parallel` request. `fields` carries the request-level fields with their
/// trailing commas already in place (`""` for the defaults).
fn parallel_line(id: &str, fields: &str, tasks: &[String]) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"parallel",{fields}"tasks":[{}]}}"#,
        json::quote(id),
        tasks.join(",")
    )
}

fn task_by_id<'a>(response: &'a json::Value, id: &str) -> &'a json::Value {
    array(get(response, "tasks"))
        .iter()
        .find(|task| text(get(task, "id")) == id)
        .unwrap_or_else(|| panic!("no task {id} in {response:?}"))
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

/// `{"op":"cancel"}` with the optional task scope and grace.
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

/// Reads one response and asserts it is the ack of `op`.
fn ack_frame(session: &mut Session, op: &str) -> json::Value {
    let response = session.json();
    assert_eq!(
        response.get("type").and_then(json::Value::as_str),
        Some("ack"),
        "{response:?}"
    );
    assert_eq!(text(get(&response, "op")), op, "{response:?}");
    response
}

/// `kill(pid, 0)` reports ESRCH once the process is gone; a reaped-but-not-yet-
/// collected child still answers signal 0, so its `Z` state also counts.
fn gone(pid: i64) -> bool {
    if pid <= 0 {
        return true;
    }
    let pid = pid as i32;
    // SAFETY: signal 0 only performs an existence check.
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

/// Open descriptors of the agent process, from `/proc`.
fn open_fds(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .expect("agent /proc/<pid>/fd")
        .count()
}

/// Children of `pid` that are already dead but not collected. A `<defunct>`
/// child means the group leaked a status.
fn zombie_children(pid: u32) -> Vec<String> {
    let expected = pid.to_string();
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // `pid (comm) state ppid ...`; `comm` may contain spaces and parens, so
        // the fields start after the last ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.first() == Some(&"Z") && fields.get(1) == Some(&expected.as_str()) {
            found.push(name.to_string());
        }
    }
    found
}

// ------------------------------- table 1: request order and own output

#[test]
fn ten_tasks_come_back_in_request_order_with_their_own_output() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let first = exec(&mut session, "cwd", "pwd");
    let session_cwd = text(get(&first, "cwd")).to_string();

    // Ids whose lexicographic order differs from the request order, so a
    // response sorted by id instead of by request index would fail.
    let ids = ["t9", "t1", "t7", "t3", "t5", "t0", "t8", "t2", "t6", "t4"];
    let tasks: Vec<String> = ids
        .iter()
        .map(|id| task(id, &format!("printf 'out-{id}\\n'")))
        .collect();
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":3,"#, &tasks));

    assert_eq!(text(get(&response, "id")), "p1");
    assert_eq!(text(get(&response, "op")), "parallel");
    assert_eq!(text(get(&response, "policy")), "wait-all");
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "status")), 0);
    assert_eq!(int(get(&response, "max_concurrency")), 3);
    assert_eq!(int(get(&response, "started_tasks")), 10);
    assert!(int(get(&response, "duration_us")) >= 0);

    let entries = array(get(&response, "tasks"));
    assert_eq!(entries.len(), 10, "one entry per requested task");
    for (index, id) in ids.iter().enumerate() {
        let entry = &entries[index];
        assert_eq!(
            text(get(entry, "id")),
            *id,
            "task {index} is not in request order"
        );
        assert_eq!(int(get(entry, "status")), 0, "{id}: {entry:?}");
        assert_eq!(text(get(entry, "state")), "completed");
        assert_eq!(text(get(entry, "termination")), "exited");
        assert_eq!(text(get(entry, "stdout")), format!("out-{id}\n"));
        assert_eq!(text(get(entry, "stdout_encoding")), "utf8");
        assert_eq!(int(get(entry, "stdout_bytes")), 7);
        assert_eq!(text(get(entry, "stderr")), "");
        assert!(!flag(get(entry, "stdout_truncated")));
        assert!(int(get(entry, "pgid")) > 0, "{id}: missing pgid");
        assert!(int(get(entry, "duration_us")) >= 0);
        assert_eq!(text(get(entry, "cwd")), session_cwd);
    }
    assert!(!flag(get(&response, "output_truncated")));

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ------------------------- table 2: the window is real and never exceeded

#[test]
fn the_concurrency_window_is_never_exceeded() {
    let dir = scratch("window");
    let log = dir.join("log");
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // Each task records the nanosecond it starts and ends. Six tasks with a
    // window of two: the sweep below must see a maximum of exactly two
    // overlapping intervals, and at least one overlap (no serialization).
    let cwd = dir.to_string_lossy();
    let tasks: Vec<String> = (0..6)
        .map(|index| {
            let id = format!("w{index}");
            let command = format!(
                "printf 'S {id} %s\\n' $(date +%s%N) >> log; \
                 sleep 0.2; \
                 printf 'E {id} %s\\n' $(date +%s%N) >> log"
            );
            format!(
                r#"{{"id":{},"command":{},"cwd":{}}}"#,
                json::quote(&id),
                json::quote(&command),
                json::quote(&cwd)
            )
        })
        .collect();
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    assert_eq!(int(get(&response, "started_tasks")), 6);
    assert_eq!(int(get(&response, "status")), 0);

    let log = std::fs::read_to_string(&log).expect("task event log");
    let mut events: Vec<(u128, i32)> = Vec::new();
    let mut pairs = 0;
    for line in log.lines() {
        let mut fields = line.split_whitespace();
        let kind = fields.next().expect("event kind");
        let _id = fields.next().expect("event id");
        let nanos: u128 = fields.next().expect("timestamp").parse().expect("nanos");
        match kind {
            "S" => events.push((nanos, 1)),
            "E" => {
                events.push((nanos, -1));
                pairs += 1;
            }
            other => panic!("unexpected event {other}"),
        }
    }
    assert_eq!(pairs, 6, "every task must report one end: {log}");
    // Ends before starts on a tie, so back-to-back tasks are not an overlap.
    events.sort_by_key(|(nanos, delta)| (*nanos, *delta));
    let mut current = 0i32;
    let mut maximum = 0i32;
    for (_, delta) in events {
        current += delta;
        maximum = maximum.max(current);
    }
    assert_eq!(maximum, 2, "the window was exceeded: {log}");
    assert!(
        log.lines().count() == 12,
        "each task writes exactly one start and one end: {log}"
    );

    session.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------- table 3: one failure does not corrupt the others

#[test]
fn a_failing_task_does_not_corrupt_the_others() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("ok", "echo a"),
        task("no", "false"),
        task("out", "printf 'b\\n'"),
        task("exit3", "exit 3"),
    ];
    let response = session.parallel(&parallel_line("p1", "", &tasks));

    assert_eq!(text(get(task_by_id(&response, "ok"), "stdout")), "a\n");
    assert_eq!(int(get(task_by_id(&response, "ok"), "status")), 0);
    assert_eq!(int(get(task_by_id(&response, "no"), "status")), 1);
    assert_eq!(int(get(task_by_id(&response, "no"), "stderr_bytes")), 0);
    assert_eq!(text(get(task_by_id(&response, "out"), "stdout")), "b\n");
    assert_eq!(int(get(task_by_id(&response, "out"), "status")), 0);
    assert_eq!(int(get(task_by_id(&response, "exit3"), "status")), 3);
    for id in ["no", "exit3"] {
        assert_eq!(text(get(task_by_id(&response, id), "state")), "completed");
        assert_eq!(
            text(get(task_by_id(&response, id), "termination")),
            "exited"
        );
    }
    // §2.4: the aggregate is the status of the first failed task in request
    // order, and the group itself completed.
    assert_eq!(int(get(&response, "status")), 1);
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "started_tasks")), 4);
    session.finish();
}

// --------------------------- table 4: wait-all runs everything

#[test]
fn wait_all_lets_the_other_tasks_finish() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![task("fails", "false"), task("slow", "sleep 0.3; echo late")];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    let slow = task_by_id(&response, "slow");
    assert_eq!(int(get(slow, "status")), 0);
    assert_eq!(text(get(slow, "stdout")), "late\n");
    assert!(
        int(get(slow, "duration_ms")) >= 250,
        "wait-all must not cancel the sibling: {slow:?}"
    );
    assert_eq!(
        int(get(&response, "status")),
        1,
        "the failure still publishes"
    );
    session.finish();
}

/// The window refills on ANY completion, not on the oldest one. One long task
/// is spawned FIRST, so a driver that blocks on the oldest running leader
/// serializes the short tasks into waves behind it; a driver that polls refills
/// each freed slot as soon as its task ends.
///
/// Ideal: the twelve 0.15 s tasks fit in the three free slots while the 1 s
/// task runs, so the group ends with the long task (~1.05 s). Wave-shaped: the
/// shorts wait for the long one and then run in batches of four (1.0 + 3 × 0.15
/// ≈ 1.45 s). 1.3 s separates the two shapes with ~25% margin on each side.
#[test]
fn wait_all_refills_the_window_on_any_completion() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let mut tasks = vec![task("long", "sleep 1")];
    for index in 0..12 {
        tasks.push(task(&format!("s{index}"), "sleep 0.15"));
    }
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":4,"#, &tasks));
    assert_eq!(int(get(&response, "started_tasks")), 13);
    assert_eq!(
        int(get(&response, "status")),
        0,
        "every task must succeed: {response:?}"
    );
    let elapsed = int(get(&response, "duration_ms"));
    assert!(
        elapsed < 1300,
        "the window did not refill on the first completion: {elapsed} ms \
         (a 1 s task first plus twelve 0.15 s tasks in a window of 4 should \
         end with the long task, not after three more waves)"
    );
    session.finish();
}

// --------------------- table 9: binary, truncation and a clean wire

#[test]
fn binary_and_long_streams_stay_inside_the_response() {
    let mut session = Session::spawn(&["--max-capture-bytes", "1024"]);
    ready(&mut session);
    let tasks = vec![
        task("bin", "printf '\\x00\\xff\\xfe'"),
        task("long", "seq 1 200000"),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));

    let binary = task_by_id(&response, "bin");
    assert_eq!(text(get(binary, "stdout_encoding")), "base64");
    assert_eq!(
        json::base64_decode(text(get(binary, "stdout"))).expect("valid base64"),
        vec![0x00, 0xff, 0xfe]
    );
    assert_eq!(int(get(binary, "stdout_bytes")), 3);
    assert!(!flag(get(binary, "stdout_truncated")));

    let long = task_by_id(&response, "long");
    assert!(flag(get(long, "stdout_truncated")));
    assert_eq!(text(get(long, "stdout")).len(), 1024);
    assert!(
        int(get(long, "stdout_bytes")) > 1024,
        "the real size must survive truncation: {long:?}"
    );

    // No task may write to the protocol: the group answers exactly one line and
    // the transport stays silent until the next request.
    assert!(
        session.recv_within(150).is_none(),
        "a task produced an extra protocol line"
    );
    let alive = exec(&mut session, "after", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn the_response_budget_truncates_later_tasks_explicitly() {
    // Window 1 keeps the budget order equal to request order, so "later" is
    // deterministic (a wider window would make the charge order depend on the
    // completion order).
    let mut session = Session::spawn(&["--max-parallel-output-bytes", "64"]);
    ready(&mut session);
    let tasks = vec![task("first", "seq 1 100"), task("second", "echo second")];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":1,"#, &tasks));

    let first = task_by_id(&response, "first");
    assert!(flag(get(first, "stdout_truncated")), "{first:?}");
    assert_eq!(text(get(first, "stdout")).len(), 64);
    assert_eq!(int(get(first, "stdout_bytes")), 292);

    let second = task_by_id(&response, "second");
    assert_eq!(text(get(second, "stdout")), "", "budget is exhausted");
    assert_eq!(
        int(get(second, "stdout_bytes")),
        7,
        "the size is still real"
    );
    assert!(flag(get(second, "stdout_truncated")));
    assert!(flag(get(&response, "output_truncated")));
    session.finish();
}

#[test]
fn a_budget_that_meets_the_capture_cap_is_still_a_budget_cut() {
    // Budget and per-stream cap are the same number: the read limit is the cap,
    // so the old "limit is below the cap" test never saw the cut, even though
    // the budget is exactly what the group has left.
    let mut session = Session::spawn(&[
        "--max-capture-bytes",
        "1024",
        "--max-parallel-output-bytes",
        "1024",
    ]);
    ready(&mut session);
    let response = session.parallel(&parallel_line("p1", "", &[task("big", "seq 1 1000")]));
    let big = task_by_id(&response, "big");
    assert!(flag(get(big, "stdout_truncated")), "{big:?}");
    assert_eq!(text(get(big, "stdout")).len(), 1024);
    assert!(
        flag(get(&response, "output_truncated")),
        "the budget was exactly exhausted by this cut: {response:?}"
    );
    session.finish();

    // The control: a stream cut by its own cap with the budget untouched is the
    // task's business, not the group's (§6).
    let mut roomy = Session::spawn(&["--max-capture-bytes", "1024"]);
    ready(&mut roomy);
    let response = roomy.parallel(&parallel_line("p2", "", &[task("big", "seq 1 1000")]));
    assert!(flag(get(task_by_id(&response, "big"), "stdout_truncated")));
    assert!(
        !flag(get(&response, "output_truncated")),
        "the group budget did not cut anything: {response:?}"
    );
    roomy.finish();
}

#[test]
fn a_task_stdin_payload_arrives_as_exact_bytes() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        format!(
            r#"{{"id":"contains","command":"cat","stdin":{}}}"#,
            json::quote("hello task\n")
        ),
        r#"{"id":"none","command":"cat"}"#.to_string(),
    ];
    let response = session.parallel(&parallel_line("p1", "", &tasks));
    assert_eq!(
        text(get(task_by_id(&response, "contains"), "stdout")),
        "hello task\n"
    );
    assert_eq!(
        text(get(task_by_id(&response, "none"), "stdout")),
        "",
        "a task without stdin reads /dev/null"
    );
    session.finish();
}

// ------------------------------------ table 10: 100 tasks, no leak

#[test]
fn a_hundred_tasks_leave_the_session_healthy() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let pid = session.pid();
    let fds_before = open_fds(pid);
    let tasks: Vec<String> = (0..100)
        .map(|index| task(&format!("t{index}"), "true"))
        .collect();
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":8,"#, &tasks));
    assert_eq!(int(get(&response, "started_tasks")), 100);
    assert_eq!(int(get(&response, "status")), 0);
    let entries = array(get(&response, "tasks"));
    assert_eq!(entries.len(), 100);
    assert!(entries.iter().all(|task| int(get(task, "status")) == 0));
    assert!(
        entries
            .iter()
            .all(|task| text(get(task, "state")) == "completed")
    );

    assert_eq!(
        open_fds(pid),
        fds_before,
        "the group must not leak descriptors"
    );
    assert!(
        zombie_children(pid).is_empty(),
        "unreaped children: {:?}",
        zombie_children(pid)
    );

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");
    session.finish();
}

// ----------------------------- table 12: the parent is never mutated

#[test]
fn tasks_never_mutate_the_session() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let before = exec(&mut session, "setup", "X=1; cd /tmp; SESSION_ONLY=keep");
    assert_eq!(int(get(&before, "status")), 0);

    let tasks = vec![
        task("mutate", "X=2; cd /; export Y=3; echo $X"),
        task("leave", "exit 9"),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    assert_eq!(text(get(task_by_id(&response, "mutate"), "stdout")), "2\n");
    assert_eq!(text(get(task_by_id(&response, "mutate"), "cwd")), "/");
    let leave = task_by_id(&response, "leave");
    assert_eq!(int(get(leave, "status")), 9);
    assert_eq!(text(get(leave, "state")), "completed");

    // Nothing the tasks did reached the session: not X, not the directory, not
    // the export, and `exit` ended only the task.
    let after = exec(
        &mut session,
        "read",
        "printf '%s %s %s %s\\n' \"$X\" \"$PWD\" \"${Y-unset}\" \"$SESSION_ONLY\"",
    );
    assert_eq!(text(get(&after, "stdout")), "1 /tmp unset keep\n");
    assert_eq!(text(get(&after, "cwd")), "/tmp");
    session.finish();
}

// --------------------- table 13: no filesystem sandbox, no serialization

#[test]
fn tasks_share_the_filesystem_without_serializing() {
    let dir = scratch("shared");
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        format!(
            r#"{{"id":"a","command":{},"cwd":{}}}"#,
            json::quote("printf 'A\\n' >> f; sleep 0.2; printf 'a\\n' >> f"),
            json::quote(&dir.to_string_lossy())
        ),
        format!(
            r#"{{"id":"b","command":{},"cwd":{}}}"#,
            json::quote("printf 'B\\n' >> f; sleep 0.2; printf 'b\\n' >> f"),
            json::quote(&dir.to_string_lossy())
        ),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    assert_eq!(int(get(&response, "status")), 0);

    let file = std::fs::read_to_string(dir.join("f")).expect("shared file");
    let lines: Vec<&str> = file.lines().collect();
    assert_eq!(lines.len(), 4, "both tasks must have written: {file:?}");
    // Both first writes land before either second write: the tasks ran at the
    // same time, and no lock, sandbox or serialization exists.
    let mut first: Vec<&str> = lines[..2].to_vec();
    first.sort();
    assert_eq!(first, vec!["A", "B"], "the tasks were serialized: {file:?}");
    let mut second: Vec<&str> = lines[2..].to_vec();
    second.sort();
    assert_eq!(
        second,
        vec!["a", "b"],
        "the tasks were serialized: {file:?}"
    );
    session.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------ table 14: per-task cwd and env

#[test]
fn per_task_cwd_and_env_are_visible_inside_and_invisible_outside() {
    let dir = scratch("task-scope");
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    exec(&mut session, "setup", "SCRUB=present");

    let tasks = vec![
        format!(
            r#"{{"id":"scoped","command":{},"cwd":{},"env":{{"TASK_ONLY":"task-value"}},"env_delta":true}}"#,
            json::quote("printf '%s|%s\\n' \"$PWD\" \"$TASK_ONLY\"; env"),
            json::quote(&dir.to_string_lossy())
        ),
        format!(
            r#"{{"id":"unset","command":{},"env":{{"SCRUB":null}}}}"#,
            json::quote("printf '%s\\n' \"${SCRUB-unset}\"")
        ),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    let scoped = task_by_id(&response, "scoped");
    let stdout = text(get(scoped, "stdout"));
    let expected = format!("{}|task-value", dir.to_string_lossy());
    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some(expected.as_str()),
        "cwd and $VAR must be set in the task: {stdout:?}"
    );
    assert!(
        stdout.contains("TASK_ONLY=task-value"),
        "the variable must be exported to a child process: {stdout:?}"
    );
    assert_eq!(text(get(scoped, "cwd")), dir.to_string_lossy());
    assert_eq!(
        text(get(task_by_id(&response, "unset"), "stdout")),
        "unset\n"
    );

    // The request's env is the task's: the session keeps its own value and
    // never learns TASK_ONLY.
    let after = exec(
        &mut session,
        "read",
        "printf '%s %s\\n' \"$SCRUB\" \"${TASK_ONLY-unset}\"",
    );
    assert_eq!(text(get(&after, "stdout")), "present unset\n");
    assert_eq!(
        text(get(&after, "cwd")),
        text(get(&exec(&mut session, "cwd", "pwd"), "cwd"))
    );

    session.send(r#"{"schema_version":1,"id":"s1","op":"state","include":["env"]}"#);
    let state = session.json();
    let env = get(&state, "env");
    assert!(
        env.get("TASK_ONLY").is_none(),
        "a task variable leaked into the session: {env:?}"
    );
    session.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

// ----------------------------------------------- status and failure shapes

#[test]
fn the_aggregate_status_is_the_first_failure_in_request_order() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("ok", "true"),
        task("three", "exit 3"),
        task("one", "false"),
    ];
    let response = session.parallel(&parallel_line("p1", "", &tasks));
    assert_eq!(int(get(&response, "status")), 3, "first failure wins");
    assert_eq!(text(get(&response, "state")), "completed");

    let all_ok = session.parallel(&parallel_line("p2", "", &[task("a", "true")]));
    assert_eq!(int(get(&all_ok, "status")), 0);
    session.finish();
}

#[test]
fn a_task_that_cannot_start_does_not_fail_the_group() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        format!(
            r#"{{"id":"nowhere","command":"true","cwd":{}}}"#,
            json::quote("/nonexistent/sharky-parallel-task")
        ),
        task("ok", "echo still-here"),
    ];
    let response = session.parallel(&parallel_line("p1", "", &tasks));
    let broken = task_by_id(&response, "nowhere");
    assert_eq!(text(get(broken, "state")), "spawn_error");
    assert_eq!(text(get(broken, "termination")), "spawn_error");
    assert_eq!(int(get(broken, "status")), 126);
    assert_eq!(text(get(get(broken, "error"), "code")), "SPAWN_FAILED");
    assert_eq!(
        text(get(task_by_id(&response, "ok"), "stdout")),
        "still-here\n"
    );
    // The task was forked and then failed its `chdir`, so it counts as started
    // (the failure happened in the child, not in the parent's spawn).
    assert_eq!(int(get(&response, "started_tasks")), 2);
    assert_eq!(int(get(&response, "status")), 126);
    assert_eq!(text(get(&response, "state")), "completed");
    session.finish();
}

#[test]
fn a_task_with_an_unusable_tmpdir_still_completes_with_captures() {
    // Captures are created in the parent before the fork. A bogus `TMPDIR`
    // now falls back to /tmp, /var/tmp and the cwd, so the task runs and its
    // output is captured; only a failure of every candidate is a spawn error.
    let dir = scratch("parent-spawn");
    let tmpdir = dir.to_str().expect("scratch path is UTF-8").to_string();
    let mut session = Session::spawn_with_env(&[], &[("TMPDIR", &tmpdir)]);
    ready(&mut session);
    std::fs::remove_dir_all(&dir).expect("drop the temp directory");

    let response = session.parallel(&parallel_line(
        "p1",
        "",
        &[task("captured-anyway", "printf TASK_OK")],
    ));
    let finished = task_by_id(&response, "captured-anyway");
    assert_eq!(text(get(finished, "state")), "completed", "{finished:?}");
    assert_eq!(text(get(finished, "termination")), "exited");
    assert_eq!(int(get(finished, "status")), 0);
    assert_eq!(text(get(finished, "stdout")), "TASK_OK");
    assert_eq!(
        int(get(&response, "started_tasks")),
        1,
        "the fallback let the parent fork the task"
    );
    assert_eq!(int(get(&response, "status")), 0);
    assert_eq!(text(get(&response, "state")), "completed");

    // The session keeps answering after the unusable TMPDIR.
    let after = exec(&mut session, "after", "echo alive");
    assert_eq!(int(get(&after, "status")), 0, "{after:?}");
    assert_eq!(text(get(&after, "stdout")), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "stderr: {errors}");
}

/// A full process-group registry refuses the task before the fork: spawning a
/// group the session cannot record would let it outlive the shutdown cleanup.
/// The task is answered `spawn_error`/`126` with a visible message and nothing
/// runs, exactly like the parent-side capture failure. The debug-only hook
/// simulates a registry full of live groups, which no test can fill honestly.
#[test]
fn a_task_refused_by_a_full_group_registry_never_spawns() {
    let dir = scratch("registry-full");
    let log = dir.join("ran");
    let mut session =
        Session::spawn_with_env(&[], &[("SHARKY_PARALLEL_TEST_REGISTRY_FULL", "1")]);
    ready(&mut session);
    let command = format!("printf ran > {}", log.display());

    let response = session.parallel(&parallel_line("p1", "", &[task("untrackable", &command)]));
    let refused = task_by_id(&response, "untrackable");
    assert_eq!(text(get(refused, "state")), "spawn_error", "{refused:?}");
    assert_eq!(text(get(refused, "termination")), "spawn_error");
    assert_eq!(int(get(refused, "status")), 126);
    assert_eq!(text(get(get(refused, "error"), "code")), "SPAWN_FAILED");
    assert_eq!(
        text(get(get(refused, "error"), "message")),
        "process-group registry is full"
    );
    assert_eq!(
        int(get(&response, "started_tasks")),
        0,
        "the refusal must happen before the fork"
    );
    assert!(
        !log.exists(),
        "a task the registry cannot track must not run: {log:?}"
    );
    assert_eq!(int(get(&response, "status")), 126);
    assert_eq!(text(get(&response, "state")), "completed");

    // The refusal is a task failure, not a session one: the session keeps
    // serving ordinary requests afterwards.
    let after = exec(&mut session, "after", "echo alive");
    assert_eq!(int(get(&after, "status")), 0, "{after:?}");
    assert_eq!(text(get(&after, "stdout")), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(
        errors.is_empty(),
        "the refusal must not reach the session: {errors}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_command_is_a_valid_no_op() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let response = session.parallel(&parallel_line("p1", "", &[task("noop", "   ")]));
    let noop = task_by_id(&response, "noop");
    assert_eq!(int(get(noop, "status")), 0);
    assert_eq!(text(get(noop, "stdout")), "");
    assert_eq!(int(get(&response, "status")), 0);
    session.finish();
}

/// A task that defines an EXIT action of its own runs it, exactly once, even
/// when the action text is identical to the one it inherited: bash runs the
/// action defined in the subshell, not the session's copy. The session's own
/// action still runs once, at close.
#[test]
fn a_task_runs_an_exit_trap_it_redefines_even_when_it_matches_the_session_action() {
    let dir = scratch("task-exit-trap");
    let log = dir.join("log");
    let action = format!("printf s >> \"{}\"", log.display());
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    exec(&mut session, "set-trap", &format!("trap '{action}' EXIT"));

    let tasks = vec![task("same", &format!("trap '{action}' EXIT; true"))];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":1,"#, &tasks));
    assert_eq!(int(get(task_by_id(&response, "same"), "status")), 0);
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "s",
        "the action the task defined must run once, at task end"
    );

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "ss",
        "the session's action runs once, at close, and not once per task"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------------------ deadlines

#[test]
fn a_task_deadline_ends_only_that_task() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let started = Instant::now();
    let tasks = vec![
        String::from(r#"{"id":"slow","command":"sleep 5","timeout_ms":100}"#),
        task("fast", "sleep 0.3; echo ok"),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a deadline must end the task, not the wait"
    );
    let slow = task_by_id(&response, "slow");
    assert_eq!(text(get(slow, "state")), "timeout");
    assert_eq!(text(get(slow, "termination")), "timeout");
    assert_eq!(int(get(slow, "status")), 124);
    let fast = task_by_id(&response, "fast");
    assert_eq!(int(get(fast, "status")), 0);
    assert_eq!(text(get(fast, "stdout")), "ok\n");
    assert_eq!(int(get(&response, "status")), 124);
    session.finish();
}

#[test]
fn a_group_deadline_drops_what_never_started() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let started = Instant::now();
    let tasks = vec![
        task("slow", "sleep 5"),
        task("queued", "true"),
        task("queued2", "true"),
    ];
    let response = session.parallel(&parallel_line(
        "p1",
        r#""max_concurrency":1,"timeout_ms":150,"#,
        &tasks,
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
    let slow = task_by_id(&response, "slow");
    assert_eq!(text(get(slow, "state")), "timeout");
    assert_eq!(int(get(slow, "status")), 124);
    for id in ["queued", "queued2"] {
        let task = task_by_id(&response, id);
        assert_eq!(text(get(task, "state")), "cancelled", "{task:?}");
        assert_eq!(int(get(task, "status")), 130);
        assert_eq!(text(get(task, "reason")), "group_timeout");
    }
    assert_eq!(int(get(&response, "started_tasks")), 1);
    session.finish();
}

// ------------------------- table 6/7/8: cancellation by the supervisor

#[test]
fn the_feature_is_advertised_now_that_cancellation_is_honored() {
    let mut session = Session::spawn(&[]);
    let response = session.json();
    assert_eq!(text(get(&response, "type")), "ready");
    let features = array(get(get(&response, "protocol"), "features"));
    assert!(
        features
            .iter()
            .any(|feature| text(feature) == "parallel_task_cancel"),
        "the supervisor honors per-task cancel, so it must be advertised: {features:?}"
    );
    session.finish();
}

#[test]
fn cancelling_the_group_cancels_every_running_task_and_keeps_the_session_alive() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![task("a", "sleep 30"), task("b", "sleep 30")];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(150));

    let started = Instant::now();
    session.send(&cancel_line("c1", "p1", None, Some(200)));
    let ack = ack_frame(&mut session, "cancel");
    assert_eq!(text(get(&ack, "id")), "c1");
    assert_eq!(text(get(&ack, "outcome")), "delivered");

    let response = session.json();
    assert_eq!(text(get(&response, "type")), "parallel");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a group cancel must not wait for the task deadlines: {:?}",
        started.elapsed()
    );
    assert_eq!(text(get(&response, "state")), "cancelled");
    assert_eq!(int(get(&response, "status")), 143);
    assert_eq!(int(get(&response, "started_tasks")), 2);
    for id in ["a", "b"] {
        let entry = task_by_id(&response, id);
        assert_eq!(text(get(entry, "state")), "cancelled", "{entry:?}");
        assert_eq!(text(get(entry, "termination")), "cancelled");
        assert_eq!(int(get(entry, "status")), 143);
        assert_eq!(text(get(entry, "reason")), "group_cancel");
        let pgid = int(get(entry, "pgid"));
        assert!(
            gone_within(pgid, 3000),
            "task {id} group {pgid} outlived the cancel"
        );
    }

    // Cancellation is request-scoped: the session keeps serving.
    let after = exec(&mut session, "after", "echo alive");
    assert_eq!(text(get(&after, "stdout")), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn cancelling_one_task_leaves_its_sibling_alone() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("keep", "sleep 0.8; echo one"),
        task("stop", "sleep 30"),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(150));

    session.send(&cancel_line("c1", "p1", Some("stop"), Some(200)));
    let ack = ack_frame(&mut session, "cancel");
    assert_eq!(text(get(&ack, "outcome")), "delivered");

    let response = session.json();
    let stopped = task_by_id(&response, "stop");
    assert_eq!(text(get(stopped, "state")), "cancelled");
    assert_eq!(text(get(stopped, "termination")), "cancelled");
    assert_eq!(int(get(stopped, "status")), 143);
    assert_eq!(text(get(stopped, "reason")), "task_cancel");
    assert!(gone_within(int(get(stopped, "pgid")), 3000));

    let kept = task_by_id(&response, "keep");
    assert_eq!(int(get(kept, "status")), 0, "{kept:?}");
    assert_eq!(text(get(kept, "stdout")), "one\n");
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(
        int(get(&response, "status")),
        143,
        "the cancelled task still publishes its status"
    );
    session.finish();
}

#[test]
fn a_task_deadline_escalates_to_kill_inside_the_grace() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![String::from(
        r#"{"id":"stubborn","command":"trap '' TERM; sleep 10","timeout_ms":100}"#,
    )];
    let started = Instant::now();
    let response = session.parallel(&parallel_line("p1", "", &tasks));
    let elapsed = started.elapsed();
    let stubborn = task_by_id(&response, "stubborn");
    assert_eq!(text(get(stubborn, "state")), "timeout", "{stubborn:?}");
    assert_eq!(text(get(stubborn, "termination")), "timeout");
    assert_eq!(int(get(stubborn, "status")), 124);
    assert!(
        elapsed >= Duration::from_millis(1800),
        "SIGTERM was ignored, so the escalation must have taken the grace: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the escalation was not bounded by the grace: {elapsed:?}"
    );
    session.finish();
}

#[test]
fn the_group_deadline_and_a_task_deadline_are_enforced_together() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        String::from(r#"{"id":"own","command":"sleep 5","timeout_ms":100}"#),
        task("group", "sleep 5"),
        task("queued", "true"),
    ];
    let started = Instant::now();
    let response = session.parallel(&parallel_line(
        "p1",
        r#""max_concurrency":1,"timeout_ms":300,"#,
        &tasks,
    ));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadlines were not enforced: {:?}",
        started.elapsed()
    );
    for id in ["own", "group"] {
        let entry = task_by_id(&response, id);
        assert_eq!(text(get(entry, "state")), "timeout", "{entry:?}");
        assert_eq!(text(get(entry, "termination")), "timeout");
        assert_eq!(int(get(entry, "status")), 124);
    }
    let queued = task_by_id(&response, "queued");
    assert_eq!(text(get(queued, "state")), "cancelled", "{queued:?}");
    assert_eq!(int(get(queued, "status")), 130);
    assert_eq!(text(get(queued, "reason")), "group_timeout");
    assert_eq!(int(get(&response, "started_tasks")), 2);
    session.finish();
}

#[test]
fn cancelling_a_group_twice_is_idempotent() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![String::from(
        r#"{"id":"stubborn","command":"trap '' TERM; sleep 10"}"#,
    )];
    session.send(&parallel_line("p1", "", &tasks));
    std::thread::sleep(Duration::from_millis(150));

    let started = Instant::now();
    session.send(&cancel_line("c1", "p1", None, Some(300)));
    ack_frame(&mut session, "cancel");
    session.send(&cancel_line("c2", "p1", None, Some(300)));
    let second = ack_frame(&mut session, "cancel");
    assert_eq!(
        text(get(&second, "outcome")),
        "delivered",
        "a second cancel for a live group must not fail"
    );

    let response = session.json();
    let stubborn = task_by_id(&response, "stubborn");
    assert_eq!(text(get(stubborn, "state")), "cancelled");
    assert_eq!(int(get(stubborn, "status")), 143);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the escalation was not bounded: {:?}",
        started.elapsed()
    );
    session.finish();
}

#[test]
fn cancel_after_completion_is_too_late_and_unknown_ids_are_typed() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let done = session.parallel(&parallel_line("p1", "", &[task("t1", "true")]));
    assert_eq!(int(get(&done, "status")), 0);

    session.send(&cancel_line("c1", "p1", None, None));
    let late = session.json();
    assert_eq!(
        text(get(get(&late, "error"), "code")),
        "CANCEL_TOO_LATE",
        "{late:?}"
    );

    session.send(&cancel_line("c2", "p1", Some("t1"), None));
    let late_task = session.json();
    assert_eq!(
        text(get(get(&late_task, "error"), "code")),
        "CANCEL_TOO_LATE",
        "{late_task:?}"
    );

    // `task` is only meaningful on a parallel request, never on `exec`.
    exec(&mut session, "r1", "true");
    session.send(&cancel_line("c4", "r1", Some("t1"), None));
    let invalid = session.json();
    assert_eq!(
        text(get(get(&invalid, "error"), "code")),
        "INVALID_REQUEST",
        "{invalid:?}"
    );

    // A live group answers an unknown task id with the other code in the pair.
    let tasks = vec![task("real", "sleep 30")];
    session.send(&parallel_line("p2", "", &tasks));
    std::thread::sleep(Duration::from_millis(150));
    session.send(&cancel_line("c3", "p2", Some("ghost"), None));
    let unknown = session.json();
    assert_eq!(
        text(get(get(&unknown, "error"), "code")),
        "CANCEL_UNKNOWN_ID",
        "{unknown:?}"
    );

    // The group is still alive and answers the whole-group cancel.
    session.send(&cancel_line("c5", "p2", None, Some(200)));
    ack_frame(&mut session, "cancel");
    let response = session.json();
    assert_eq!(text(get(&response, "state")), "cancelled");
    session.finish();
}

#[test]
fn close_during_a_group_stops_it_and_the_session_exits_zero() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    // One plain task and one with a `&` grandchild (U6): both must die, and the
    // grandchild must be in the task's group without `sh -c`.
    let tasks = vec![
        task("plain", "sleep 30"),
        task("background", "sleep 30 & echo $!; wait"),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(200));

    session.send(r#"{"schema_version":1,"id":"cl1","op":"close","wait_ms":500}"#);
    // The accepted request is answered first; the close then acks and closes.
    let response = session.json_within(4000).expect("the group response");
    assert_eq!(
        text(get(&response, "type")),
        "parallel",
        "the in-flight group is answered before the close: {response:?}"
    );
    assert_eq!(text(get(&response, "state")), "cancelled");
    let bg = task_by_id(&response, "background");
    assert_eq!(text(get(bg, "state")), "cancelled");
    assert_eq!(int(get(bg, "status")), 143);
    let plain = task_by_id(&response, "plain");
    assert_eq!(int(get(plain, "status")), 143);

    let ack = ack_frame(&mut session, "close");
    assert_eq!(text(get(&ack, "id")), "cl1");
    let closed = session.json();
    assert_eq!(text(get(&closed, "type")), "closed");
    assert_eq!(int(get(&closed, "status")), 0);

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");

    for id in ["plain", "background"] {
        let pgid = int(get(task_by_id(&response, id), "pgid"));
        assert!(gone_within(pgid, 3000), "task {id} group {pgid} survived close");
    }
    let grandchild: i64 = text(get(bg, "stdout"))
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
        .expect("the background pid was printed");
    assert!(
        gone_within(grandchild, 3000),
        "the `&` grandchild {grandchild} outlived the task group"
    );
}

/// Only a `close` that decodes and carries an unused id may stop the group. A
/// malformed line or a repeated id is the driver's to reject, in request order,
/// so the queued frame must not have touched a running task.
#[test]
fn an_invalid_close_does_not_cancel_a_running_group() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    // An id the driver has already accepted makes a well-shaped close a
    // duplicate: the supervisor must treat it exactly like a decode error.
    exec(&mut session, "used", "true");

    let tasks = vec![
        task("one", "sleep 1; echo one"),
        task("two", "sleep 1; echo two"),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(100));

    // `wait_ms` has the wrong type, so the request cannot be validated.
    session.send(r#"{"schema_version":1,"id":"c1","op":"close","wait_ms":"invalid"}"#);
    // Well shaped, but `used` has been taken on this connection.
    session.send(r#"{"schema_version":1,"id":"used","op":"close","wait_ms":0}"#);

    let response = session.json_within(6000).expect("the group response");
    assert_eq!(text(get(&response, "type")), "parallel", "{response:?}");
    assert_eq!(text(get(&response, "state")), "completed", "{response:?}");
    for (id, stdout) in [("one", "one\n"), ("two", "two\n")] {
        let entry = task_by_id(&response, id);
        assert_eq!(text(get(entry, "state")), "completed", "{entry:?}");
        assert_eq!(
            int(get(entry, "status")),
            0,
            "the invalid close cancelled task {id}: {entry:?}"
        );
        assert_eq!(text(get(entry, "stdout")), stdout, "{entry:?}");
    }

    // Both frames were queued behind the group and are answered in order.
    let invalid = session.json();
    assert_eq!(text(get(&invalid, "id")), "c1", "{invalid:?}");
    assert_eq!(
        text(get(get(&invalid, "error"), "code")),
        "INVALID_REQUEST",
        "{invalid:?}"
    );
    let duplicate = session.json();
    assert_eq!(text(get(&duplicate, "id")), "used", "{duplicate:?}");
    assert_eq!(
        text(get(get(&duplicate, "error"), "code")),
        "DUPLICATE_ID",
        "{duplicate:?}"
    );

    // The session is still healthy: a valid close acks and exits zero.
    session.send(r#"{"schema_version":1,"id":"cl2","op":"close","wait_ms":300}"#);
    let ack = ack_frame(&mut session, "close");
    assert_eq!(text(get(&ack, "id")), "cl2");
    let closed = session.json();
    assert_eq!(text(get(&closed, "type")), "closed");
    assert_eq!(int(get(&closed, "status")), 0);

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

/// A malformed `cancel` is answered with an error and changes nothing: the
/// group keeps running and every task publishes its real result.
#[test]
fn an_invalid_cancel_does_not_stop_anything() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("one", "sleep 1; echo one"),
        task("two", "sleep 1; echo two"),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(100));

    // `target` must be a string; the supervisor answers the error itself, so
    // this frame arrives before the group's own response.
    session.send(r#"{"schema_version":1,"id":"c1","op":"cancel","target":5}"#);
    let invalid = session.json_within(4000).expect("the cancel error");
    assert_eq!(text(get(&invalid, "id")), "c1", "{invalid:?}");
    assert_eq!(
        text(get(get(&invalid, "error"), "code")),
        "INVALID_REQUEST",
        "{invalid:?}"
    );

    let response = session.json_within(6000).expect("the group response");
    assert_eq!(text(get(&response, "type")), "parallel", "{response:?}");
    assert_eq!(text(get(&response, "state")), "completed", "{response:?}");
    for (id, stdout) in [("one", "one\n"), ("two", "two\n")] {
        let entry = task_by_id(&response, id);
        assert_eq!(text(get(entry, "state")), "completed", "{entry:?}");
        assert_eq!(
            int(get(entry, "status")),
            0,
            "the invalid cancel stopped task {id}: {entry:?}"
        );
        assert_eq!(text(get(entry, "stdout")), stdout, "{entry:?}");
    }
    session.finish();
}

#[test]
fn an_unwrapped_background_job_dies_with_its_task_cancel() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    // U6's guarantee, exercised without `sh -c`: the `&` job must join the
    // task's own group, so cancelling the task takes the grandchild with it.
    let tasks = vec![task("bg", "sleep 30 & echo $!; wait")];
    session.send(&parallel_line("p1", "", &tasks));
    std::thread::sleep(Duration::from_millis(200));

    session.send(&cancel_line("c1", "p1", Some("bg"), Some(200)));
    ack_frame(&mut session, "cancel");
    let response = session.json();
    let bg = task_by_id(&response, "bg");
    assert_eq!(text(get(bg, "state")), "cancelled");
    assert_eq!(int(get(bg, "status")), 143);

    let grandchild: i64 = text(get(bg, "stdout"))
        .lines()
        .next()
        .and_then(|line| line.trim().parse().ok())
        .expect("the background pid was printed");
    assert!(grandchild > 0);
    assert!(
        gone_within(grandchild, 3000),
        "the `&` grandchild {grandchild} outlived its task cancel"
    );
    assert!(
        gone_within(int(get(bg, "pgid")), 3000),
        "the task group outlived its cancel"
    );
    session.finish();
}

#[test]
fn a_group_cancelled_while_queued_never_spawns() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // The group is accepted while an `exec` owns the session, so it is still a
    // queued frame when the cancel arrives. Parity with `exec` says it must be
    // answered without ever running a task.
    session.send(&format!(
        r#"{{"schema_version":1,"id":"r1","op":"exec","command":{}}}"#,
        json::quote("sleep 0.4")
    ));
    std::thread::sleep(Duration::from_millis(50));
    let tasks = vec![task("a", "sleep 30"), task("b", "sleep 30")];
    session.send(&parallel_line("p1", "", &tasks));
    std::thread::sleep(Duration::from_millis(50));
    session.send(&cancel_line("c1", "p1", None, None));
    let ack = ack_frame(&mut session, "cancel");
    assert_eq!(text(get(&ack, "outcome")), "delivered");

    let first = session.json();
    assert_eq!(text(get(&first, "type")), "result", "{first:?}");
    assert_eq!(text(get(&first, "id")), "r1");

    let response = session.json();
    assert_eq!(text(get(&response, "type")), "parallel", "{response:?}");
    assert_eq!(text(get(&response, "state")), "cancelled");
    assert_eq!(int(get(&response, "started_tasks")), 0);
    assert_eq!(int(get(&response, "status")), 130);
    for id in ["a", "b"] {
        let entry = task_by_id(&response, id);
        assert_eq!(text(get(entry, "state")), "cancelled");
        assert_eq!(int(get(entry, "status")), 130);
        assert_eq!(text(get(entry, "reason")), "group_cancel");
    }

    let after = exec(&mut session, "after", "echo alive");
    assert_eq!(text(get(&after, "stdout")), "alive\n");
    session.finish();
}

// ----------------------------- table 5: fail-fast stops the group

/// A failure in the first slot stops the window: the sibling is cancelled and
/// the tasks that never got a slot are answered without ever starting.
#[test]
fn fail_fast_cancels_the_running_sibling_and_drops_the_queue() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("fails", "false"),
        task("slow", "sleep 5"),
        task("queued1", "printf 'one\\n'"),
        task("queued2", "printf 'two\\n'"),
    ];
    let start = Instant::now();
    let response = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(3),
        "fail-fast waited for the long task: {elapsed:?}"
    );
    assert_eq!(text(get(&response, "policy")), "fail-fast");
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "status")), 1);
    assert_eq!(int(get(&response, "started_tasks")), 2);
    assert_eq!(array(get(&response, "tasks")).len(), 4);

    let failed = task_by_id(&response, "fails");
    assert_eq!(text(get(failed, "state")), "completed");
    assert_eq!(text(get(failed, "termination")), "exited");
    assert_eq!(int(get(failed, "status")), 1);
    assert!(
        failed.get("reason").is_none(),
        "the trigger keeps its real result: {failed:?}"
    );

    let running = task_by_id(&response, "slow");
    assert_eq!(text(get(running, "state")), "cancelled", "{running:?}");
    assert_eq!(text(get(running, "termination")), "cancelled");
    assert_eq!(int(get(running, "status")), 143);
    assert_eq!(text(get(running, "reason")), "fail_fast");

    for id in ["queued1", "queued2"] {
        let queued = task_by_id(&response, id);
        assert_eq!(text(get(queued, "state")), "cancelled", "{queued:?}");
        assert_eq!(text(get(queued, "termination")), "cancelled");
        assert_eq!(int(get(queued, "status")), 130);
        assert_eq!(text(get(queued, "reason")), "fail_fast");
        assert_eq!(text(get(queued, "stdout")), "");
        assert_eq!(int(get(queued, "duration_ms")), 0);
    }

    let alive = exec(&mut session, "alive", "echo alive");
    assert_eq!(text(get(&alive, "stdout")), "alive\n");
    session.finish();
}

/// A task deadline is a failure signal too (design §7): it keeps its `timeout`
/// result and stops the rest of the group.
#[test]
fn fail_fast_triggers_on_a_task_timeout() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        String::from(r#"{"id":"slow","command":"sleep 5","timeout_ms":200}"#),
        task("sibling", "sleep 5"),
        task("queued", "true"),
    ];
    let response = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let timeout = task_by_id(&response, "slow");
    assert_eq!(text(get(timeout, "state")), "timeout", "{timeout:?}");
    assert_eq!(text(get(timeout, "termination")), "timeout");
    assert_eq!(int(get(timeout, "status")), 124);
    assert!(timeout.get("reason").is_none(), "{timeout:?}");

    let cancelled = task_by_id(&response, "sibling");
    assert_eq!(text(get(cancelled, "state")), "cancelled", "{cancelled:?}");
    assert_eq!(int(get(cancelled, "status")), 143);
    assert_eq!(text(get(cancelled, "reason")), "fail_fast");

    let queued = task_by_id(&response, "queued");
    assert_eq!(text(get(queued, "state")), "cancelled", "{queued:?}");
    assert_eq!(int(get(queued, "status")), 130);
    assert_eq!(text(get(queued, "reason")), "fail_fast");

    assert_eq!(int(get(&response, "status")), 124);
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "started_tasks")), 2);
    session.finish();
}

/// A child-side spawn error is a genuine failure, unlike a cancel: it keeps its
/// `SPAWN_FAILED` error object and still stops the group.
#[test]
fn fail_fast_triggers_on_a_spawn_error() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        format!(
            r#"{{"id":"nowhere","command":"true","cwd":{}}}"#,
            json::quote("/nonexistent/sharky-parallel-task")
        ),
        task("sibling", "sleep 5"),
    ];
    let response = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let broken = task_by_id(&response, "nowhere");
    assert_eq!(text(get(broken, "state")), "spawn_error", "{broken:?}");
    assert_eq!(text(get(broken, "termination")), "spawn_error");
    assert_eq!(int(get(broken, "status")), 126);
    assert_eq!(text(get(get(broken, "error"), "code")), "SPAWN_FAILED");
    assert!(broken.get("reason").is_none(), "{broken:?}");

    let cancelled = task_by_id(&response, "sibling");
    assert_eq!(text(get(cancelled, "state")), "cancelled", "{cancelled:?}");
    assert_eq!(int(get(cancelled, "status")), 143);
    assert_eq!(text(get(cancelled, "reason")), "fail_fast");

    assert_eq!(int(get(&response, "status")), 126);
    assert_eq!(text(get(&response, "state")), "completed");
    session.finish();
}

/// The pair below pins §2.4 for `fail-fast`: the aggregate is the status of the
/// first failed task in REQUEST order, never the task whose completion arrived
/// first. Here the last slot fails first and cancels the earlier slot, so the
/// earlier (cancelled, 143) status wins the aggregate.
#[test]
fn fail_fast_aggregate_follows_request_order_when_the_last_task_fails_first() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("early", "sleep 0.4; exit 3"),
        task("late", "sleep 0.05; exit 7"),
    ];
    let response = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let early = task_by_id(&response, "early");
    let late = task_by_id(&response, "late");

    assert_eq!(text(get(late, "state")), "completed", "{late:?}");
    assert_eq!(text(get(late, "termination")), "exited");
    assert_eq!(
        int(get(late, "status")),
        7,
        "the trigger keeps its real result"
    );
    assert!(late.get("reason").is_none(), "{late:?}");

    assert_eq!(text(get(early, "state")), "cancelled", "{early:?}");
    assert_eq!(text(get(early, "termination")), "cancelled");
    assert_eq!(int(get(early, "status")), 143);
    assert_eq!(text(get(early, "reason")), "fail_fast");

    assert_eq!(
        int(get(&response, "status")),
        143,
        "request order, not completion order, decides the aggregate"
    );
    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "started_tasks")), 2);
    session.finish();
}

/// Mirror of the previous test: the same task fails first in wall-clock order,
/// but now it is first in request order, so its real status wins the aggregate.
#[test]
fn fail_fast_aggregate_follows_request_order_when_the_first_task_fails_first() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("early", "sleep 0.05; exit 7"),
        task("late", "sleep 0.4; exit 3"),
    ];
    let response = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let early = task_by_id(&response, "early");
    let late = task_by_id(&response, "late");

    assert_eq!(text(get(early, "state")), "completed", "{early:?}");
    assert_eq!(int(get(early, "status")), 7);
    assert!(early.get("reason").is_none(), "{early:?}");

    assert_eq!(text(get(late, "state")), "cancelled", "{late:?}");
    assert_eq!(int(get(late, "status")), 143);
    assert_eq!(text(get(late, "reason")), "fail_fast");

    assert_eq!(int(get(&response, "status")), 7);
    assert_eq!(text(get(&response, "state")), "completed");
    session.finish();
}

/// A cancel is explicitly not a failure signal (design §7): `fail-fast` must
/// leave the other task alone when the only non-zero status came from a cancel.
#[test]
fn fail_fast_does_not_trigger_on_a_task_cancel() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("stop", "sleep 5"),
        task("work", "sleep 0.3; echo done"),
    ];
    session.send(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    // Let both slots open before cancelling, so the task is cancelled while
    // running (143) rather than while queued (130).
    std::thread::sleep(Duration::from_millis(150));
    session.send(&cancel_line("c1", "p1", Some("stop"), Some(200)));
    assert_eq!(
        text(get(&ack_frame(&mut session, "cancel"), "outcome")),
        "delivered"
    );

    let response = session.json();
    let stopped = task_by_id(&response, "stop");
    assert_eq!(text(get(stopped, "state")), "cancelled", "{stopped:?}");
    assert_eq!(int(get(stopped, "status")), 143);
    assert_eq!(text(get(stopped, "reason")), "task_cancel");

    let work = task_by_id(&response, "work");
    assert_eq!(text(get(work, "state")), "completed", "{work:?}");
    assert_eq!(int(get(work, "status")), 0);
    assert_eq!(text(get(work, "stdout")), "done\n");
    assert!(work.get("reason").is_none(), "{work:?}");

    assert_eq!(text(get(&response, "state")), "completed");
    assert_eq!(int(get(&response, "started_tasks")), 2);
    session.finish();
}

/// With no failure there is nothing to stop, so `fail-fast` and `wait-all`
/// produce the same results (apart from the echoed policy and request id).
#[test]
fn fail_fast_with_no_failures_matches_wait_all() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let tasks = vec![
        task("a", "printf 'a\\n'"),
        task("b", "printf 'b\\n'"),
        task("c", "printf 'c\\n'"),
        task("d", "printf 'd\\n'"),
    ];
    let fast = session.parallel(&parallel_line(
        "p1",
        r#""policy":"fail-fast","max_concurrency":2,"#,
        &tasks,
    ));
    let all = session.parallel(&parallel_line("p2", r#""max_concurrency":2,"#, &tasks));

    for (id, response) in [("p1", &fast), ("p2", &all)] {
        assert_eq!(text(get(response, "id")), id);
        assert_eq!(text(get(response, "state")), "completed");
        assert_eq!(int(get(response, "status")), 0);
        assert_eq!(int(get(response, "started_tasks")), 4);
        let entries = array(get(response, "tasks"));
        assert_eq!(entries.len(), 4);
        for (index, entry) in entries.iter().enumerate() {
            let expected = format!("{}\n", ["a", "b", "c", "d"][index]);
            assert_eq!(text(get(entry, "stdout")), expected, "{entry:?}");
            assert_eq!(int(get(entry, "status")), 0, "{entry:?}");
            assert!(entry.get("reason").is_none(), "{entry:?}");
        }
    }
    assert_eq!(text(get(&fast, "policy")), "fail-fast");
    assert_eq!(text(get(&all, "policy")), "wait-all");
    session.finish();
}

// ------------------------- empty task commands and task durations (#4, #5)

/// A whitespace-only task command is a no-op with status 0 whatever `$?` the
/// session is carrying: `session_eval` of an empty program reports the inherited
/// status, so evaluating it would make a task that ran nothing report the
/// session's last failure. `exec` already answers 0; a task must too.
#[test]
fn a_whitespace_task_command_does_not_inherit_the_session_status() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let failed = exec(&mut session, "fail", "false");
    assert_eq!(int(get(&failed, "status")), 1, "{failed:?}");

    let response = session.parallel(&parallel_line("p1", "", &[task("noop", "   ")]));
    let noop = task_by_id(&response, "noop");
    assert_eq!(
        int(get(noop, "status")),
        0,
        "an empty command must be a no-op, not the session's $?: {noop:?}"
    );
    assert_eq!(text(get(noop, "state")), "completed");
    assert_eq!(text(get(noop, "termination")), "exited");
    assert_eq!(text(get(noop, "stdout")), "");
    assert_eq!(int(get(&response, "status")), 0);

    // Control: a real command still reports its own status after the failure.
    let real = session.parallel(&parallel_line(
        "p2",
        "",
        &[task("real", "exit 7"), task("ok", "true")],
    ));
    assert_eq!(int(get(task_by_id(&real, "real"), "status")), 7);
    assert_eq!(int(get(task_by_id(&real, "ok"), "status")), 0);
    session.finish();
}

/// The public per-task duration is the parent's spawn-to-reap measurement, so it
/// covers the task's EXIT action. The child used to time only `session_eval`,
/// which ends before `finish_task` runs the action, reporting ~0 ms for a task
/// that really slept inside its trap.
#[test]
fn a_task_duration_includes_its_exit_action() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let response = session.parallel(&parallel_line(
        "p1",
        "",
        &[task("trapped", "trap 'sleep 0.4' EXIT; true")],
    ));
    let trapped = task_by_id(&response, "trapped");
    assert_eq!(int(get(trapped, "status")), 0, "{trapped:?}");
    let duration = int(get(trapped, "duration_ms"));
    assert!(
        duration >= 350,
        "the EXIT action is part of the task and must be measured: {duration} ms"
    );
    let group_duration = int(get(&response, "duration_ms"));
    assert!(
        group_duration >= duration,
        "a group cannot be shorter than its task: {group_duration} ms vs {duration} ms"
    );
    session.finish();
}

// ------------------------------------------- process group confirmation (#3)

/// A task shell must be the leader of its own process group: that pgid is what
/// a cancel or a deadline signals. `/proc/self` at redirection time is the task
/// shell itself, so this reads the same fact the child confirms before it runs
/// the command, and the recorded `pgid` must be that pid.
#[test]
fn a_task_leads_its_own_process_group() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let command = r#"read -r pid comm state ppid pgrp _ < /proc/self/stat
printf '%s %s\n' "$pid" "$pgrp""#;
    let response = session.parallel(&parallel_line("p1", "", &[task("leader", command)]));
    let leader = task_by_id(&response, "leader");
    let stdout = text(get(leader, "stdout"));
    let mut fields = stdout.split_whitespace();
    let pid: i64 = fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_else(|| panic!("no task pid in {stdout:?}"));
    let pgrp: i64 = fields
        .next()
        .and_then(|field| field.parse().ok())
        .unwrap_or_else(|| panic!("no task pgrp in {stdout:?}"));
    assert_eq!(
        pid, pgrp,
        "the task shell is not leading its own group: {stdout:?}"
    );
    assert_eq!(
        Some(pid),
        get(leader, "pgid").as_i64(),
        "the recorded pgid must be the task's own group: {leader:?}"
    );
    session.finish();
}

/// A child that cannot become its own group leader must not run the task: a
/// cancel would signal a group it is not in and leave the tree alive. The
/// failure is reported as a spawn error, not as a success. The debug-only hook
/// simulates the `setpgid` failure no test can provoke.
#[test]
fn a_task_without_its_own_process_group_never_runs() {
    let dir = scratch("group-failure");
    let log = dir.join("ran");
    let mut session = Session::spawn_with_env(&[], &[("SHARKY_PARALLEL_TEST_GROUP_FAILURE", "1")]);
    ready(&mut session);
    let command = format!("printf ran > {}", log.display());
    let response = session.parallel(&parallel_line("p1", "", &[task("ungrouped", &command)]));
    let broken = task_by_id(&response, "ungrouped");
    assert_eq!(text(get(broken, "state")), "spawn_error", "{broken:?}");
    assert_eq!(text(get(broken, "termination")), "spawn_error");
    assert_eq!(int(get(broken, "status")), 126);
    assert_eq!(text(get(get(broken, "error"), "code")), "SPAWN_FAILED");
    assert_eq!(
        text(get(get(broken, "error"), "message")),
        "could not create process group"
    );
    assert!(
        text(get(broken, "stderr")).contains("could not create process group"),
        "the refusal belongs to the task's own stderr: {broken:?}"
    );
    assert!(
        !log.exists(),
        "a task without its own group must not run: {log:?}"
    );
    assert_eq!(int(get(&response, "status")), 126);
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(
        errors.is_empty(),
        "the refusal must not reach the session: {errors}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------- task env over a readonly session name (#7)

/// `tasks[].env` is an external context, like `env NAME=value command`: it sets
/// and unsets uniformly and does not inherit the shell's readonly attribute.
/// The session keeps its own name and attribute, and the task's delta reports
/// what the task actually saw.
#[test]
fn a_task_env_overrides_a_readonly_session_variable() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    let ro = exec(&mut session, "ro", "readonly R=old");
    assert_eq!(int(get(&ro, "status")), 0, "{ro:?}");

    let tasks = vec![
        format!(
            r#"{{"id":"set","command":{},"env":{{"R":"new"}},"env_delta":true}}"#,
            json::quote("printf '%s\\n' \"$R\"; R=changed")
        ),
        format!(
            r#"{{"id":"unset","command":{},"env":{{"R":null}},"env_delta":true}}"#,
            json::quote("printf '%s\\n' \"${R-unset}\"; R=added")
        ),
    ];
    let response = session.parallel(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    let set = task_by_id(&response, "set");
    let set_stdout = text(get(set, "stdout"));
    assert!(
        set_stdout.starts_with("new\n"),
        "a task env value must apply to a readonly name: {set:?}"
    );
    // The task copy does not inherit the attribute, so its own assignment works
    // and the delta reports the value the task really ended with.
    assert_eq!(
        text(get(get(set, "env_delta"), "R")),
        "changed",
        "the task copy must not stay readonly: {set:?}"
    );
    let unset = task_by_id(&response, "unset");
    let unset_stdout = text(get(unset, "stdout"));
    assert!(
        unset_stdout.starts_with("unset\n"),
        "a task env null must remove a readonly name too: {unset:?}"
    );
    assert_eq!(text(get(get(unset, "env_delta"), "R")), "added");

    // The session's own name and attribute did not move.
    let after = exec(&mut session, "check", "printf '%s\\n' \"$R\"; R=changed");
    assert_eq!(text(get(&after, "stdout")), "old\n", "{after:?}");
    assert_ne!(int(get(&after, "status")), 0, "{after:?}");
    assert!(
        text(get(&after, "stderr")).contains("readonly variable"),
        "the session's name must still be readonly: {after:?}"
    );
    session.finish();
}

// ------------------------------------------- env_delta reporting bound (#11)

/// The status record has its own bound, independent of the 1 MiB protocol line
/// limit, so a delta larger than a line still arrives instead of being cut into
/// an unparseable record and silently omitted.
#[test]
fn an_env_delta_larger_than_a_protocol_line_still_arrives() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);
    session.send(&format!(
        r#"{{"schema_version":1,"id":"big","op":"exec","command":{},"env_delta":false}}"#,
        json::quote("BIG=$(printf '%*s' 1200000 '' | tr ' ' x)")
    ));
    let big = session.json();
    assert_eq!(
        int(get(&big, "status")),
        0,
        "{:?}",
        text(get(&big, "stderr"))
    );

    let request = vec![
        r#"{"id":"delta","command":"PAD=$(printf '%*s' 1200000 '' | tr ' ' x)","env_delta":true}"#
            .to_string(),
    ];
    let response = session.parallel(&parallel_line("p1", "", &request));
    let task = task_by_id(&response, "delta");
    assert_eq!(int(get(task, "status")), 0, "{task:?}");
    let delta = get(task, "env_delta");
    assert_eq!(
        text(get(delta, "PAD")).len(),
        1_200_000,
        "a delta inside the record bound must arrive whole"
    );
    assert!(
        task.get("env_delta_omitted").is_none(),
        "nothing was omitted: {task:?}"
    );
    session.finish();
}

/// Over the record bound the delta is dropped, but the response says so: a
/// requested `env_delta` never disappears without a marker while the task keeps
/// reporting its real status.
#[test]
fn an_env_delta_over_the_record_bound_is_reported_as_omitted() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let response = session.parallel(&parallel_line(
        "p1",
        "",
        &[
            r#"{"id":"delta","command":"PAD=$(printf '%*s' 4400000 '' | tr ' ' x); exit 5","env_delta":true}"#
                .to_string(),
        ],
    ));
    let task = task_by_id(&response, "delta");
    assert!(
        task.get("env_delta").is_none(),
        "the delta does not fit the record: {task:?}"
    );
    assert_eq!(
        text(get(task, "env_delta_omitted")),
        "record_too_large",
        "an omitted delta must be explicit: {task:?}"
    );
    // The rest of the result stays real.
    assert_eq!(int(get(task, "status")), 5, "{task:?}");
    assert_eq!(text(get(task, "termination")), "exited");
    assert_eq!(text(get(task, "state")), "completed");

    // A small delta is still delivered, with no marker.
    let small = session.parallel(&parallel_line(
        "p2",
        "",
        &[r#"{"id":"small","command":"SMALL=1","env_delta":true}"#.to_string()],
    ));
    let small_task = task_by_id(&small, "small");
    assert_eq!(text(get(get(small_task, "env_delta"), "SMALL")), "1");
    assert!(small_task.get("env_delta_omitted").is_none());
    session.finish();
}

#[test]
fn a_task_cannot_clobber_its_status_record_through_low_descriptors() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let response = session.parallel(&parallel_line(
        "p1",
        r#""max_concurrency":1,"#,
        &[
            r#"{"id":"clobber","command":"i=10; while [ \"$i\" -le 40 ]; do eval \"exec ${i}>/dev/null\"; i=$((i+1)); done; cd /tmp; export MARK=1","env_delta":true}"#.to_string(),
            r#"{"id":"fds","command":"sh -c 'for f in /proc/$PPID/fd/*; do printf \"%s->%s\\n\" \"${f##*/}\" \"$(readlink \"$f\" 2>/dev/null || echo ?)\"; done'"}"#.to_string(),
        ],
    ));

    let clobber = task_by_id(&response, "clobber");
    assert_eq!(int(get(clobber, "status")), 0, "{clobber:?}");
    assert_eq!(text(get(clobber, "cwd")), "/tmp", "{clobber:?}");
    assert_eq!(
        text(get(get(clobber, "env_delta"), "MARK")),
        "1",
        "{clobber:?}"
    );

    let fds = task_by_id(&response, "fds");
    assert_eq!(int(get(fds, "status")), 0, "{fds:?}");
    let listing = text(get(fds, "stdout"));
    for line in listing.lines() {
        let Some((fd_str, target)) = line.split_once("->") else {
            continue;
        };
        let Ok(fd) = fd_str.parse::<i32>() else {
            continue;
        };
        if target.contains("agent-task-out") || target.contains("agent-task-err") {
            assert!(
                fd == 1 || fd == 2,
                "duplicate stream capture fd exposed in task child: {line}"
            );
        }
        if target.contains("agent-task-status") {
            assert!(
                fd >= 900,
                "status record fd exposed in low range (<900): {line}"
            );
        }
    }

    session.finish();
}
