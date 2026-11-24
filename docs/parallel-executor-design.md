# Parallel Executor — Architecture and Wire Contract

Status: **implemented (Fase D, U1–U6)** — this document was the proposal; the
executor, supervisor extensions, fail-fast policy and task-group guard are now
in the tree and the full test suite is green. User-facing contract: `sharky help
parallel` and `sharky help agent`.
(`SHARKY_AGENT_RUNTIME_CODE_MODE_ROADMAP.md` §8, with §5, §7, §10, §11, §12.3).
Repository: `sharky-fasea`, baseline commit `0cee5de` (the series that lands the
agent runtime over 3.7.0).

Remaining non-goals (§8, §12 in detail): workflows/DAGs, conditions, retries,
resource locks (including shared `target/` and any filesystem sandbox),
streaming task events and a `sharky parallel` CLI or process-isolation mode.
Parallel tasks are isolated interpreter copies over one shared filesystem; use
separate worktrees/directories for commands that write.

This document extends `docs/agent-runtime-mvp-design.md`; it never contradicts
it. The session protocol stays as designed there (`ready`/`exec`/`state`/
`cancel`/`close`, additive `ops` and `features` negotiation, fd 900+ control
channel, per-request temp-file capture, supervisor with pgid cancel TERM→KILL,
`Shell::session_eval`, `isolate_groups`). Everything here is local; nothing
merges or pushes until the roadmap completes.

Citations are `path:line` inside this worktree. Every claim about existing
behavior was read from the source.

---

## 0. Decision summary

| # | Question | Decision |
| --- | --- | --- |
| D1 | Wire contract | New `op:"parallel"` with `tasks[]`, `max_concurrency`, `policy` (`wait-all`/`fail-fast`); one `type:"parallel"` response with `tasks[]` in request order; aggregate `status` = first failing task in request order; no streaming |
| D2 | Isolation model | **Fork one child per task**, the child runs the command on its own copied `Shell` and reports through its capture files plus one small status record; parent `Shell` is never mutated |
| D3 | Concurrency and fairness | Driver-side window (no semaphore object, no threads): spawn in request order while `running < max_concurrency`; FIFO over a bounded queue; on `fail-fast` the queue is dropped and answered `cancelled`/`130` |
| D4 | Cancellation | Every task leader is its own process group; `cancel` targets the request id (whole group) or `target:"<request>"` + `task:"<id>"`; escalation reuses the supervisor's TERM→grace→KILL with one `Pending` per task |
| D5 | Output | Parallel captures with `fd >= 10`, read back per task with the `exec` truncation/binary rules, bounded by `max_capture_bytes` per stream **and** `max_parallel_output_bytes` for the whole response; nothing is ever written to the protocol stream by a task |
| D6 | Failure taxonomy | Per task `termination ∈ exited\|signaled\|timeout\|cancelled\|spawn_error` and `state ∈ completed\|cancelled\|timeout\|spawn_error`; `wait-all` always runs everything, `fail-fast` cancels running and drops queued |
| D7 | Non-goals/seams | No DAG, conditions, retries, locks, streaming events; Cargo target-dir contention explicitly not solved; `sharky parallel` CLI and a process-isolation mode are named seams |
| D8 | Tests | Roadmap §8.4 mapped to concrete scenarios, including an event-log proof that the limit is never exceeded and an explicit same-file conflict test |
| D9 | Implementation order | Seven narrow units, each with its tests; riskiest is the parallel wait loop plus the per-task supervisor table |

The one-line product promise: **parallel is a fan-out of isolated interpreter
copies over the same filesystem, not a sandbox and not a scheduler for dependent
work.**

---

## 1. Baseline facts this design builds on

1. The session driver is a single thread that owns every `waitpid`. `exec.rs:238`
   takes the blocking path outside a session and `exec.rs:269`
   (`wait_member_session`) pumps the supervisor between `WNOHANG` polls. There is
   no thread anywhere in `rust/src/agent/`; `std::thread` appears only in the
   shutdown cleanup (poll loop).
2. The supervisor is process-global state behind one `Mutex`:
   `ACTIVE`, `CONTROL_FDS`, `STATE` (`supervisor.rs:66-73`). It models **one**
   in-flight request: `in_flight`, `current` (the one foreground pgid),
   `timeout_at`, and one `pending` escalation (`supervisor.rs:92-121`). That
   single-request assumption is the main thing parallel has to extend.
