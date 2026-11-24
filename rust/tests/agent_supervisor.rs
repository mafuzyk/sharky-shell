//! Regression tests for the supervision hardening unit (adversarial review
//! findings #9, #10, #2 and #8), all driven through the public protocol:
//!
//! * #9 — a cancel must take the whole task group down even when the leader is
//!   reaped before the `SIGTERM`→`SIGKILL` deadline passes;
//! * #10 — the group registry must not forget a live group while short task
//!   groups come and go, so the shutdown cleanup still reaches it;
//! * #2 — the pump queue is bounded by what it already holds, overflow frames
//!   are answered with a defined error, and a `cancel` still wins;
//! * #8 — a forked child does not inherit the supervisor self-pipe.
//!
//! Deadlock discipline (§10): one writer at a time per direction. The only
//! test that writes while the agent answers does so from a writer thread and
//! keeps reading on the test thread.

mod common;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use common::agent::{Session, json};

/// Mirrors `supervisor.rs`'s accumulated pump bound. The wire contract does
/// not advertise it; the test only needs to send more frames than it holds.
const MAX_QUEUED_FRAMES: usize = 1024;

// -------------------------------------------------------------------- helpers

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

fn array(value: &json::Value) -> &[json::Value] {
    value
        .as_array()
        .unwrap_or_else(|| panic!("not an array: {value:?}"))
}

fn kind(value: &json::Value) -> &str {
    value
        .get("type")
        .and_then(json::Value::as_str)
        .unwrap_or_else(|| panic!("error response without a type: {value:?}"))
}

fn ready(session: &mut Session) {
    let response = session.json();
    assert_eq!(kind(&response), "ready", "{response:?}");
}

fn task(id: &str, command: &str) -> String {
    format!(
        r#"{{"id":{},"command":{}}}"#,
        json::quote(id),
        json::quote(command)
    )
}

fn parallel_line(id: &str, fields: &str, tasks: &[String]) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"parallel",{fields}"tasks":[{}]}}"#,
        json::quote(id),
        tasks.join(",")
    )
}

fn exec_line(id: &str, command: &str) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"exec","command":{}}}"#,
        json::quote(id),
        json::quote(command)
    )
}

fn state_line(id: &str) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"state","include":[]}}"#,
        json::quote(id)
    )
}

fn close_line(id: &str) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"close"}}"#,
        json::quote(id)
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

fn task_by_id<'a>(response: &'a json::Value, id: &str) -> &'a json::Value {
    array(get(response, "tasks"))
        .iter()
        .find(|task| text(get(task, "id")) == id)
        .unwrap_or_else(|| panic!("no task {id} in {response:?}"))
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

/// Inode numbers of every `pipe:[...]` descriptor `pid` holds.
fn pipe_inodes(pid: u32) -> Vec<String> {
    let mut inodes = Vec::new();
    let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return inodes;
    };
    for entry in entries.flatten() {
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        if let Some(rest) = target.strip_prefix("pipe:[")
            && let Some(inode) = rest.strip_suffix(']')
        {
            inodes.push(inode.to_string());
        }
    }
    inodes.sort();
    inodes.dedup();
    inodes
}

// --------------------------------------------------------------- #9: group stop