3. `child_guard()` (`supervisor.rs:258`) is what makes a forked child safe: it
   flips `ACTIVE` false (so the child's waits never pump the protocol), sets the
   SIGCHLD self-pipe fd to `-1` (so the child cannot wake the parent's poll), and
   closes the high control descriptors. It cannot lock or allocate.
4. The existing fork idiom for a whole command is `run_json_command`
   (`cli.rs:315`): create unlinked capture files, `fork`, `dup2` them onto 1/2 in
   the child, let the child run the shell, `_exit`, then the parent waits and
   reads the files (`cli.rs:343-420`). Fork-per-task is this idiom generalized to
   N children.
5. `Shell` is **not `Send`**: `shlvl_tail: Option<*const Simple>`
   (`shell.rs:46`) is a raw pointer to a parse node. Any design that moves a
   `&mut Shell` to a worker thread needs an `unsafe impl Send` or an AST refactor.
6. `Shell` state that a task must inherit is exactly the state a subshell
   inherits: `env`, `exported`, `readonly_vars`, `funcs`, `arrays`, `assoc`,
   `aliases`, `cwd`, options, traps (`shell.rs:31`). `fork` gives all of it
   for free through copy-on-write.

   One precise exception to item 6's `options` applies at task start: a task
   evaluates its `command` as a fresh command string (`-c` semantics), not as a
   continuation of the session's input. The COW snapshot reaches the task with
   `errexit` and its internal `errexit_suppressed` flag explicitly cleared
   (`enter_task_child`, `shell.rs:857-858`), so a `set -e` set in the session
   does not abort a task body; a `set -e` inside the task's own command enables
   errexit normally, exactly as in `bash -c`. This is the same reason command
   substitution turns errexit off in its child, and why a task's reported
   `status` is final-command semantics — the last command's status, with no
   errexit-induced early exit — just like `-c`. Every other shell option
   (`nounset`, `xtrace`, `pipefail`, `noclobber`) is inherited unchanged through
   the COW copy; `enter_task_child` resets no option other than errexit.

7. `Shell::session_eval` (`shell.rs:739`) is the no-`finish` evaluator the
   session uses; `Session::exec` (`mod.rs:449-607`) is the reference sequence:
   capture creation → `StdioGuard` swap → `clear_interrupt` → `begin_request` →
   `session_eval` → `end_request` → restore → `read_back` → encode.
8. A forked child joins a group only when `job_pgid` is set:
   `spawn` gates on `job_pgid.is_none()` (`shell.rs:2398`), pipelines use
   `job_pgid.or(leader)` (`shell.rs:1268`), and `run_background`'s child sets
   `job_pgid = Some(getpid())` for its own subtree (`shell.rs:1118`). `job_pgid`
   starts `None` (`shell.rs:387`) and is only ever set inside a forked child
   (`shell.rs:1118`, `shell.rs:1938`); the remaining writes in `shell.rs` are
   resets to `None`. At the top level of the REPL, `-c`, a script and the agent
   session it is therefore always `None`.
9. `run_background` gives a background job its **own** group unconditionally
   (`shell.rs:1093`, `shell.rs:1114`), even non-interactively.
10. Bash runs an EXIT trap at subshell end only when the subshell itself set it;
    a trap inherited from the parent is visible via `trap -p` but does not fire
    (`bash -c "trap 'echo A' EXIT; ( echo hi )"` prints `hi` only;
    `bash -c "( trap 'echo B' EXIT; true )"` prints `B`). Verified against bash
    5.3.20 in this environment.
11. Capture discipline: descriptors always `>= 10` (`capture.rs:29`), unlinked,
    `O_CLOEXEC`, `read_back` reports real size plus `truncated`
    (`capture.rs:201-226`), invalid UTF-8 switches the whole stream to base64.
12. A forked child that execs is protected by `O_CLOEXEC`; a forked child that
    runs shell code is protected by `child_guard`. Both already apply here.

---

## 2. D1 — Wire contract

### 2.1 Request

```json
{"schema_version":1,"id":"p1","op":"parallel",
 "policy":"wait-all",
 "max_concurrency":3,
 "timeout_ms":120000,
 "tasks":[
   {"id":"status","command":"git status --short"},
   {"id":"diff","command":"git diff --stat","timeout_ms":30000},
   {"id":"find","command":"find src -name '*.rs'","cwd":"/repo","stdin":"","env":{"GIT_PAGER":"cat"}}
 ]}
```

| field | type | default | meaning |
| --- | --- | --- | --- |
| `id` | string | required | request id, same rules and duplicate detection as every other `op` |
| `op` | `"parallel"` | required | |
| `policy` | `"wait-all"` \| `"fail-fast"` | `"wait-all"` | group failure policy |
| `max_concurrency` | integer | `4` | tasks allowed to run at once; must be `1..=limits.max_parallel`; the effective window is `min(value, len(tasks))` |
| `timeout_ms` | integer | none | **group** wall-clock deadline, counted from the first spawn |
| `tasks[].id` | string | required | unique inside the request, `1..=max_id_bytes` |
| `tasks[].command` | string | required | shell source, evaluated by `session_eval`; empty/whitespace = valid no-op (status 0), as in `exec` |
| `tasks[].stdin` | string | none | payload; `tasks[].stdin_encoding` ∈ `utf8` (default) \| `base64`, same limits as `exec` |
| `tasks[].timeout_ms` | integer | none | per-task deadline, counted from that task's spawn (queued time does not count) |
| `tasks[].cwd` | string | session cwd | working directory for the task's `Shell` copy |
| `tasks[].env` | object | none | `{name: string\|null}` applied on top of the snapshot, in the child, after the fork |
| `tasks[].env_delta` | bool | **`false`** | report the task's own env delta |

`env_delta` defaults to `false` here, unlike `exec` (where it defaults to `true`).
The reason is mechanical, not cosmetic: an `exec` delta describes a mutation the
caller must know about, while a task delta is informational by construction
(D2 — the parent is never modified). Paying a full `env` clone and a delta
computation per task for a field no consumer may apply is waste in the hot path
of the exact feature that exists to be fast. A client that wants the delta asks
for it per task; the `ready.protocol.features` list advertises
`parallel_env_delta` so no client assumes it. If a requested delta is dropped
because it would overflow the status record, the response carries
`env_delta_omitted:"record_too_large"` (§2.3), so a client that asked for a delta
must check that marker rather than read an absent `env_delta` as "the
environment did not change".

`tasks[].cwd` and `tasks[].env` are applied **in the child**, by the child, with
the ordinary mechanisms (`chdir`, and an assignment loop over `shell.env` plus
`shell.exported` additions so `env`/`$VAR` and exported children agree). Doing it
in the child is what keeps the parent's `Shell` untouched even when the request
is malformed halfway through (§3.7).

`id` uniqueness is per request, not per connection: `p1/status` and `p2/status`
coexist. `cancel` disambiguates with an explicit `task` field (D4).

### 2.2 Validation and protocol errors

Decoding happens in `protocol.rs` next to `decode_exec`, in the same strict
style, and rejects the whole request (nothing runs) with the existing codes plus
three additive ones:

| condition | code |
| --- | --- |
| `tasks` missing, not an array, or empty | `INVALID_REQUEST` |
| more than `limits.max_parallel_tasks` tasks | `TOO_MANY_TASKS` |
| duplicate `tasks[].id`, empty or over-long id | `DUPLICATE_TASK_ID` / `INVALID_ID` |
| `policy` not one of the two values | `INVALID_REQUEST` |
| `max_concurrency` `0` or above `limits.max_parallel` | `INVALID_REQUEST` (message names the range; clamping would hide a client bug) |
| `tasks[].command` missing | `INVALID_REQUEST` |
| `tasks[].env` value not string/null | `INVALID_REQUEST` |
| bad base64 / over `max_stdin_bytes` | `INVALID_ENCODING` / `STDIN_TOO_LARGE` |

All of these are non-fatal; the session keeps serving. Adding codes is forward
compatible (`agent-runtime-mvp-design.md` §3.5); no existing code changes
meaning.

### 2.3 Response

One response per request, `type:"parallel"`. A distinct `type` is used instead of
`type:"result"` because the shape differs (tasks with their own streams) and a
client that does not know `parallel` must never be handed a `result` it will
misread. The `type` union is already the documented growth point
(`agent-runtime-mvp-design.md` §3.1/§D10), and a client can only receive this
line after it sent `op:"parallel"`, which an unaware client never does.

```json
{"schema_version":1,"id":"p1","type":"parallel","op":"parallel",
 "policy":"wait-all","state":"completed","status":124,"duration_ms":30037,
 "max_concurrency":3,"started_tasks":3,"output_truncated":false,
 "tasks":[
  {"id":"status","state":"completed","termination":"exited","status":0,
   "duration_ms":12,"cwd":"/repo","pgid":4301,
   "stdout":" M a.rs\n","stdout_encoding":"utf8","stdout_bytes":7,"stdout_truncated":false,
   "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false},
  {"id":"diff","state":"timeout","termination":"timeout","status":124,
   "duration_ms":30011,"cwd":"/repo","pgid":4302,
   "stdout":"","stdout_encoding":"utf8","stdout_bytes":0,"stdout_truncated":false,
   "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false}
 ]}
```

Group fields:

| field | meaning |
| --- | --- |
| `state` | `"completed"` or `"cancelled"` — the group-level outcome, never a substitute for per-task detail |
| `status` | aggregate numeric status (rule below), `0` when everything succeeded |
| `duration_ms` | wall clock from the first spawn to the last reap |
| `max_concurrency` | echoed, after validation |
| `started_tasks` | how many tasks actually spawned (less than `len(tasks)` only after `fail-fast`/cancel/group deadline) |
| `output_truncated` | true if the response budget forced any task stream to be dropped or cut (D5) |
| `tasks[]` | **request order**, always, one entry per requested task |

Per-task fields are the `ExecResult` set (`json.rs:1027-1038`) verbatim —
`id`, `status`, `termination`, `signal`, `duration_ms`/`duration_us`, `cwd`,
`stdout*`, `stderr*`, `env_delta`, `env_delta_omitted` (optional; only
`"record_too_large"` today), `pgid` — plus `state` and two further optional fields.
The example above prints only `duration_ms` for brevity; `duration_us` is
reported exactly as `ExecResult` already does (`json.rs:1035`, `json.rs:1073`).

* `state` is the coarse lifecycle outcome: `completed` (the child was reaped and
  its status is reported, including non-zero exit and signal death),
  `cancelled`, `timeout`, `spawn_error`. `termination` stays the precise cause
  (`exited`, `signaled`, `timeout`, `cancelled`, `spawn_error`). Both are
  present because the roadmap's example (`§8.3`) uses `state`, while every
  existing client of `exec` reads `termination`.
* `reason` ∈ `"fail_fast"`, `"group_cancel"`, `"task_cancel"`,
  `"group_timeout"` — present only when supervision ended the task, and it is
  the one place where "why" survives for a task that never started.
* `error` (`{"code","message"}`) on `spawn_error` (`SPAWN_FAILED`) and on the
  defensive no-result path (`INTERNAL`, §3.7).

`termination:"spawn_error"` and `state:"spawn_error"` are additive vocabulary;
`exec` never produces them, so no `exec` client can start emitting them.

### 2.4 Aggregate status rule (exact)

```
failed(t)  :=  t.termination != "exited" || t.status != 0
if no task satisfies failed(t):        status = 0
else:                                  status = status of the first failed task
                                                in REQUEST order
```

With the per-task status conventions already fixed by the MVP:

| task outcome | `status` |
| --- | --- |
| exited | its exit code |
| signaled | `128 + signal` |
| timeout | `124` |
| cancelled before spawn (queued) | `130` |
| cancelled while running | `143` |
| spawn error | `126` |

Consequences, stated so nobody has to reverse-engineer them:

* `wait-all` with one `false` and one `sleep` kill is non-zero, and the failing
  task is identifiable because every task carries its own status.
* `fail-fast` reports the trigger first: the task that failed is, by
  construction, the first failed one in request order **only when** no earlier
  task also failed. If two fail in the same window, the earlier one wins — a
  deterministic rule beats a "most severe" ranking nobody can predict.
* A group cancel yields `state:"cancelled"` and a `status` derived by the same
  rule, so it is `143` when any task had started and `130` when none had.
* A task whose result could not be read takes the `INTERNAL` defensive path
  (§3.7): its status is unknown but non-zero and the task carries
  `error.code=INTERNAL`, so `failed(t)` is true and it fails the aggregate
  rather than passing silently as a success.
* The rule never collapses to a boolean: the roadmap (§8.3) forbids that, and
  §10.4 requires that a transforming layer cannot erase failure state.

### 2.5 Single response, not streaming

Streaming is rejected for the MVP: it needs a drainer discipline and stable
event ordering rules, which the file-capture design deliberately avoids
(`agent-runtime-mvp-design.md` §5.1), and it would let a task's output arrive
before the group's own status, complicating cancel/ack ordering (§3.6 of the
MVP design: order is acceptance order, with `cancel` acks as the sole out-of-order
exception).

Seam, requiring no version bump: unknown request keys are **ignored**, not
rejected (forward compatibility, as in the rest of the protocol), so a future
`"stream":true` field simply has no effect today rather than answering
`INVALID_REQUEST`. When it is implemented the group emits additive events
`{"type":"task_event","group":"p1","task":"t1","event":"started"|"stdout_chunk"|...}`
plus a final `type:"parallel"` line. New fields are negotiated through
`ready.protocol.features`: a client must not assume an unlisted field is
honored just because the session accepted the request. Clients that ignore
unknown `type` values already survive it.

---

## 3. D2 — Isolation model (the core decision)

### 3.1 Options

| | (a) fork per task | (b) one `Shell` per task on a worker thread | (c) `sharky -c` per task |
| --- | --- | --- | --- |
| parent state isolation | total: the child owns a COW copy, the parent's `Shell` is never borrowed | total if each task gets its own `Shell` | total |
| state snapshot | free (COW of the session's shell) | needs N `Shell` clones — the type is not `Clone` and not `Send` | **lost**: a new process cannot see the session's functions, aliases, options or cwd without a serialization format that does not exist |
| mutability discipline | no `&mut` sharing at all — matches roadmap §5 ("no shared `&mut Shell`") | requires `unsafe impl Send` or an AST refactor (`shell.rs:46`) | no sharing |
| fork/thread hazard | none added: still zero threads | a thread that forks is exactly the hazard roadmap §7.2 names; the whole signal design assumes one thread | none |
| cancel scope | one pgid per task leader, descendants join it | same, but signalling from a thread needs its own ownership story | each child is already its own process group candidate; but its descendants are a second layer |
| capture | existing temp-file captures, `fd >= 10` | same | `--json -c` capture is lossy for binary (`from_utf8_lossy`, `cli.rs:481`) |
| cost per task | one `fork` of a warm interpreter (COW) | one thread + a `Shell` clone | one full startup + config load + parse of the rc files |
| result transport | inherited files + one status record (§3.6) | channels/thread join | re-parse of a JSON line on stdout |
| reaping | parent reaps its own children, exactly like today | one waiter per thread (two consumers of `waitpid`, roadmap §7.2 hazard) | parent reaps `sharky` children |

**Decision: (a) fork per task.** It is the only option that satisfies all four
non-negotiables at once — inheriting the session snapshot, no threads, no
`unsafe`, and one reap owner — and it is a generalization of code that already
ships (`cli.rs:343`, `shell.rs:1090`).

Rejected (b): `Shell` is not `Send` (`shell.rs:46`); making it `Send` is an AST
refactor plus an `unsafe` assertion about every field, and the result would put
`fork` on a multithreaded process for no benefit.

Rejected (c) for the MVP, kept as a seam: it loses the context snapshot
(functions, aliases, `set -o` state, `cwd`), pays a full process start and config
load per task, and reuses a capture path that destroys binary output. It is the
right answer for a future `"isolation":"process"` mode where a task must not
inherit interpreter state at all — the request field is additive and the same
driver can spawn `sharky` instead of forking. Naming it matters: (c) is *weaker*
on semantics and *stronger* on state separation, so it is a policy choice, not an
upgrade.

### 3.2 Spawn sequence (per task, in the driver)

For task `t` with its slot available:

1. Create the task's capture files **in the parent**: stdout, stderr, status
   record, plus a payload file when `stdin` is present. All are unlinked temp
   files, `fd >= 10`, `O_CLOEXEC`. Parent-side creation is deliberate: the parent
   must be able to read a task's output even if the child dies before opening
   anything, and it must be able to report `spawn_error` without a half-created
   task.
2. Flush the Rust stdio handles (`capture::flush_stdio`, `capture.rs:646`) before
   forking, exactly as `run_json_command` does (`cli.rs:337-338`); otherwise a
   buffered byte from the parent would surface inside a task's capture. Then
   `block_all()` (the mask idiom at `shell.rs:1262-1296`, `shell.rs:2389`) and
   `fork()`.
3. **Child**: `setpgid(0, 0)` (become a group leader), then `child_guard()`
   (drops `ACTIVE`, clears the self-pipe fd, closes the control fds), then
   `reset_child_traps()` (`shell.rs:5627`), then `restore_mask(mask)`.
   Then the task-specific preparation:
   * `dup2` the task's stdout/stderr/payload onto 1/2/0; close the group's other
     capture descriptors (§6);
   * `enter_task_child()` on the inherited `Shell` (new, tiny): `session_mode`
     stays on (so the `exec` builtin keeps refusing), `isolate_groups` stays on,
     and `job_pgid = Some(getpid())` — from that point every child the task
     creates joins the task's group instead of making its own
     (`shell.rs:1268`, `spawn` parent gate `shell.rs:2398`, subshell
     `shell.rs:1936`);
   * apply `tasks[].cwd` (`chdir`) and `tasks[].env`;
   * `session_eval(command)` exactly as `Session::exec` does (`mod.rs:536-552`),
     including `close_substitutions_from(0)` and the parse-error path
     (message on fd 2, status 2);
   * record the outcome in the status file with `capture::write_safely`
     (`capture.rs:597`) so it never depends on buffering, `flush_stdio()`, then
     `_exit(status & 0xFF)`.
4. **Parent**: `setpgid(child, child)` (closing the same race the shell closes at
   `shell.rs:2402-2410`), record `pgid` for cleanup (`supervisor::note_group`,
   `supervisor.rs:186`), insert the task in the supervisor table as `Running`
   with its deadline, and add it to the driver's `pending_pids`.

`fork` is only ever called with the supervisor `Mutex` **not** held: the driver
calls the spawn outside any `with_state` closure, so a child can never inherit a
locked mutex. This is a hard rule for the implementing workers.

### 3.3 Why the child cannot damage the session

* `child_guard` (`supervisor.rs:258`) already gives: no protocol pumping
  (`ACTIVE` false), no write into the parent's self-pipe, closed control
  descriptors. This is exactly the existing subshell guarantee.
* The parent's `Shell` is not borrowed while children run; `fork` copied it. Any
  field the child mutates dies with the child. That is the roadmap's "parallel
  does not modify the parent session" (§8.2), enforced by the process boundary
  rather than by discipline.
* fd 1/2 of the *parent* stay `/dev/null` between requests, and the task's
  captures are distinct files, so no task's output can reach the protocol
  stream. The protocol lives at fd 900+ and is closed in the child.

### 3.4 Group formation and pgid semantics

`setpgid(child, child)` in both parent and child makes the task leader a group
leader. Because `enter_task_child` sets `job_pgid`, **everything the task forks
in the foreground joins that group**:

* plain externals: parent gate `job_pgid.is_none()` is false, so no second group;
  the exec child does `setpgid(0, job_pgid)` (`shell.rs:2408`);
* pipeline members: `job_pgid.or(leader)` (`shell.rs:1268`);
* subshells: `job_pgid` is already `Some` (`shell.rs:1904` is skipped, the child
  joins at `shell.rs:1937`).

So `killpg(task_pgid, SIG)` reaches the whole foreground descendant tree of a
task, and `pgid` in the task result is the leader pid (verified meaning: the
value recorded before `waitpid`).

Two escape paths exist, both inherited from how the interpreter works. One is
closed by this design; the other is not.

1. **`&` inside a task — closed.** `run_background` gives the job its own group
   unconditionally (`shell.rs:1093`, `shell.rs:1114`), so cancelling the task
   would not reach it. Fixing it is one gated change: make the parent-side
   `setpgid`/`job_pgid` assignment conditional on `self.job_pgid.is_none()`
   (`shell.rs:1092-1094`, `shell.rs:1113-1118`), so a background job started in a
   forked child joins that child's group — which is what a non-interactive bash
   does. It is inert everywhere else: `job_pgid` starts `None`
   (`shell.rs:387`) and is only ever set inside a forked child
   (`shell.rs:1118`, `shell.rs:1938`); the other writes are resets to `None`
   (`shell.rs:6842`, `6922`, `7063`), so the REPL keeps its exact job-control
   behavior. **Include it as its own unit with its own tests** (D9 U6), because
   "cancel leaves no orphans" (§8.4) is not true without it for `sleep 30 & wait`
   inside a task.
2. **Command substitution.** The substitution child resets `job_pgid = None`
   (`shell.rs:6842`), so `$(sleep 30)` in a task runs in its own group and
   survives a task cancel. This is a **pre-existing** gap in `exec` too
   (`exec "echo $(sleep 100)"` + cancel leaves the `sleep` alive), not something
   parallel introduces. It is out of scope here and recorded as a defect with its
   own fix and differential test; parallel must not claim containment it does not
   have (§7, §11).

Daemonizing children that call `setpgid` themselves escape as well. The roadmap
(§7.2) refuses to promise against that; this document repeats the refusal.

### 3.5 Supervisor statics under N children

The single-request fields do not describe a group, so the supervisor gains an
optional parallel table next to them (still inside the same `Mutex`, still one
thread):

```
struct ParallelGroup {
    id: String,
    policy: Policy,
    concurrency: usize,
    tasks: Vec<TaskSlot>,          // request order; index = slot
    next_spawn: usize,
    running: usize,
    stop: Option<StopReason>,      // FailFast | CancelGroup | GroupTimeout
    group_deadline: Option<Instant>,
}
struct TaskSlot {
    id: String,
    timeout_ms: Option<i64>,
    phase: Queued | Running { pid: i32, started: Instant } | Done(Outcome),
    pending: Option<Pending>,      // same struct as supervisor.rs:85
}
```

`Pending` (`verdict`, `kill_at`, `term_sent`, `kill_sent`) is reused unchanged;
there is simply one per running task instead of one per session. `Outcome` grows
`SpawnFailed` and a `StopReason` (existing `Cancelled`/`Timeout` values keep their
meaning).

Why the table lives here and not in the driver: `pump_input` →
`consume_cancel` → `handle_cancel` runs inside the `Mutex` while the driver waits,
and it must answer `ack`/`CANCEL_UNKNOWN_ID` immediately, with knowledge of the
task ids (§5). Keeping a second copy of the ids in the driver would be two sources
of truth for the one decision that must be atomic with signalling. The driver
keeps only what is not protocol state: capture handles keyed by slot index.

Consequences to implement carefully:

* `begin_request` (`supervisor.rs:403`) must be called with the group id, and set
  `current = 0` so the single-request escalation cannot fire on a stale pgid.
* `check_deadlines` (`supervisor.rs:341`) branches: with a parallel group it
  walks the tasks, arms `pending` for any deadline that passed, escalates each
  task's own `term_sent`/`kill_at`, and applies the group deadline to all running
  tasks (timeout) plus to queued tasks (never started: `cancelled`/`130`, reason
  `group_timeout`).
* `resolve_cancel` (`supervisor.rs:562`) gains a third resolution path: if
  `in_flight == group`, then `target == group` cancels the whole group; a
  `CancelRequest` carrying `task` resolves against the group's slots
  (`CANCEL_UNKNOWN_ID` / `CANCEL_TOO_LATE` reuse the existing codes, with the
  task id in the message). Signalling stays `killpg(task.pgid, ...)`.
* `end_request` (`supervisor.rs:422`) is not used by the parallel path; the group
  is ended by `end_parallel() -> Vec<(slot, Outcome)>` after the last reap, so
  every task that supervision terminated is labelled exactly once.
* `note_group` (`supervisor.rs:186`) is called per spawned task, so a `close`,
  EOF, fatal signal or `PROTOCOL_FD_LOST` during a group still kills every task
  group through `cleanup` (`supervisor.rs:666`), the same guarantee `exec` has.

### 3.6 Status record (how a task reports more than an exit code)

Exit status is 8 bits and carries neither `cwd`, nor the signal-vs-exit
distinction, nor `env_delta`. So the child writes one line to its status capture:

```json
{"status":0,"termination":"exited","cwd":"/repo","env_delta":{}}
```

The record carries only `status`, `termination`, `cwd`, `env_delta` and an
optional `message` (`parallel.rs:918`); `duration_ms` is not in it — the public
duration is measured by the parent from spawn to reap (`parallel.rs:457`), so it
covers the task's whole life, `EXIT` trap included. Encoded with the existing
encoders (`json.rs`), parsed in the parent with `json::parse_line` — the same
strict parser the protocol uses, no new format and no new dependency. The parent
reads it with its own bound `RECORD_LIMIT_BYTES = 4 MiB` (`parallel.rs:69`),
independent of `max_line_bytes`: the record can carry a large env delta, far
bigger than a protocol line. A delta that would exceed the bound is dropped and
the response instead carries `env_delta_omitted:"record_too_large"`
(`parallel.rs:980`), so a requested delta is never lost silently.

**The record is an optimization, never a requirement.** A SIGKILLed child leaves
a truncated or empty file, so no record is read; the parent then derives
`termination` from the wait status (`Signaled` → `signaled`, `128+signal`),
`duration_ms` from its own monotonic clock, `cwd` from the request's effective
cwd, and `env_delta` is absent. A missing or unparseable record is therefore not
an error, just less detail — which is what makes the design survive a task being
killed mid-write.

### 3.7 Reaping ownership

* The driver is the only reaper of task leaders, and it waits **per pid**
  (`waitpid(pid, WNOHANG)`), never `waitpid(-1, ...)`. `reap_all`
  (`supervisor.rs:722`) exists only in session shutdown; a global reap inside the
  wait loop would steal the status of a background job still in the job table,
  which is precisely the roadmap §7.2 "two consumers" hazard.
* Each task leader reaps its own descendants through the ordinary shell path; a
  descendant orphaned by a dead leader is re-parented away but still in the
  task's group, which is why the group — not the leader pid — is what cancel
  signals.
* `ECHILD` on a task pid, or any other case where a task has no result to read,
  is treated as "already reaped, status unknown" (defensive; can only happen if
  a future path reaps globally). The contract is that the unknown status is
  **non-zero** and the task carries `error.code=INTERNAL` — never a silent
  success. Under §2.4 that non-zero status makes the task fail the aggregate.

---

## 4. D3 — Concurrency limit and fairness

**Mechanism:** a window in the driver's loop, not a semaphore object and not a
thread pool.

```
loop {
    while running < max_concurrency && next_spawn < tasks.len() && stop.is_none() {
        spawn(tasks[next_spawn]); next_spawn += 1;
    }
    if running == 0 && (next_spawn == tasks.len() || stop.is_some()) { break }
    service();            // pump frames (cancel) + enforce deadlines
    drain_reaped();       // per-pid WNOHANG; each reap frees one slot
    wait_poll();          // poll(self-pipe, control_fd, 50 ms)
}
```

* **Fairness is request order, FIFO.** A slot is offered to the lowest request
  index that has not started. There is no priority, no reordering, no
  work-stealing. That is the strongest fairness claim available without a
  scheduler, and it makes the concurrency test deterministic.
* **Bounded queue.** `tasks.len() <= limits.max_parallel_tasks` (default 128) is
  enforced at decode; there is no dynamic submission, so the queue cannot grow.
* **Defaults.** `max_concurrency` default `4`, accepted range
  `1..=limits.max_parallel` (default cap `32`). Four keeps fd pressure and CPU
  contention low while still proving real overlap; an agent that knows better
  asks for more. Both numbers are advertised in `ready.limits`, so no client
  hardcodes them.
* **`fail-fast` and the queue.** When a task fails, `stop = FailFast`: queued
  tasks are never spawned and are answered `state:"cancelled"`,
  `termination:"cancelled"`, `status:130`, `reason:"fail_fast"`, empty streams,
  `duration_ms:0`; running tasks get the cancel escalation (D4). "Dropped" is
  therefore *visible* in the response, not an omission — a task list that
  silently lost entries would be worse than no policy at all.
* **No global concurrency governor** across requests, because the session is
  serial: a second `parallel` request queues as a frame until the first answers
  (MVP §3.6). Total process count can still exceed `max_concurrency` through
  descendants; the limit bounds *tasks*, and the doc says so.

---

## 5. D4 — Cancellation

**Targets.**

* `{"id":"c9","op":"cancel","target":"p1"}` cancels the whole group: running
  tasks TERM→KILL, and the tasks of that group which have not started yet are
  dropped (`state:"cancelled"`, `status:130`, `reason:"group_cancel"`).
  `ack{"outcome":"delivered"}` is written immediately, out of order, exactly as
  `exec` cancels are (`supervisor.rs:554`).
* `{"id":"c9","op":"cancel","target":"p1","task":"diff"}` cancels one task.
  `task` is a new optional field on `CancelRequest` (`protocol.rs:112`); it is
  only meaningful when `target` names a parallel request. On an `exec` target it
  is `INVALID_REQUEST` (an explicit error beats silently ignoring a field the
  client believes is honored). Advertised as feature `parallel_task_cancel`.
* Unknown task → `CANCEL_UNKNOWN_ID`; already finished → `CANCEL_TOO_LATE`. No
  new codes: the existing pair says exactly this.
* **A group cancelled while it was still queued** never spawns. `resolve_cancel`
  already records that case in `cancelled_queued` (`supervisor.rs:562-600`) for
  `exec`; the parallel path reads the same flag at group start (`take_queued_cancelled`,
  `supervisor.rs:438`) and answers every task `cancelled`/`130`,
  `reason:"group_cancel"`, `started_tasks:0`, `duration_ms:0`. Parity with
  `exec` matters here: a client that cancels before the session reaches the
  request must get the same shape of answer, not a group that runs anyway.

**Signal scope.** `killpg(pgid)` of the task leader, never the agent's own group
— the reason the leader is a group leader at all (§3.4). The parent's own group
is untouched, so a cancel can never take the protocol down with the task.

**Escalation** reuses `Pending` semantics verbatim: `SIGTERM` to the group, then
`SIGKILL` when `kill_at` passes, driven by `check_deadlines` inside the pump the
driver already calls. No threads, no timers, no handler that can re-enter the
interpreter — the same property the MVP design bought for `exec`.

**A task cancelled while queued** never spawns; a task cancelled while running
reports `termination:"cancelled"`, `status:143` regardless of the exit code the
command produced, matching MVP §8.2.

**Group cleanup on session end.** Because every spawned task group is recorded
with `note_group`, a `close`, transport EOF, fatal signal or
`PROTOCOL_FD_LOST` during a group runs the ordinary `cleanup` path: TERM all
recorded groups, wait `wait_ms`, KILL what is left. Parallel adds no second
cleanup mechanism.

**`close`/EOF during a running group: the response comes first.** A `close`
frame, transport EOF or a control-channel read failure seen while a group runs
stops the group immediately — TERM every task group, escalation through the same
`Pending` — and a `close` frame stays queued for the driver. The accepted
`parallel` request is still answered first (`state:"cancelled"`, running tasks
`cancelled`/`143` `reason:"group_cancel"`, queued tasks `cancelled`/`130`), and
only then is the `close` acked and `closed` written. This is the `exec` ordering
rule — every accepted request is answered, a `cancel` ack being the sole
out-of-order line — applied to a terminal frame, so a response slot never
silently disappears. EOF has nobody to answer; there the write is best-effort
and `cleanup` is the guarantee. A fatal signal takes the same stop path, and the
normal shutdown cleanup still kills every recorded group.

**Cancellation of a queued request.** A `cancel` whose target is still a queued
frame (a `parallel` accepted while another request runs) never spawns anything:
the driver reads `take_queued_cancelled` before `begin_parallel` and answers
every task `cancelled`/`130`, `reason:"group_cancel"`, `started_tasks:0`,
`duration_ms:0`. A `task` on such a target is `INVALID_REQUEST`: the slots do
not exist until the group starts, and an explicit error beats silently
pretending to cancel one task of a request that never began.

**Idempotence.** A second cancel for the same task or group is a no-op
`ack{"outcome":"delivered"}` while the target still exists, `CANCEL_TOO_LATE`
after it finishes — same as `exec`.

---

## 6. D5 — Output handling

* **Per-task captures**, created by the parent at spawn time (a slot has just
  opened, so no descriptor is held for a task that is still queued), and closed
  as soon as the task's result is rendered: stdout, stderr, status record,
  optional payload. Unlinked temp files, `fd >= 10`, `O_CLOEXEC`
  (`capture.rs:29`, `capture.rs:77`). Peak descriptors are therefore
  `4 × max_concurrency` (128 at the default cap of 32), independent of the task
  count, which is why `max_parallel_tasks` can default to 128 without fd trouble
  (the roadmap's 100-task scenario fits inside the default).
* **Descriptor hygiene in the child.** The child closes every capture descriptor
  of the group that is not one of its own three (stdout, stderr, status) before
  running. Otherwise a task's subshell could hold another task's status file open
  (harmless for the protocol, corrosive for result integrity) and a leaked fd
  would keep an unlinked file alive after the parent closed it.
* **Read-back rules are `exec`'s, unchanged**: read at most `max_capture_bytes`
  from offset 0 with `pread` (`capture.rs:201`), report the real size from
  `fstat` as `*_bytes`, `truncated` when the returned bytes are fewer. Valid
  UTF-8 → `encoding:"utf8"`; invalid UTF-8 anywhere → the whole stream is
  base64, no lossy replacement, no mixed encoding.
* **Total memory bound.** Read-back happens per task, at reap time, and consumes a
  group budget `max_parallel_output_bytes` (default 8 MiB). The per-stream limit
  for a task is `min(max_capture_bytes, remaining_budget)`; when the budget is
  exhausted the remaining tasks report empty streams with
  `*_truncated:true` and the real `*_bytes`, and the group carries
  `output_truncated:true`. The budget charges **raw captured bytes** — the text
  or binary byte count read back from the capture — and not the rendered JSON
  string. `*_bytes` is always the real stream size from `fstat`, and a stream cut
  by either the per-stream cap or the group budget keeps its own
  `*_truncated:true`, so nothing is hidden. Because an invalid UTF-8 stream is
  base64-rendered at 4/3 of its raw size, the rendered response can overshoot the
  advertized budget by up to that factor: worst case is roughly
  `4/3 × max_parallel_output_bytes` of rendered bytes. This overrun is known and
  documented; the budget bounds raw input, not the final line. Even with that
  overrun, 128 tasks × 4 MiB × 2 cannot turn into gigabytes of RSS: the retained
  output is still bounded by the budget.
* **No interleaving.** Each task writes to its own files; the parent writes
  exactly one JSON line at the end. There is no shared pipe, so no interleaving
  is possible even in principle, and a task cannot write to the protocol because
  its fd 1/2 are files and fd 900+ is closed by `child_guard`.
* **Background output after the task ends.** A job that outlives its task keeps
  writing to the (unlinked) task capture, which the parent has already read and
  will not read again. Same documented loss as MVP §5.2; it cannot corrupt
  anything.

---

## 7. D6 — Failure taxonomy

| Event | Task result | Group effect |
| --- | --- | --- |
| task exits 0 | `state:"completed"`, `termination:"exited"`, `status:0` | none |
| task exits non-zero | `completed`/`exited`/code | fails the group; `fail-fast` triggers |
| task dies by a signal it did not get from us | `completed`/`signaled`/`128+n` + `signal` | fails the group |
| task exceeds its own `timeout_ms` | `timeout`/`timeout`/`124` | fails the group |
| group `timeout_ms` expires (running) | `timeout`/`timeout`/`124` | group stops, queued tasks `cancelled`/`130` `reason:"group_timeout"` |
| group `timeout_ms` expires (queued) | `cancelled`/`cancelled`/`130` | never spawned |
| `cancel` on the task | `cancelled`/`cancelled`/`143` (`reason:"task_cancel"`) | never triggers `fail-fast` (a cancel is not a failure signal), but the task's non-zero status still makes the aggregate non-zero |
| `cancel` on the group | `cancelled`/`cancelled`/`143` running, `130` queued (`reason:"group_cancel"`) | `state:"cancelled"` |
| `fail-fast` stop | running `cancelled`/`143`, queued `cancelled`/`130`, `reason:"fail_fast"` | `started_tasks < len(tasks)` |
| `fork` fails, or the child's `chdir`/env application fails before eval | `spawn_error`/`spawn_error`/`126` + `error{SPAWN_FAILED}` | fails the group |
| capture creation fails in the parent (e.g. `EMFILE`) | `spawn_error`/`spawn_error`/`126` + `error{SPAWN_FAILED}` naming the capture | fails the group like any other spawn error; the remaining tasks run and the session keeps serving |
| the process-group registry is full of live groups | `spawn_error`/`spawn_error`/`126` + `error{SPAWN_FAILED}` (`"process-group registry is full"`), checked before the fork | no child exists for the refused task; the group fails like any other spawn error |
| parse error in a task | `completed`/`exited`/`2`, message on the task's stderr | fails the group (it is a nonzero exit) |

A capture creation failure is a **degraded task**, not a fatal request: the
parent cannot open one task's stdout/stderr/status files (typically fd pressure,
`EMFILE`), so that task is answered `spawn_error`/`126` with a visible
`error{SPAWN_FAILED}` naming the capture, while every sibling task still runs and
the session stays alive. Failing the whole request with a fatal `error{INTERNAL}`
would hide a partial result and turn a local resource hiccup into a lost batch,
so the failure is reported on the task that actually suffered it.

The session records every group it creates for shutdown cleanup, and a group it
cannot record could outlive it. The spawn window therefore asks
`can_track_group()` **before** forking: the probe compacts groups that no longer
exist, and only a registry full of live groups refuses, so the task is answered
`spawn_error`/`126` without a process ever existing. The supervisor keeps its
own refusal as a last resort: a group that registers between the probe and the
fork is killed rather than silently forgotten.

`wait-all` never aborts: every task runs, every result is present, and the
aggregate status is the §2.4 rule. `fail-fast` stops spawning at the first
failure, cancels what runs, and still reports every requested task.

`exit` inside a task ends **the task**, not the session: the child takes
`exit_requested`, uses it as the task status, and `_exit`s. This is a deliberate,
documented difference from `exec` (session-terminal, MVP §6.3) and it follows
from the isolation model: a task is a subshell, and a subshell's `exit` is local.

`exit` also implies the EXIT-trap question. A task child inherits the session's
EXIT trap, which must **not** fire per task (bash 5.3.20: a subshell runs the
EXIT trap only if it defined one, §1.10). Implementation: the child records the
inherited action, and after `session_eval` calls `Shell::finish(status)`
(`shell.rs:562`) only when the action changed — `finish` is what makes a trap set
*inside* the task run, and skipping it when unchanged is what keeps a
session-level `trap ... EXIT` from firing once per task. A differential test belongs
with the unit (D9 U3).

---

## 8. D7 — Non-goals and their seams

| Not in scope now | Why | Seam |
| --- | --- | --- |
| DAG, dependencies, conditions (Fase E) | a DAG needs a scheduler that owns ordering; parallel deliberately has none | `op:"workflow"` with the same task object; the per-task result shape is already the edge payload |
| Retries | a retry changes the meaning of "one task, one result"; policy belongs to the orchestrator | client-side today (re-send with a new id); later a task field |
| Resource locks | the executor cannot know that two commands both want `target/` | documented recommendation: separate worktrees/cwd, or serialize |
| **Cargo target-dir contention** | roadmap §8 forbids advertising fake parallelism: `cargo check`, `cargo test`, `cargo clippy` in one target dir serialize on the Cargo lock, so a "3× faster" claim would be a lie | each task gets its own `cwd`/env; docs must say to use `CARGO_TARGET_DIR` per task if overlap is wanted. Never a benchmark claim across these three |
| Filesystem sandbox | out of scope for the whole product (§11); interpreter isolation ≠ FS isolation | none is implied by the protocol: no field promises it, and the acceptance test *proves* the absence (D8) |
| Streaming task events | needs drainer discipline, out of MVP | §2.5 |
| Process isolation mode (`sharky -c` per task) | loses the context snapshot | request field `"isolation":"process"`, later |
| PTY per task | changes framing and echo | `"tty":true`, later |
| `sharky parallel --json --max N --cmd ...` CLI | it is a second entry point over the same core, not new semantics; building it before the protocol path exists would duplicate the renderer | the CLI is a thin client of the same `ParallelGroup` core in-process, printing the identical `type:"parallel"` object minus `schema_version`/`id`. Explicitly later than the agent op |
| Global concurrency governor across sessions | needs a daemon | one session per connection |

Threat model, repeated because parallel makes it easy to overclaim: the client
is trusted (it can already `kill` the agent); tasks cannot reach the protocol
channel; two tasks writing the same path race, and the executor neither detects
nor prevents it.

---

## 9. D8 — Tests (roadmap §8.4 mapped)

New file `rust/tests/agent_parallel.rs`, plus a `Parallel` helper in
`rust/tests/common/agent.rs` (send one frame, read one line, assert fields).
Deadlock discipline is the existing one: one write per read.

| # | §8.4 requirement | Concrete scenario |
| --- | --- | --- |
| 1 | 10 short tasks, distinct outputs, correlated by id | 10 tasks `printf 'out-<id>\n'` shuffled by id name (not alphabetically equal to request order); assert every `tasks[i].id` matches request order, each stdout is exactly its own marker, all `status:0` |
| 2 | real concurrency, limit never exceeded | 6 tasks, `max_concurrency:2`, each runs `printf 'S %s %s\n' <id> $(date +%s%N) >> log; sleep 0.2; printf 'E %s %s\n' <id> $(date +%s%N) >> log` in a shared cwd. Test reads `log`, builds intervals, runs a sweep: assert max overlap `== 2` exactly (never 3) and `>= 2` somewhere (proves overlap, not serialization). Second assertion: `started_tasks == 6` |
| 3 | one failure does not corrupt the others | 4 tasks: `echo a`, `false`, `printf 'b\n'`, `exit 3`; assert the three sane outputs are byte-exact, statuses 0/1/0/3, group `status:1` (first failed in request order) and `state:"completed"` |
| 4 | `wait-all` completes everything | `wait-all` where task 1 fails fast (`false`) and task 2 sleeps 300 ms; assert task 2 ran to completion with its output and `duration_ms >= 250` |
| 5 | `fail-fast` cancels under a defined policy | `fail-fast`, `max_concurrency:2`, task 0 `false`, tasks 1..5 `sleep 5`; assert task 0 failed, exactly one other task started and is `cancelled`/`143`, tasks 2..5 are `cancelled`/`130` with `reason:"fail_fast"`, and the whole response arrives well under 5 s |
| 6 | cancel leaves no orphans | 3 tasks `sh -c 'sleep 30 & sleep 30; wait'`; read each `pgid`, send `cancel` on the group, read the ack and the response, then assert `killpg(pgid, 0)` fails with `ESRCH` for all three within the grace + slack (the same probe pattern as the MVP session tests) |
| 7 | per-task cancel | group with two long tasks; `cancel` with `"task":"t2"`; assert t2 is `cancelled` while t1 keeps running and completes with its own output; `CANCEL_UNKNOWN_ID` for a bogus task id; `CANCEL_TOO_LATE` after completion |
| 8 | timeout per task and per group | task `sleep 5` with `timeout_ms:100` → `timeout`/`124` while a `sleep 0.5` sibling in the same group succeeds; separate case: group `timeout_ms:100` with 3 tasks → running become `timeout`, queued become `cancelled`/`130` with `reason:"group_timeout"` |
| 9 | binary and long logs do not explode memory or pollute the protocol | one task `printf '\x00\xff\xfe'` → stdout base64, decoded bytes equal; one task `seq 1 2000000` with `--max-capture-bytes` small → `stdout_truncated:true`, `stdout_bytes` the real size; one run with `max_parallel_output_bytes` small → `output_truncated:true`, later tasks empty-but-counted. Every line the client reads parses as JSON (no stray writes) |
| 10 | 100 short tasks | 100 × `true`-ish tasks at `max_concurrency:8`; all `status:0`; fd count of the agent before/after identical (`/proc/<pid>/fd`), no `<defunct>` children (`ps --ppid`), session still answers `exec "echo alive"` |
| 11 | REPL unaffected | full existing suite green (tripwires: `tests/job_control.rs`, `tests/hazards.rs`, `tests/interrupt.rs`, `tests/signal_lifecycle.rs`, `tests/trap.rs`); plus the `&`-in-task change (U6) keeps REPL `kill %1`/`jobs` byte-identical |
| 12 | parent state isolation | session sets `X=1`, `cd /tmp`; a task does `X=2; cd /; export Y=3`; after the group, `exec "echo $X $PWD ${Y-unset}"` → `1 /tmp unset`; a task that runs `exit 9` leaves the session alive and its own status is 9 |
| 13 | **no FS sandbox (explicit)** | two tasks append to the same file in a shared cwd (`printf 'A\n' >> f; sleep 0.2; printf 'a\n' >> f` vs the same with B/b); assert the file has exactly 4 lines and both markers, i.e. the executor did not isolate, serialize or lock anything. This is the test that prevents the doc and the implementation from drifting into a false promise |
| 14 | cwd/env per task | `tasks[].cwd` respected (`pwd`), `tasks[].env` visible to `$VAR` and to a child (`env`), and neither appears in the session's later `state`/`env_delta` |
| 15 | protocol edges | unknown `op` still `UNKNOWN_OP`; `max_concurrency:0` and `33` → `INVALID_REQUEST`; duplicate task id → `DUPLICATE_TASK_ID`; 129 tasks → `TOO_MANY_TASKS`; `cancel` with `task` on an `exec` target → `INVALID_REQUEST` |
| 16 | ready negotiation | `ready.protocol.ops` contains `parallel`; `features` contains `parallel`, `parallel_task_cancel`, `parallel_env_delta`; `limits` carries `max_parallel_tasks`/`max_parallel`/`max_parallel_output_bytes` |

Non-test artifact: a measurement script (not a gate) for `§12.4` — throughput at
1/2/4/8 independent tasks, peak RSS, fd count, zombies, and the wall clock of the
child processes themselves, with the workloads being genuinely independent (never
the Cargo triple).

---

## 10. D9 — Implementation order (narrow units, each with its tests)

| Unit | Content | Tests in the same change |
| --- | --- | --- |
| **U1** | `protocol.rs`: `decode_parallel` + `ParallelRequest`/`TaskRequest` + validation + new codes; `json.rs`: `ParallelResult`/`TaskResult` encoder, `Limits` additions, `ops`/`features`/`limits` in `ready` | pure, in `tests/agent_protocol.rs` (table 15 and 16 without spawning) |
| **U2** | `shell.rs`: `enter_task_child()` (session/isolate/`job_pgid`), plus the EXIT-trap snapshot accessor pair; unit tests through the existing `in_child` harness | trap-fired-once-per-task, `$PWD`/env restored-in-child, `exec` builtin still refused, no parent mutation |
| **U3** | `agent/parallel.rs` **core**: capture creation, spawn, status record read-back, wait loop with window, per-task result assembly, single response; `wait-all` only; `Session::handle_line` arm | table 1, 3, 4, 9, 10, 12, 13, 14 |
| **U4** | supervisor: `ParallelGroup`/`TaskSlot`, `check_deadlines` branch, `resolve_cancel` third path, `end_parallel`, `note_group` per task, cleanup coverage | table 6, 7, 8; plus session-level `close` during a group |
| **U5** | `fail-fast` policy, queued cancellation surface | table 5 |
| **U6** | `run_background` group guard (`job_pgid.is_none()`) so `&` started by the task shell itself stays in the task group | test 6 variant where the task command is `sleep 30 & wait` **without** `sh -c` (an `sh -c` job would be in the group anyway); REPL `tests/job_control.rs` unchanged |
| **U7** | `help agent`/`help parallel` text, `docs/` cross-links, measurement script, `features` truthfulness review | `tests/help.rs`; manual script run |

Ordering constraints: U1 before U3 (decoder first), U2 before U3 (child entry
point), U4 before U5 (fail-fast needs the stop path), U6 independent of U1–U5 but
**must** land before the "no orphans" claim is made in docs. Every unit keeps the
full suite green; nothing merges before the roadmap allows it.

U6 ordering detail: it changes shared fork code, so it is landed and verified on
its own before parallel is advertised as containing `&`-jobs.

---

## 11. Riskiest part and mitigation

**Riskiest: U3+U4 together — the parallel wait loop and the per-task supervisor
table.** It is the place where three invariants meet that today are guaranteed by
construction:

1. exactly one `waitpid` consumer (the driver must reap only its own pids, never
   `-1`, or it steals statuses from the job table — `supervisor.rs:722` is
   shutdown-only for exactly this reason);
2. exactly one in-flight request in the supervisor (`in_flight`, `current`,
   `timeout_at`, `pending` at `supervisor.rs:92-121`), which N tasks with N
   deadlines and N escalations break by definition;
3. exactly one protocol writer, whose ordering rule (acceptance order, `cancel`
   acks excepted) must survive acks emitted from inside the pump while a group
   runs.

Failure modes if this is done casually: a task's status consumed twice (one
consumer waits, another reaps), a stale pgid signalled after the pid was reaped
and the number reused, an escalation that fires on `current` while it holds a
different task's pgid, or an ack written after the group's own response.

Mitigations, in order of importance:

* `current` is forced to 0 while a group is in flight, so the single-request
  escalation path **cannot** fire at all in parallel mode; escalation only ever
  comes from `TaskSlot.pending`.
* per-pid `WNOHANG` waits, with the pid removed from the pending set before the
  record is inspected — a second wait for the same pid is impossible by
  construction, not by luck.
* `killpg` only while the slot is `Running`; a slot transitions to `Done` under
  the same lock that would signal it, so "signal a reaped pgid" is a state
  transition the table forbids.
* the 50 ms poll backstop plus the SIGCHLD self-pipe (`signal.rs:179-205`) means
  a missed wake costs latency, not correctness.
* tests 6, 7, 8 and 10 are the gate: they fail loudly on a double reap (statuses
  vanish), on a stale signal (`ESRCH` vs a live process), and on fd/zombie leaks.

**Second risk: U6** (shared fork code). The guard is one condition on
`job_pgid.is_none()`, which is provably false at every top level
(`shell.rs:387`, `6842`, `6922`, `7063`) and true only inside a forked child —
but it changes job-control grouping in agent subshells, so it needs its own
REPL/session regression pass and its own line in the doc.

**Third risk: the child's discipline.** A task child that forgets one step of
§3.2 either pumps the protocol (would break fd isolation) or leaves a capture fd
open (would keep another task's result file alive). Mitigation: the child's
preparation is one function with one test that asserts each property from inside
a real forked child (the existing `in_child` harness already proves the pattern).

---

## 12. Accepted limitations (stated so they are not discovered later)

1. **No filesystem isolation, no locking.** Two tasks on the same path race.
   Test 13 exists to keep this honest.
2. **`&` inside a task** joins the task group only after U6 (§3.4).
3. **Command substitution children** under `isolate_groups` get their own group
   and escape cancel — a pre-existing gap of the `exec` path, recorded as a
   defect with its own fix, not silently attributed to parallel.
4. **A task cannot mutate the session**; there is no merge/commit operation, by
   design (§8.2). A client that wants a task's effect on the environment must
   use `exec`.
5. **No streaming, no partial results**: a task's output exists only after the
   group's response.
6. **Group time is bounded only by timeouts**: a group with no `timeout_ms` and a
   hung task lives as long as `exec` would.
7. **A `parallel` request arriving while another request runs is queued**, so two
   groups never overlap in one connection.
8. **`max_concurrency` bounds tasks, not processes**: descendants can exceed it.
9. **Task `stdin` is a file, not a pipe**: no interactive stdin, no two-way
   streaming; `read` sees the payload and then EOF, as in `exec`.
10. **`sharky parallel` CLI and process-isolation mode are not implemented**;
    their seams are additive and named in §8.