/// A task whose leader exits on its own while a `SIGTERM`-ignoring descendant
/// keeps sleeping. The cancel is delivered while the leader still runs, so the
/// escalation is armed; the leader is reaped long before the grace expires.
/// Reaping it must not drop the `SIGKILL`: the group still has a member, and
/// the session stays open, so nothing else would ever clean it up.
#[test]
fn a_cancelled_task_group_dies_after_its_leader_is_reaped() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![task(
        "stubborn",
        r#"trap '' TERM; sh -c 'trap "" TERM; sleep 30' & echo $!; sleep 0.5; exit 0"#,
    )];
    session.send(&parallel_line("p1", r#""max_concurrency":1,"#, &tasks));
    std::thread::sleep(Duration::from_millis(150));

    // A grace well past the leader's own exit, so the reap happens first.
    session.send(&cancel_line("c1", "p1", Some("stubborn"), Some(1200)));
    let ack = session.json();
    assert_eq!(kind(&ack), "ack", "{ack:?}");
    assert_eq!(text(get(&ack, "op")), "cancel");

    let response = session.json_within(5000).expect("the group answer");
    assert_eq!(kind(&response), "parallel", "{response:?}");
    let entry = task_by_id(&response, "stubborn");
    assert_eq!(text(get(entry, "state")), "cancelled", "{entry:?}");
    assert_eq!(text(get(entry, "termination")), "cancelled");
    assert_eq!(int(get(entry, "status")), 143);

    let descendant: i64 = text(get(entry, "stdout"))
        .trim()
        .parse()
        .expect("the task prints the descendant pid");
    assert!(
        gone_within(descendant, 3000),
        "descendant {descendant} survived the cancel while the session stayed open"
    );

    // The session is still serving: the escalation did not take it down.
    session.send(&exec_line("after", "echo alive"));
    let after = session.json();
    assert_eq!(kind(&after), "result", "{after:?}");
    assert_eq!(text(get(&after, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ---------------------------------------------------------- #10: group registry

/// A task group that outlives its leader (`sleep 30 &`) must stay registered
/// while many short task groups come and go, so the shutdown cleanup still
/// reaches it.
#[test]
fn a_long_lived_group_still_dies_at_close_after_many_task_groups() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![task("sentinel", "sleep 30 & echo $!")];
    let response = session.parallel(&parallel_line("sentinel", "", &tasks));
    let sentinel: i64 = text(get(task_by_id(&response, "sentinel"), "stdout"))
        .trim()
        .parse()
        .expect("the sentinel prints its pid");
    assert!(!gone(sentinel), "the sentinel must outlive its leader");

    // 1152 short task groups: more than the registry bound, spread over
    // requests so each one registers a fresh window of live groups.
    let started = Instant::now();
    for request in 0..9 {
        let tasks: Vec<String> = (0..128)
            .map(|index| task(&format!("t{request}-{index}"), "true"))
            .collect();
        let response = session.parallel(&parallel_line(
            &format!("s{request}"),
            r#""max_concurrency":32,"#,
            &tasks,
        ));
        assert_eq!(kind(&response), "parallel");
        assert_eq!(int(get(&response, "status")), 0, "{response:?}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the saturation requests must stay cheap: {:?}",
        started.elapsed()
    );

    session.send(&close_line("done"));
    let ack = session.json();
    assert_eq!(kind(&ack), "ack", "{ack:?}");
    let closed = session.json();
    assert_eq!(kind(&closed), "closed", "{closed:?}");
    assert!(
        gone_within(sentinel, 5000),
        "the long-lived group {sentinel} was forgotten by the registry"
    );
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// -------------------------------------------------------------- #2: pump queue

/// A minimal client for the two tests that have to write while the agent
/// answers: the writer lives in its own thread, so a full pipe in either
/// direction stalls one side instead of deadlocking the test.
struct Flood {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    buffer: Vec<u8>,
}

impl Flood {
    fn spawn() -> Flood {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
        command
            .arg("agent")
            .arg("--stdio")
            .env("SHARKY_CONFIG", common::agent::NO_CONFIG)
            .env("PATH", common::tool_path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().expect("spawn sharky agent --stdio");
        let stdin = child.stdin.take().expect("agent stdin");
        let stdout = child.stdout.take().expect("agent stdout");
        Flood {
            child,
            stdin: Some(stdin),
            stdout,
            buffer: Vec::new(),
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("agent stdin is still open");
        stdin.write_all(line.as_bytes()).expect("write request");
        stdin.write_all(b"\n").expect("write request");
        stdin.flush().expect("flush request");
    }

    /// Hands the write end to a thread that streams `frames` and gives it back
    /// when the last one is flushed, so the test can keep talking afterwards.
    /// Ordering is the pipe's; nothing here waits on a response.
    fn flood(&mut self, frames: Vec<String>) -> std::thread::JoinHandle<ChildStdin> {
        let mut stdin = self.stdin.take().expect("agent stdin is still open");
        std::thread::spawn(move || {
            for frame in frames {
                stdin.write_all(frame.as_bytes()).expect("write frame");
                stdin.write_all(b"\n").expect("write frame");
            }
            stdin.flush().expect("flush frames");
            stdin
        })
    }

    /// Waits for the flood to finish and takes the write end back.
    fn settle_writer(&mut self, writer: std::thread::JoinHandle<ChildStdin>) {
        self.stdin = Some(writer.join().expect("writer thread"));
    }

    fn next(&mut self, timeout_ms: i64) -> Option<json::Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(0) as u64);
        loop {
            if let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.buffer.drain(..=position).collect();
                let text = String::from_utf8(line[..line.len() - 1].to_vec())
                    .expect("the protocol stream must stay UTF-8");
                return Some(
                    json::parse_line(text.as_bytes())
                        .unwrap_or_else(|error| panic!("response is not JSON: {text:?}: {error}")),
                );
            }
            let now = Instant::now();
            if now >= deadline {
                return None;
            }
            let wait_ms = (deadline - now).as_millis().max(1) as i32;
            let mut descriptor = nix::libc::pollfd {
                fd: self.stdout.as_raw_fd(),
                events: nix::libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid descriptor with a millisecond timeout.
            let ready = unsafe { nix::libc::poll(&mut descriptor, 1, wait_ms) };
            assert!(ready >= 0, "poll on the agent stdout failed");
            if ready == 0 {
                continue;
            }
            let mut chunk = [0u8; 8192];
            // SAFETY: the buffer is valid for writes of its own length.
            let count = unsafe {
                nix::libc::read(
                    self.stdout.as_raw_fd(),
                    chunk.as_mut_ptr() as *mut _,
                    chunk.len(),
                )
            };
            if count <= 0 {
                return None;
            }
            self.buffer.extend_from_slice(&chunk[..count as usize]);
        }
    }

    fn expect(&mut self, timeout_ms: i64) -> json::Value {
        self.next(timeout_ms)
            .expect("the agent closed or went quiet before answering")
    }

    fn finish(&mut self) -> Option<std::process::ExitStatus> {
        self.stdin.take();
        // Drain whatever is left so the child cannot block on a full pipe.
        while self.next(500).is_some() {}
        self.child.wait().ok()
    }
}

impl Drop for Flood {
    fn drop(&mut self) {
        self.stdin.take();
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Overflow frames are answered out of order with one defined error; the
/// frames that did fit are answered later, in order, after the running request.
#[test]
fn the_pump_queue_is_bounded_and_keeps_response_order() {
    let mut agent = Flood::spawn();
    let ready_line = agent.expect(5000);
    assert_eq!(kind(&ready_line), "ready");

    agent.send(&exec_line("slow", "sleep 2"));
    let queued = MAX_QUEUED_FRAMES;
    let total = MAX_QUEUED_FRAMES + 1076;
    let frames: Vec<String> = (0..total)
        .map(|index| state_line(&format!("q{index}")))
        .collect();
    let writer = agent.flood(frames);

    // The overflow answers arrive while the exec still runs.
    let started = Instant::now();
    let mut rejected = Vec::new();
    while rejected.len() < total - queued {
        let response = agent.expect(10_000);
        assert_eq!(kind(&response), "error", "{response:?}");
        assert_eq!(
            text(get(get(&response, "error"), "code")),
            "QUEUE_FULL",
            "overflow must be a typed error: {response:?}"
        );
        assert_eq!(
            get(get(&response, "error"), "fatal").as_bool(),
            Some(false),
            "an overflow must not end the session: {response:?}"
        );
        rejected.push(
            response
                .get("id")
                .and_then(json::Value::as_str)
                .expect("the overflow error carries the frame id")
                .to_string(),
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "overflow frames must be rejected without waiting for the exec: {:?}",
        started.elapsed()
    );
    let expected: Vec<String> = (queued..total).map(|index| format!("q{index}")).collect();
    assert_eq!(
        rejected, expected,
        "the overflow must hit the newest frames"
    );

    // The accepted frames keep their order behind the running request.
    let slow = agent.expect(10_000);
    assert_eq!(kind(&slow), "result", "{slow:?}");
    assert_eq!(text(get(&slow, "id")), "slow");
    for index in 0..queued {
        let response = agent.expect(10_000);
        assert_eq!(kind(&response), "state", "{response:?}");
        assert_eq!(
            text(get(&response, "id")),
            format!("q{index}"),
            "the queued frames must keep request order"
        );
    }
    agent.settle_writer(writer);
    // Still serving: a normal request after the flood answers with its own
    // status, which is also what makes the session exit zero.
    agent.send(&exec_line("final", "echo done"));
    let final_line = agent.expect(5000);
    assert_eq!(text(get(&final_line, "stdout")), "done\n");
    assert_eq!(agent.finish().and_then(|status| status.code()), Some(0));
}

/// A `cancel` behind a saturated queue is still read, answered and honored: the
/// exec ends long before its own `sleep` would.
#[test]
fn a_cancel_is_honored_while_the_pump_queue_is_saturated() {
    let mut agent = Flood::spawn();
    assert_eq!(kind(&agent.expect(5000)), "ready");

    agent.send(&exec_line("slow", "sleep 5"));
    let queued = MAX_QUEUED_FRAMES;
    let overflow = 76;
    let mut frames: Vec<String> = (0..queued + overflow)
        .map(|index| state_line(&format!("q{index}")))
        .collect();
    frames.push(cancel_line("c1", "slow", None, Some(200)));
    let writer = agent.flood(frames);

    let started = Instant::now();
    for _ in 0..overflow {
        let response = agent.expect(10_000);
        assert_eq!(kind(&response), "error", "{response:?}");
    }
    let ack = agent.expect(10_000);
    assert_eq!(kind(&ack), "ack", "{ack:?}");
    assert_eq!(text(get(&ack, "outcome")), "delivered");

    let slow = agent.expect(10_000);
    assert_eq!(kind(&slow), "result", "{slow:?}");
    assert_eq!(text(get(&slow, "id")), "slow");
    assert_eq!(text(get(&slow, "termination")), "cancelled");
    assert_eq!(int(get(&slow, "status")), 143);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the cancel was stuck behind the queue: {:?}",
        started.elapsed()
    );

    // Saturation did not break the queue: every accepted frame is still served.
    for index in 0..queued {
        let response = agent.expect(10_000);
        assert_eq!(kind(&response), "state", "{response:?}");
        assert_eq!(text(get(&response, "id")), format!("q{index}"));
    }
    agent.settle_writer(writer);
    // The queue survived saturation: a fresh request is answered, and its
    // status is the session's exit status.
    agent.send(&exec_line("final", "echo done"));
    let final_line = agent.expect(5000);
    assert_eq!(text(get(&final_line, "stdout")), "done\n");
    assert_eq!(agent.finish().and_then(|status| status.code()), Some(0));
}

// ------------------------------------------------------- #8: self-pipe hygiene

/// A forked child of the session keeps the interpreter image, so descriptor
/// hygiene is `child_guard`'s job. The self-pipe belongs to the session only: a
/// child that inherits it can hold the pipe open and see wake-up bytes.
#[test]
fn a_forked_child_does_not_inherit_the_self_pipe() {
    let ambient_pipes = pipe_inodes(std::process::id());
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    // A background job keeps the forked interpreter alive while the parent test
    // inspects it, and `$!` names it.
    session.send(&exec_line("bg", "sleep 0.6 & echo $!"));
    let response = session.json();
    assert_eq!(kind(&response), "result", "{response:?}");
    let background: u32 = text(get(&response, "stdout"))
        .trim()
        .parse()
        .expect("the request prints the background pid");

    let session_pipes: Vec<String> = pipe_inodes(session.pid())
        .into_iter()
        .filter(|inode| !ambient_pipes.contains(inode))
        .collect();
    assert!(
        session_pipes.len() >= 2,
        "the session owns the self-pipe pair: {session_pipes:?}"
    );
    // The child is still sleeping; the fds are inherited at fork time.
    let child_pipes = pipe_inodes(background);
    for inode in &session_pipes {
        assert!(
            !child_pipes.contains(inode),
            "child {background} inherited the session pipe {inode}: {child_pipes:?}"
        );
    }

    // The child exits on its own; the session stays healthy.
    assert!(gone_within(i64::from(background), 3000));
    session.send(&exec_line("after", "echo alive"));
    let after = session.json();
    assert_eq!(text(get(&after, "stdout")), "alive\n");
    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// ------------------------------------------------------ fatal signals (review)

fn send_sigterm(pid: u32) {
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("send SIGTERM");
}

#[test]
fn sigterm_ends_an_idle_session_with_status_143() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    send_sigterm(session.pid());
    let status = session
        .wait_within(3000)
        .expect("idle session ignored SIGTERM");
    assert_eq!(status.code(), Some(143));
}

#[test]
fn sigterm_ends_a_running_exec_and_cleans_up_its_child() {
    let dir = std::env::temp_dir().join(format!("sharky-sigterm-exec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let pid_file = dir.join("child.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let command = format!(
        "sh -c 'echo $$ > {}; exec sleep 30'",
        pid_file.display()
    );
    session.send(&exec_line("slow", &command));

    let deadline = Instant::now() + Duration::from_secs(3);
    let child_pid: i64 = loop {
        if let Ok(raw) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = raw.trim().parse::<i64>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "exec child never wrote its pid");
        std::thread::sleep(Duration::from_millis(20));
    };

    send_sigterm(session.pid());
    let status = session
        .wait_within(4000)
        .expect("exec session ignored SIGTERM");
    let child_gone = gone_within(child_pid, 2000);
    let _ = std::fs::remove_dir_all(&dir);
    if !child_gone {
        unsafe {
            nix::libc::kill(child_pid as i32, nix::libc::SIGKILL);
        }
    }
    assert_eq!(status.code(), Some(143));
    assert!(child_gone, "exec child {child_pid} leaked after SIGTERM");
}

#[test]
fn sigterm_ends_a_running_parallel_group_without_leaking_children() {
    let dir = std::env::temp_dir().join(format!("sharky-sigterm-par-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let pid_a = dir.join("a.pid");
    let pid_b = dir.join("b.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![
        task(
            "a",
            &format!("sh -c 'echo $$ > {}; exec sleep 30'", pid_a.display()),
        ),
        task(
            "b",
            &format!("sh -c 'echo $$ > {}; exec sleep 30'", pid_b.display()),
        ),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));

    let deadline = Instant::now() + Duration::from_secs(3);
    let (child_a, child_b): (i64, i64) = loop {
        if let (Ok(raw_a), Ok(raw_b)) = (
            std::fs::read_to_string(&pid_a),
            std::fs::read_to_string(&pid_b),
        ) && let (Ok(a), Ok(b)) = (raw_a.trim().parse::<i64>(), raw_b.trim().parse::<i64>())
        {
            break (a, b);
        }
        assert!(
            Instant::now() < deadline,
            "parallel children never wrote their pids"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    send_sigterm(session.pid());
    let status = session
        .wait_within(4000)
        .expect("parallel session ignored SIGTERM");
    let a_gone = gone_within(child_a, 2000);
    let b_gone = gone_within(child_b, 2000);
    let _ = std::fs::remove_dir_all(&dir);
    for pid in [child_a, child_b] {
        if !gone(pid) {
            unsafe {
                nix::libc::kill(pid as i32, nix::libc::SIGKILL);
            }
        }
    }
    assert_eq!(status.code(), Some(143));
    assert!(a_gone, "parallel child a ({child_a}) leaked after SIGTERM");
    assert!(b_gone, "parallel child b ({child_b}) leaked after SIGTERM");
}

// -------------------------------------------- queued duplicate ids (review #2)

#[test]
fn a_close_whose_id_duplicates_a_queued_frame_does_not_cancel_running_tasks() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![
        task("a", "sleep 0.4; echo ok_a"),
        task("b", "sleep 0.4; echo ok_b"),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));
    std::thread::sleep(Duration::from_millis(100));

    // A queued request holds `"dup"`, and a `close` with the same id arrives
    // behind it while the parallel group is still running.
    session.send(&state_line("dup"));
    session.send(&close_line("dup"));

    let parallel = session.json_within(5000).expect("parallel response");
    assert_eq!(kind(&parallel), "parallel", "{parallel:?}");
    assert_eq!(text(get(&parallel, "state")), "completed", "{parallel:?}");
    assert_eq!(int(get(&parallel, "status")), 0, "{parallel:?}");
    for id in ["a", "b"] {
        let entry = task_by_id(&parallel, id);
        assert_eq!(text(get(entry, "state")), "completed", "{entry:?}");
        assert_eq!(int(get(entry, "status")), 0, "{entry:?}");
    }

    let state = session.json();
    assert_eq!(kind(&state), "state", "{state:?}");
    assert_eq!(text(get(&state, "id")), "dup", "{state:?}");

    let dup_close = session.json();
    assert_eq!(kind(&dup_close), "error", "{dup_close:?}");
    assert_eq!(text(get(get(&dup_close, "error"), "code")), "DUPLICATE_ID");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn a_cancel_whose_id_duplicates_a_queued_frame_is_rejected_without_rejecting_the_earlier_frame() {
    let mut session = Session::spawn(&[]);
    ready(&mut session);

    session.send(&exec_line("slow", "sleep 0.5; echo done"));
    std::thread::sleep(Duration::from_millis(100));

    session.send(&state_line("dup"));
    session.send(&cancel_line("dup", "slow", None, Some(100)));

    let dup_cancel = session.json_within(5000).expect("duplicate cancel error");
    assert_eq!(kind(&dup_cancel), "error", "{dup_cancel:?}");
    assert_eq!(text(get(get(&dup_cancel, "error"), "code")), "DUPLICATE_ID");

    let slow = session.json_within(5000).expect("slow result");
    assert_eq!(kind(&slow), "result", "{slow:?}");
    assert_eq!(text(get(&slow, "id")), "slow");
    assert_eq!(text(get(&slow, "termination")), "exited", "{slow:?}");
    assert_eq!(text(get(&slow, "stdout")), "done\n");

    let state = session.json();
    assert_eq!(kind(&state), "state", "{state:?}");
    assert_eq!(text(get(&state, "id")), "dup", "{state:?}");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

// -------------------------------------------- substitution supervision (step 5)

fn exec_with_timeout(id: &str, command: &str, timeout_ms: i64) -> String {
    format!(
        r#"{{"schema_version":1,"id":{},"op":"exec","command":{},"timeout_ms":{timeout_ms}}}"#,
        json::quote(id),
        json::quote(command)
    )
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

#[test]
fn exec_timeout_interrupts_command_substitution_and_kills_child() {
    let dir = std::env::temp_dir().join(format!("sharky-cmdsub-timeout-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let pid_file = dir.join("child.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let command = format!(
        "x=$(sh -c 'echo $$ > {}; exec sleep 30')",
        pid_file.display()
    );
    session.send(&exec_with_timeout("cmdsub-to", &command, 250));

    let response = session.json_within(3000);
    let child_pid = read_pid_within(&pid_file, 1000).unwrap_or(0);
    let child_gone = child_pid > 0 && gone_within(child_pid, 2000);
    let _ = std::fs::remove_dir_all(&dir);
    if child_pid > 0 && !gone(child_pid) {
        unsafe {
            nix::libc::kill(child_pid as i32, nix::libc::SIGKILL);
        }
    }

    let response = response.expect("command substitution ignored exec timeout_ms");
    assert_eq!(kind(&response), "result", "{response:?}");
    assert_eq!(text(get(&response, "id")), "cmdsub-to");
    assert_eq!(text(get(&response, "termination")), "timeout", "{response:?}");
    assert_eq!(int(get(&response, "status")), 124, "{response:?}");
    assert!(child_pid > 0, "command substitution child never wrote its pid");
    assert!(
        child_gone,
        "command substitution child {child_pid} leaked after exec timeout"
    );

    session.send(&exec_line("after", "echo alive"));
    let after = session.json_within(3000).expect("follow-up exec");
    assert_eq!(kind(&after), "result", "{after:?}");
    assert_eq!(text(get(&after, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}

#[test]
fn parallel_cancel_kills_command_and_process_substitution_descendants() {
    let dir = std::env::temp_dir().join(format!("sharky-par-sub-cancel-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let pid_cmd = dir.join("cmd.pid");
    let pid_proc = dir.join("proc.pid");

    let mut session = Session::spawn(&[]);
    ready(&mut session);

    let tasks = vec![
        task(
            "cmdsub",
            &format!(
                "x=$(sh -c 'echo $$ > {}; exec sleep 30')",
                pid_cmd.display()
            ),
        ),
        task(
            "procsub",
            &format!(
                "cat <(sh -c 'echo $$ > {}; exec sleep 30')",
                pid_proc.display()
            ),
        ),
    ];
    session.send(&parallel_line("p1", r#""max_concurrency":2,"#, &tasks));

    let child_cmd = read_pid_within(&pid_cmd, 3000);
    let child_proc = read_pid_within(&pid_proc, 3000);

    session.send(&cancel_line("c1", "p1", None, Some(200)));
    let ack = session.json_within(3000);
    let parallel = session.json_within(4000);

    let cmd_gone = child_cmd.is_some_and(|pid| gone_within(pid, 2000));
    let proc_gone = child_proc.is_some_and(|pid| gone_within(pid, 2000));
    let _ = std::fs::remove_dir_all(&dir);
    for pid in [child_cmd, child_proc].into_iter().flatten() {
        if !gone(pid) {
            unsafe {
                nix::libc::kill(pid as i32, nix::libc::SIGKILL);
            }
        }
    }

    let child_cmd = child_cmd.expect("cmdsub child never wrote its pid");
    let child_proc = child_proc.expect("procsub child never wrote its pid");
    let ack = ack.expect("cancel ack");
    assert_eq!(kind(&ack), "ack", "{ack:?}");
    let parallel = parallel.expect("parallel cancel response timed out");
    assert_eq!(kind(&parallel), "parallel", "{parallel:?}");
    assert_eq!(text(get(&parallel, "state")), "cancelled", "{parallel:?}");
    for id in ["cmdsub", "procsub"] {
        let entry = task_by_id(&parallel, id);
        assert_eq!(text(get(entry, "state")), "cancelled", "{entry:?}");
    }
    assert!(
        cmd_gone,
        "command substitution descendant {child_cmd} leaked after parallel cancel"
    );
    assert!(
        proc_gone,
        "process substitution descendant {child_proc} leaked after parallel cancel"
    );

    session.send(&exec_line("after", "echo alive"));
    let after = session.json_within(3000).expect("follow-up exec");
    assert_eq!(kind(&after), "result", "{after:?}");
    assert_eq!(text(get(&after, "stdout")), "alive\n");

    let (exit, leftovers, errors) = session.finish();
    assert_eq!(exit.code(), Some(0));
    assert!(leftovers.is_empty(), "extra lines: {leftovers:?}");
    assert!(errors.is_empty(), "agent stderr: {errors}");
}
