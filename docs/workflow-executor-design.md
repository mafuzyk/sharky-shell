# Workflow Executor — Architecture and Wire Contract

Status: **design proposal (no code)**. Target: roadmap Fase E
(`SHARKY_AGENT_RUNTIME_CODE_MODE_ROADMAP.md` §9, with §8, §10, §11, §12.3),
built directly on the delivered parallel executor (Fase D).
Repository: `sharky-fasea`, detached HEAD `995e904` (53 commits over 3.7.0).
Everything here stays local; nothing merges or pushes.

This document extends `docs/parallel-executor-design.md` and
`docs/agent-runtime-mvp-design.md`; it never contradicts them. The session
protocol stays as designed there (`ready`/`exec`/`state`/`cancel`/`close`,
additive `ops`/`features` negotiation, fd 900+ control channel, per-request
temp-file capture, supervisor with pgid cancel TERM→KILL, `Shell::session_eval`,
`enter_task_child`), and this document adds one op, one response type, and a
small number of additive vocabulary values. No `schema_version` bump.

Citations are `path:line` inside this worktree. Every claim about existing
behavior was read from the source.

---

## 0. Decision summary

| # | Question | Decision |
| --- | --- | --- |
| D1 | Wire contract | New `op:"workflow"` with `tasks[] {id, command, depends_on[], timeout_ms, retry{max_attempts,on,backoff_ms,backoff}, env, stdin, cwd, lock, condition}` plus group `max_concurrency`/`policy`/`failure_policy`/`timeout_ms`; one `type:"workflow"` response, `tasks[]` in **request order**, `order[]` in topological order, no streaming |
| D2 | DAG validation | Two-pass decode: ids first, then edges; `WORKFLOW_CYCLE`, `INVALID_DEPENDENCY`, `INVALID_CONDITION` added; empty graph and duplicates reuse the parallel codes; nothing runs when validation fails |
| D3 | Execution model | The parallel executor's own window loop, spawn and reap, driven by a Kahn-style in-degree scheduler; **one** interpreter, no second evaluator; `policy` and `failure_policy` are orthogonal |
| D4 | Retries | `max_attempts` default **1** (no retry unless asked); `on` defaults to `["timeout","spawn_error"]`, `"failure"` is opt-in; fixed or exponential backoff evaluated as a deadline in the loop, never a blocking sleep; one node never has two live process groups |
| D5 | Conditions | One `condition` per task over **one dependency's** result: `exit_code` (`eq`/`ne`/`in`), `stdout_contains`, `stdout_equals`, `stderr_contains`; AND of present fields; false ⇒ `skipped`; malformed ⇒ `INVALID_CONDITION`; no expressions, no JSON paths |
| D6 | Cancellation | `cancel` on the request id stops the run (running attempts TERM→KILL, everything else `blocked`/`130`); `cancel` with `task` stops one node and its dependents follow `failure_policy`; a cancel during backoff cancels the retry; the supervisor's existing escalation is the only mechanism |
| D7 | Resource locks | `lock:"<name>"` is a scheduler-level semaphore of one; acquired together with the window slot, held for the node (attempts and backoff included), released at node end; **no `flock`**, because a lock file adds a stale-lock failure class, would leak into descendants and across sessions, and would not make Cargo faster anyway |
| D8 | Observability | Per node `attempts`, `attempts_detail[]` (bounded by `max_attempts ≤ 8`, carries `started_ms` for a real timeline), `duration_ms`; superseded attempts keep sizes, not streams; one response budget shared with parallel, no logs on disk, no global trace array |
| D9 | Non-goals/seams | Code Mode JS/DSL stays client-side (Fase F); no sandbox, no persistent workflow daemon, no cross-session locks, no data edges between nodes, no `kind:"aggregate"` |
| D10 | Tests | Roadmap §9/§12.3 mapped to concrete scenarios: cycle, missing ref, each failure policy, retry+backoff timings, cascade cancel, lock serialization, 100-node graph, no orphans, output budget, and the same-file test that proves there is no FS sandbox |
| D11 | Implementation order | Six narrow units (U1 validation → U2 scheduler on shared attempt machinery → U3 policies/retries/conditions → U4 locks → U5 cancel + failure paths → U6 docs), each with its tests; first unit is U1 |

The one-line product promise: **a workflow is the parallel executor plus a
scheduler that knows which nodes may start — same isolated interpreter copies,
same captures, same supervisor, plus order, policy, retries and named locks.**

---

## 1. Baseline facts this design builds on

1. A group is currently one window loop: `parallel::run` (`parallel.rs:84-141`)
   spawns in request order while `running < window`, drains its own reaped
   children (`parallel.rs:339-367`), waits for one event
   (`parallel.rs:379-405`), and only sleeps when a drain produced nothing
   (`parallel.rs:121-133`). **A reap immediately re-enters the spawn window**,
   which is what makes dependency handoff cost no scheduling latency (§4.2).
2. The supervisor owns one `ParallelGroup` table with a `TaskSlot` per task in
   request order: `TaskPhase::{Queued, Running{pid, deadline}, Done}`, one
   `stop: Option<TaskStop>`, one `pending: Option<Pending>`
   (`supervisor.rs:137-165`). `begin_parallel` (`supervisor.rs:853`) installs it
   and forces `current = 0`, so the single-request escalation path cannot fire
   while a group exists.
3. The slot API is index-addressed and already the right shape for a scheduler:
   `task_running(index, pid)` (`supervisor.rs:887`) starts the slot's own
   deadline, `take_task_stop(index)` (`supervisor.rs:984`) returns the verdict
   of a slot that never started, `finish_task(index, pid)`
   (`supervisor.rs:1004`) ends a reaped slot and returns its verdict exactly
   once, `end_parallel()` (`supervisor.rs:1040`) closes the group.
4. `TaskStop` carries `outcome ∈ {Cancelled, Timeout}` plus `reason ∈
   {FailFast, GroupCancel, TaskCancel, GroupTimeout}` (`supervisor.rs:84-133`).
   A **task deadline** yields `Outcome::Timeout` with `reason: None`
   (`supervisor.rs:127-132`), and the **group** deadline arms *running* slots
   with the very same `timed_out()` (`supervisor.rs:594-596`), so the two are
   indistinguishable from the slot; the driver must consult
   `parallel_stopped()` before treating a timeout as retryable (§5.2).
5. `record_stop(StopReason)` (`supervisor.rs:929`) is the single stop path: it
   arms every `Queued` slot (`cancelled`/`130` for a group stop) and every
   `Running` slot (TERM, then the `Pending` escalation), first stop wins.
6. Escalation survives the leader: `finish_task` moves a pending `SIGKILL`
   deadline to a per-group `GroupStop` (`supervisor.rs:1019-1031`), and
   `check_group_stops` kills it when the group is still alive
   (`supervisor.rs:678`). This is what makes "no orphans" true for a node whose
   attempt was already reaped (§6).
7. A `cancel` for a group is resolved inside the pump lock:
   `resolve_cancel` → `stop_parallel_task` (`supervisor.rs:1276-1423`), which
   arms one slot or all slots and answers the ack in the same critical section.
   `targets_queued` (`supervisor.rs:1426`) matches `op` values literally
   (`exec`, `parallel`), so a new op must be added there or a queued cancel
   would answer `CANCEL_UNKNOWN_ID`.
8. One attempt is: parent-side captures (`parallel.rs:647-717`), a forked child
   that calls `setpgid`, `child_guard` (`supervisor.rs:446`),
   `enter_task_child`, `dup2`s its three descriptors, closes every other one,
   applies `cwd`/`env`, then `session_eval` + `finish_task` and one status
   record (`parallel.rs:735-857`). The parent then reaps and assembles the
   `TaskResult` (`parallel.rs:421-567`).
9. Capture discipline is fixed and reused unchanged: descriptors `>= 10`,
   unlinked, `O_CLOEXEC`, `read_back` reports real size plus `truncated`,
   invalid UTF-8 switches the whole stream to base64 (`capture.rs:29`,
   `capture.rs:201-226`), and one group budget bounds the whole response
   (`parallel.rs:573-588`, `Limits::max_parallel_output_bytes`).
10. `limits.max_parallel_tasks = 128`, `limits.max_parallel = 32`,
    `limits.max_parallel_output_bytes = 8 MiB`, `max_id_bytes = 256`
    (`json.rs:47-61`, `json.rs:1184-1191`). A workflow node is the same size of
    object as a parallel task, so it reuses these bounds instead of inventing
    new ones.
11. Capabilities are advertised per feature, not per build
    (`json.rs:71-78`, `json.rs:1112-1119`): `ops` and `features` list only what
    the driver honors, which is how a new op appears without a version bump.
12. `Shell` is not `Send` (`shell.rs:46`), there are no threads in
    `rust/src/agent/`, and the driver is the only `waitpid` consumer. Nothing
    here changes that.

---

## 2. D1 — Wire contract

### 2.1 Request

```json
{"schema_version":1,"id":"w1","op":"workflow",
 "policy":"wait-all","failure_policy":"skip","max_concurrency":4,
 "timeout_ms":600000,
 "tasks":[
   {"id":"status","command":"git status --short"},
   {"id":"diff","command":"git diff --stat"},
   {"id":"fmt","command":"cargo fmt --check","lock":"cargo-target"},
   {"id":"tests","command":"cargo test -q","lock":"cargo-target","timeout_ms":120000,
    "retry":{"max_attempts":2,"on":["timeout","spawn_error"],"backoff_ms":500}},
   {"id":"report","command":"scripts/report.sh","depends_on":["status","tests"],
    "condition":{"task":"status","exit_code":{"eq":0}}}
 ]}
```

| field | type | default | meaning |
| --- | --- | --- | --- |
| `id` | string | required | request id, same rules/duplicate detection as every op |
| `op` | `"workflow"` | required | |
| `policy` | `"wait-all"` \| `"fail-fast"` | `"wait-all"` | group reaction to a real failure (parallel's field, same values) |
| `failure_policy` | `"skip"` \| `"fail"` \| `"continue"` | `"skip"` | what happens to the **dependents** of a node that did not succeed (§4.3) |
| `max_concurrency` | integer | `4` | attempts allowed to run at once; `1..=limits.max_parallel`; effective window `min(value, len(tasks))` |
| `timeout_ms` | integer | none | **group** wall clock, counted from the first attempt spawn |
| `tasks[].id` | string | required | unique in the request, `1..=max_id_bytes` |
| `tasks[].command` | string | required | shell source, evaluated by `session_eval`; empty/whitespace = no-op status 0 |
| `tasks[].depends_on` | `[string]` | `[]` | ids of this request; each must exist, no duplicates, never itself |
| `tasks[].timeout_ms` | integer | none | **per attempt**, counted from that attempt's spawn |
| `tasks[].retry` | object | none | §5; absent means one attempt |
| `tasks[].retry.max_attempts` | integer | `1` | `1..=limits.max_workflow_attempts` (8); `1` = no retry |
| `tasks[].retry.on` | `[string]` | `["timeout","spawn_error"]` | `failure`, `timeout`, `spawn_error`; `[]` rejected |
| `tasks[].retry.backoff_ms` | integer | `0` | base delay, `0..=limits.max_workflow_backoff_ms` |
| `tasks[].retry.backoff` | `"fixed"` \| `"exponential"` | `"fixed"` | fixed: `backoff_ms`; exponential: `backoff_ms * 2^(n-1)`, capped |
| `tasks[].condition` | object | none | §6: one dependency's result must match |
| `tasks[].lock` | string | none | named lock, `1..=64` bytes, no control characters (§8) |
| `tasks[].cwd` | string | session cwd | applied **in the child** |
| `tasks[].env` | `{name: string\|null}` | none | applied **in the child** on top of the snapshot |
| `tasks[].stdin` / `stdin_encoding` | string / `utf8`\|`base64` | none | fresh payload file per attempt |
| `tasks[].env_delta` | bool | **`false`** | same reason as parallel (`parallel-executor-design.md` §2.1) |

`depends_on` is an **ordering and failure constraint, not data flow.** Node B
does not inherit A's `cwd`, `env`, variables or output; every node starts from
the session snapshot. A client that wants to feed A's output to B writes a file
in A and reads it in B, or uses `condition` to gate on A (§6). This is stated
here because "workflow" invites the opposite assumption, and the roadmap (§9.3)
explicitly forbids shipping `kind:"aggregate"` before its transformer semantics
exist.

### 2.2 Validation and protocol errors

Decoding lives next to `decode_parallel` (`protocol.rs:611`) and follows the
same rule: **the whole request is refused, nothing runs**, so a malformed
request cannot leave a partially spawned DAG behind.

| condition | code |
| --- | --- |
| `tasks` missing / not an array / empty | `INVALID_REQUEST` |
| more than `limits.max_parallel_tasks` tasks | `TOO_MANY_TASKS` |
| duplicate `tasks[].id`, empty or over-long id | `DUPLICATE_TASK_ID` / `INVALID_ID` |
| `depends_on` not an array of strings | `INVALID_DEPENDENCY` |
| `depends_on` repeats an id | `INVALID_DEPENDENCY` |
| `depends_on` names the node itself | `INVALID_DEPENDENCY` |
| `depends_on` names an id absent from the request | `INVALID_DEPENDENCY` (message names both ends) |
| the graph has a cycle | `WORKFLOW_CYCLE` (message names the cycle path) |
| `policy`/`failure_policy` outside their vocabularies | `INVALID_REQUEST` (never coerced) |
| `max_concurrency` `0` or above `limits.max_parallel` | `INVALID_REQUEST` naming the range |
| `retry.*` out of range, unknown `on` value, empty `on`, unknown `backoff` | `INVALID_REQUEST` |
| `lock` empty, over 64 bytes, or containing a control character | `INVALID_REQUEST` |
| `condition` malformed, or `condition.task` not in `depends_on` | `INVALID_CONDITION` |
| `condition.task` names an absent id | `INVALID_CONDITION` |
| `command` missing | `INVALID_REQUEST` |
| bad base64 / over `max_stdin_bytes` | `INVALID_ENCODING` / `STDIN_TOO_LARGE` |

Three codes are added to the stable taxonomy (`json.rs:1751`): `WORKFLOW_CYCLE`,
`INVALID_DEPENDENCY`, `INVALID_CONDITION`. Adding a code is forward compatible;
changing one is not (`agent-runtime-mvp-design.md` §3.5). All three are
non-fatal: the session keeps serving.

Validation order is fixed so only the first failure is reported and it is the
most structural one: group fields → per-task scalars in request order → id
uniqueness → `depends_on` references → cycles → conditions. Conditions come last
because a condition is validated against the edge set.

### 2.3 Response

One response per request, `type:"workflow"`. A distinct `type` (not
`type:"result"`) for the same reason `parallel` has one: the shape differs and a
client that does not know the op must never be handed a payload it will misread
(`parallel-executor-design.md` §2.3).

```json
{"schema_version":1,"id":"w1","type":"workflow","op":"workflow",
 "policy":"wait-all","failure_policy":"skip",
 "state":"completed","status":124,"duration_ms":240700,"duration_us":240700112,
 "max_concurrency":4,"started_tasks":4,"attempts_total":5,
 "skipped_tasks":1,"blocked_tasks":0,"output_truncated":false,
 "order":["status","diff","fmt","tests","report"],
 "tasks":[
  {"id":"status","depends_on":[],"state":"completed","termination":"exited","status":0,
   "signal":null,"reason":null,"error":null,"attempts":1,
   "attempts_detail":[{"n":1,"started_ms":0,"duration_ms":12,"status":0,"termination":"exited","stdout_bytes":7,"stderr_bytes":0}],
   "duration_ms":12,"duration_us":12073,"cwd":"/repo","pgid":4301,
   "stdout":" M a.rs\n","stdout_encoding":"utf8","stdout_bytes":7,"stdout_truncated":false,
   "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false},
  {"id":"tests","depends_on":[],"lock":"cargo-target",
   "state":"timeout","termination":"timeout","status":124,
   "signal":null,"reason":null,"error":null,"attempts":2,
   "attempts_detail":[
    {"n":1,"started_ms":60,"duration_ms":120004,"status":124,"termination":"timeout","stdout_bytes":0,"stderr_bytes":1822},
    {"n":2,"started_ms":120564,"duration_ms":120101,"status":124,"termination":"timeout","stdout_bytes":0,"stderr_bytes":1804}],
   "duration_ms":240105,"duration_us":240105331,"cwd":"/repo","pgid":4318,
   "stdout":"","stdout_encoding":"utf8","stdout_bytes":0,"stdout_truncated":false,
   "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false},
  {"id":"report","depends_on":["status","tests"],
   "condition":{"task":"status","exit_code":{"eq":0}},
   "state":"skipped","termination":"skipped",
   "status":125,"signal":null,"reason":"dependency_failed","error":null,
   "attempts":0,"attempts_detail":[],"duration_ms":0,"duration_us":0,"cwd":"/repo","pgid":null,
   "stdout":"","stdout_encoding":"utf8","stdout_bytes":0,"stdout_truncated":false,
   "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false}
 ]}
```

Reading the example: `fmt` and `tests` share `lock:"cargo-target"`, so they
never overlap (`fmt` ran first, `tests` starts at 60 ms); `tests` timed out
twice, 500 ms of backoff apart, and holds its lock across both attempts;
`report` never ran because one of its dependencies is not a success, so
`failure_policy:"skip"` applied before its `condition` was even consulted; the
aggregate is `124`, the first **failed** node in `order` (`tests`), not `125`
from the node that was left out.

The example elides two nodes (`diff`, `fmt`) for brevity: a real response
carries one entry per requested node, in request order, always.

Group fields:

| field | meaning |
| --- | --- |
| `state` | `completed` (the scheduler exhausted the graph without a group stop), `failed` (a node failed and `policy:"fail-fast"` or `failure_policy:"fail"` stopped the run), `cancelled` (a client cancel stopped it) |
| `status` | aggregate status, exact rule in §2.4 |
| `duration_ms` / `duration_us` | driver start to response assembly, monotonic |
| `max_concurrency` | echoed, after validation |
| `started_tasks` | nodes that spawned at least once |
| `attempts_total` | sum of `attempts` over all nodes |
| `skipped_tasks` / `blocked_tasks` | counts by state, so a consumer never has to recompute them |
| `output_truncated` | true when the response budget cut or dropped any stream (`parallel-executor-design.md` §6) |
| `order` | node ids in **topological order**, deterministic tie-break by request index; this is the order the aggregate rule reads |
| `tasks` | **request order**, always, one entry per requested node |

Per-node fields are the `TaskResult` set — `id`, `state`, `termination`,
`status`, `signal`, `reason`, `error`, `duration_ms`/`duration_us`, `cwd`,
`stdout*`, `stderr*`, `env_delta`, `pgid` — plus the workflow-only
`depends_on`, `lock`, `condition`, `attempts`, `attempts_detail`. `pgid` is the
last attempt's leader pid (the group a `cancel` would signal), `null` for a node
that never ran.

New vocabulary, additive and produced only by `op:"workflow"`:

| field | new values | meaning |
| --- | --- | --- |
| `state` | `skipped` | never ran; a dependency did not succeed under `failure_policy:"skip"`, or its own `condition` was false |
| `state` | `blocked` | never ran; the whole run stopped (fail-fast, `failure_policy:"fail"`, group cancel, group timeout) before the node became runnable |
| `termination` | `skipped` | pairs with `state:"skipped"`; `blocked` reuses `cancelled` for parallel parity |
| `reason` | `dependency_failed`, `condition_false`, `failure_policy` | plus the existing `fail_fast`, `group_cancel`, `task_cancel`, `group_timeout` |
| group `state` | `failed` | a real failure stopped the run |

Status conventions per outcome:

| node outcome | `state` | `termination` | `status` |
| --- | --- | --- | --- |
| exited | `completed` | `exited` | its exit code |
| signaled | `completed` | `signaled` | `128 + signal` |
| attempt deadline, retries exhausted | `timeout` | `timeout` | `124` |
| group deadline, was running | `timeout` | `timeout` | `124` |
| `cancel` on the node while running | `cancelled` | `cancelled` | `143` |
| `cancel` on the node before it ran | `cancelled` | `cancelled` | `130` |
| group cancel / fail-fast / group timeout, never ran | `blocked` | `cancelled` | `130` |
| dependency did not succeed, or `condition` false | `skipped` | `skipped` | `125` |
| could not spawn (all attempts) | `spawn_error` | `spawn_error` | `126` |
| result lost (defensive, `parallel-executor-design.md` §3.7) | `completed` | `exited` | non-zero + `error.code=INTERNAL` |

`125` is chosen for `skipped` on purpose: it is the "command not run" territory,
distinct from `124` (deadline), `126` (spawn failure), `130`/`143` (signals), and
no command produces it here, so a consumer can never confuse it with an exit
code of the node's own command.

### 2.4 Aggregate status rule (exact)

"Failure" and "left out" are separated so a consequence never outranks its
cause:

```
failed(n)   := n.state == "completed" && (n.termination != "exited" || n.status != 0)
            || n.state in {"timeout", "cancelled", "spawn_error"}
leftout(n)  := n.state in {"skipped", "blocked"}

if any failed(n):   status = status of the first failed node in `order`
else if any leftout(n): status = status of the first leftout node in `order`
else:               status = 0
```

Consequences, stated so nobody has to reverse-engineer them:

* In a topological order a dependency always precedes its dependents, so the
  "failed" pass finds a real failure before any of its `skipped`/`blocked`
  shadows. The second pass only runs when nothing failed at all, which is
  exactly the cancel and group-timeout cases: `130` when nothing had started,
  `124` when a running node hit the group deadline.
* A group cancel yields `state:"cancelled"` and the same rule, so the aggregate
  matches `parallel`'s.
* A retried node contributes its **final** attempt's status, never an earlier
  one; earlier attempts are visible in `attempts_detail`, so nothing is hidden.
* The rule never collapses to a boolean (roadmap §8.3), and a transforming
  layer cannot erase failure state (roadmap §10.4).

### 2.5 Single response, not streaming

No streaming in the MVP, for the same reason as parallel: a drainer discipline
and stable event ordering are exactly what the file-capture design avoids, and
partial results would let a node's output arrive before the run's own status,
complicating cancel/ack ordering (`parallel-executor-design.md` §2.5).

Seam, no version bump: unknown request keys are ignored, so a future
`"stream":true` today has no effect instead of becoming `INVALID_REQUEST`;
unknown response `type` values must be ignored by clients, which is what makes
additive `task_event` lines possible. Capability is negotiated through
`ready.protocol.features`; a client must not assume an unlisted field is
honored. The one exception to ignoring unknown request keys is `condition`:
its keys are a closed set and an unknown key is `INVALID_CONDITION`, because a
silently dropped predicate would change what the gate means.

`ready` gains, gated by a `WORKFLOW_AVAILABLE` constant in the style of
`json::PARALLEL_AVAILABLE` (`json.rs:71`):

* `protocol.ops` += `"workflow"`;
* `protocol.features` += `workflow`, `workflow_retry`, `workflow_condition`,
  `workflow_lock`, `workflow_task_cancel` (one flag per capability, so a
  partial delivery cannot advertise the whole op);
* `protocol.limits` += `max_workflow_attempts` (8),
  `max_workflow_backoff_ms` (60000). The task count, the concurrency cap and the
  output budget are **reused** from `max_parallel_tasks`, `max_parallel` and
  `max_parallel_output_bytes`: a workflow node is the same size of object as a
  parallel task, and two numbers that must stay equal are two numbers that will
  drift. The budget field keeps its `parallel` name; it is the per-group response
  budget and `workflow` charges the same one.

---

## 3. D2 — DAG validation

### 3.1 Two passes, one source of truth

`decode_workflow` collects ids first, then resolves edges:

1. decode every task with the parallel field rules (`protocol.rs:751-894`), plus
   the workflow-only fields, and reject duplicate ids at insertion — the same
   `DUPLICATE_TASK_ID` message shape as parallel so a client's error handling is
   identical;
2. build an `id -> index` map (plain `BTreeMap`/`Vec` scan; 128 entries do not
   justify a hash map, and determinism is worth more than the microseconds);
3. resolve each `depends_on` entry: three client bugs are checked here — a
   self-reference, a repeated edge and an absent id — and all three yield
   `INVALID_DEPENDENCY` (the message names `tasks[i].depends_on[j] = "x"` and
   says which it is: "a task cannot depend on itself", "the dependency is
   repeated", or "no task in this request has that id"). The decoder checks
   them in this order: self, repeated, absent; that only decides which message
   wins when one entry is several bugs at once, not a difference in error code.
   A repeated edge is semantically a no-op, but it is almost always a client
   bug, and this project's rule is that clamping or silently normalizing a
   request hides that bug — `protocol.rs:657-665`);
4. compute the topological order and detect cycles.

### 3.2 Cycle detection that names the cycle

Kahn's algorithm runs anyway to produce `order`, so it is the cheap primary
check: process nodes with in-degree zero, decrement successors. If
`order.len() != tasks.len()`, the remaining nodes are in or behind a cycle.
Kahn alone cannot name a path, and "there is a cycle somewhere" is a poor error
for an agent, so a second walk (iterative DFS over the unresolved set) finds one
cycle and reports it:

```
{"type":"error","id":"w1","error":{"code":"WORKFLOW_CYCLE",
 "message":"cycle: fmt -> tests -> clippy -> fmt","fatal":false}}
```

Both walks are O(V+E) and both are bounded by 128 nodes / 8128 edges, so the
extra pass costs nothing measurable. Ordering tie-break is **request index**:
two nodes that are both ready appear in the order the client wrote them, which
makes `order` a stable, explainable field rather than an artifact of iteration.

The resulting `order` is carried in the validated request
(`WorkflowRequest { order: Vec<usize> }`), so the scheduler never recomputes it
and the response echoes it as ids (§2.3). One computation, one consumer, no
drift.

### 3.3 What validation deliberately does not do

* No reachability analysis ("this node can never run"): a node whose dependency
  is skipped is handled at runtime by `failure_policy`, and a runtime skip is
  observable in the response; refusing the graph at validation would forbid the
  legitimate "gate the expensive half" shape.
* No duplicate-`lock` warning: two nodes sharing a lock is the feature.
* No per-lock capacity, no priority, no node weights (§10).
* No `depends_on` size cap beyond the task count: with 128 nodes a node can have
  at most 127 edges, and the total edge count is bounded by 8128.

---

## 4. D3 — Execution model

### 4.1 One interpreter, one spawn path

There is no workflow interpreter. `workflow.rs` is a **scheduler** that calls
the same three primitives `parallel.rs` already uses:

* `spawn_attempt(shell, task, null_fd, close_fds) -> Result<Attempt, String>`
  (the body of `parallel.rs:647-717`, extracted),
* `attempt.finish(status, stop) -> TaskResult` (the body of
  `parallel.rs:421-567`, extracted),
* `supervisor::{task_running, take_task_stop, finish_task, begin_group,
  record_stop, parallel_stopped, end_parallel}` unchanged except for the
  additions in §4.5.

The extraction is mechanical and lands **before** any workflow code, with
`tests/agent_parallel.rs` green before and after: parallel's behavior is the
regression gate for the refactor, not a workflow test.

Why the extraction instead of a second copy: two copies of the child
preparation sequence (setpgid → child_guard → enter_task_child → dup2s → close
fds → cwd/env → session_eval → status record → `_exit`) is exactly the
duplication roadmap §5 forbids, and the second copy is the one that would forget
a step (`parallel-executor-design.md` §11, third risk).

`Shell` is never borrowed while an attempt runs (the fork copies it), so a node
still cannot mutate the session, exactly as in parallel (§8.2 of the roadmap:
parallel does not modify the parent).

### 4.2 The scheduler loop

```
let mut ready_at: Vec<Option<Instant>> = ...;   // backoff per node
let mut held_lock: Vec<Option<&str>> = ...;     // lock per node
let mut attempts: Vec<u32> = ...;

loop {
    supervisor::service();                      // pump: acks, deadlines, cancels

    // 1. Classify every node whose dependencies are all final (once).
    //    skipped / blocked / ready / waiting-on-backoff, per §4.3 and §5.3.

    // 2. Spawn window: earliest ready node in `order` that can start now.
    //    `next_ready_node` walks `order` and skips a ready node whose lock is
    //    held, so a lock wait never costs a window slot and never spins.
    while running_attempts() < window {
        let Some(node) = next_ready_node(&held_lock) else { break };
        spawn_attempt_for(node);                 // window slot + lock, atomically
    }

    // 3. Reap. A reap frees a window slot, a lock, and may make nodes ready.
    if !drain_reaped()? {
        if running_attempts() == 0 && !any_node_pending() { break }
        supervisor::wait_poll();                // ≤ 50 ms, never a long sleep
    }
}
```

Three invariants the implementing worker must not lose:

1. **A reap re-enters the spawn window before any wait.** This is the existing
   structure (`parallel.rs:121-133`), and it is why dependency handoff costs no
   scheduling latency when a node becomes ready.
2. **`wait_poll()` is the only sleep.** A backoff deadline is data checked each
   tick, never a `std::thread::sleep` of the whole delay — that would freeze
   cancel acks, deadline escalation and the protocol pump for the duration.
3. **Never scan-spin.** If nothing is running and nothing can become ready
   without a clock (`backoff`, `timeout`), the loop waits; a workflow with no
   timeout and an exhausted graph terminates, it does not busy-wait.

`next_ready_node` scans `order` from the start on each iteration. With 128 nodes
that is 128 comparisons per spawn decision — under a microsecond, and it
guarantees the tie-break is request order, not a heap's incidental order. A
priority queue would be the optimization if the bound ever grew; it is not
justified at 128.

### 4.3 Ready, skipped and blocked

Once a node's dependencies are all final (each in `completed`, `timeout`,
`cancelled`, `spawn_error`, `skipped` or `blocked`):

```
if any dependency is not a success:
    skip     -> node = skipped(125, reason=dependency_failed)
    continue -> node may run (dependency treated as satisfied)
    fail     -> record_stop(StopReason::FailurePolicy) if the node has at least
                one dependent that has not finished; the node itself is
                `blocked`/130 with reason `failure_policy`
else if node has a condition and it evaluates false:
    node = skipped(125, reason=condition_false)
else:
    node = ready
```

"Success" is `state == completed && termination == exited && status == 0` —
the same predicate as `triggers_fail_fast` without its `reason.is_none()`
qualifier, which is a *supervision* concern and not a graph concern.

The three policies, with their exact consequences:

| `failure_policy` | dependents of a failed node | rest of the graph |
| --- | --- | --- |
| `skip` (default) | `skipped`/`125`, reason `dependency_failed`, transitively | keeps running; the aggregate is non-zero |
| `continue` | run anyway, as if the dependency had succeeded | keeps running |
| `fail` | `blocked`/`130`, reason `failure_policy` | stopping: running attempts are cancelled, everything unfinished is `blocked` |

`continue` is not a way to erase a failure: the failed node keeps its own status
and the aggregate is still non-zero. It exists for cleanup and reporting nodes
whose job is to run regardless (roadmap §9.1: "política para dependência que
falhou").

### 4.4 `policy` and `failure_policy` are orthogonal, and here is the difference

* `policy:"fail-fast"` reacts to **any** real failure, leaf or not: one bad
  `git status` stops the run. Implementation: after a node's final result, run
  `triggers_fail_fast(&result)` (`parallel.rs:1065`, reused verbatim) and call
  `record_stop(StopReason::FailFast)`.
* `failure_policy:"fail"` reacts only when the failure **denies a dependent**:
  a failing leaf in a `wait-all` workflow with `failure_policy:"fail"` still
  lets every other independent node finish, which is the useful behavior for
  "run all the checks, but do not build on a broken base". Implementation: the
  same stop path, different trigger condition (§4.3), so there is one mechanism
  with two callers, not two stop mechanisms.

Both end with the same supervisor machinery: `record_stop` arms queued slots and
escalates running ones (`supervisor.rs:929-934`, `supervisor.rs:630-660`), the
driver stops spawning, and every unfinished node is labelled once.

A failing node that also has `retry` is retried first; the policy only sees the
**final** attempt (§5.2). A node stopped by supervision (`reason.is_some()`)
never triggers either policy: a cancel is not a failure signal, which is the
existing `triggers_fail_fast` rule and stays true here.

### 4.5 Supervisor delta (small and gated)

The existing table already models "N slots, one verdict each", so the delta is
vocabulary and two functions, not a second table:

```
enum GroupKind { Parallel, Workflow }        // stored on the group
begin_parallel(id, specs, timeout)  ->  begin_group(id, GroupKind::Parallel, specs, timeout)
task_retry(index)                            // Workflow only: Done -> Queued
StopReason::FailurePolicy                    // new stop reason, same machinery
TaskReason::FailurePolicy                    // new reason string "failure_policy"
parallel_seen: HashSet<String>  ->  group_seen          // both ops
targets_queued: "exec" | "parallel"  ->  + "workflow"
```

`task_retry` is the only transition that ever moves a slot back to `Queued`. It
exists because a retry is a **new attempt of the same node**, and the slot is the
node's identity for `cancel` and deadlines. Its contract:

* refuses unless `kind == Workflow` and the slot is `Done`;
* clears `stop`, `pending` and the phase's pid, and returns the previous pid so
  the driver can enforce "the previous group is empty" before respawning (§5.4);
* keeps the slot's `timeout_ms`, so the next attempt gets its own deadline from
  its own spawn, exactly like the first (`supervisor.rs:900-908`).

Why not one supervisor slot per attempt: the attempt count is only known after
the previous one fails, and a client `cancel` addresses a *node*, so slots must
be per node or `cancel` would have to translate "which attempt" — an identity
the client never sees. Per-node slots keep `CANCEL_TOO_LATE` /
`CANCEL_UNKNOWN_ID` semantics identical to parallel.

`current` stays `0` for a workflow group, exactly as `begin_parallel` forces it
(`supervisor.rs:859`), so the single-request escalation path cannot fire.

### 4.6 Concurrency, and what the number means

* `max_concurrency` bounds **running attempts**, not nodes and not processes.
  A node waiting for a lock, waiting for a dependency, or sleeping in backoff
  holds no slot.
* A lock is acquired **together with** the window slot, never after the spawn.
  A node that took a slot and then blocked on a lock would let three
  `cargo-target` nodes occupy the whole window while one cargo runs — the exact
  fake-parallelism the roadmap forbids (§8.1).
* The window is computed once as `min(max_concurrency, tasks.len())` and echoed.
* `max_concurrency` bounds tasks, not descendants: a node that forks twenty
  children still counts as one (parallel's documented rule,
  `parallel-executor-design.md` §4).
* The group deadline starts at the first attempt spawn
  (`supervisor.rs:897-899`, unchanged), so time spent validating and scheduling
  is not charged to the workflow.

---

## 5. D4 — Retries

### 5.1 The default is no retry

`max_attempts` defaults to `1`. A body without `retry` performs exactly one
attempt and cannot be retried by the executor. This is roadmap §9.1 read
literally — "nunca repetir automaticamente comandos mutáveis sem política ou
opt-in" — and it is the only defensible default: the executor cannot know
whether `git commit` is idempotent, and a silent second attempt of a mutating
command is a data-integrity bug, which this project ranks above convenience
(roadmap §16.1).

When `retry` **is** present, `on` defaults to `["timeout","spawn_error"]` — the
two outcomes that say "the attempt did not get a fair chance". Retrying a plain
non-zero exit (a deterministic failing test, a compiler error) is opt-in through
`"on":["failure", ...]`, because that is the case where a half-applied side
effect would be repeated.

The doc and `sharky help workflow` carry the warning in one sentence: **a retry
re-runs the command; if it is not idempotent, do not enable `failure` and prefer
`timeout`/`spawn_error` or no retry at all.** No amount of executor machinery
can make a mutating command safe to repeat, and the roadmap forbids pretending
otherwise.

### 5.2 Outcome classes and which ones retry

| final attempt outcome | class | retried when `on` contains |
| --- | --- | --- |
| `completed` + `exited` + non-zero | `failure` | `"failure"` |
| `completed` + `signaled` (not by us) | `failure` | `"failure"` |
| `timeout` (own deadline) | `timeout` | `"timeout"` |
| `spawn_error` | `spawn_error` | `"spawn_error"` |
| `cancelled` (client, group stop, fail-fast) | — | never |
| `skipped` / `blocked` | — | never (nothing ran) |

`on` is a set of outcome classes, so a repeated entry is accepted as written
and not deduplicated: listing `"timeout"` twice is the same predicate as once.

A safety valve beyond the policy: a retry requires `stop.is_none()` **and**
`parallel_stopped().is_none()`. The first covers a task cancel; the second
covers the group deadline, which is otherwise indistinguishable from a node
deadline because the group deadline arms running slots with the same
`timed_out()` verdict (`supervisor.rs:594-596`). Retrying into a group that has
already stopped would mint attempts nobody can wait for.

### 5.3 Backoff without blocking the pump

`backoff_ms` is a deadline, not a sleep:

```
on retryable outcome:
    attempts += 1
    if attempts == max_attempts: node is final (its outcome stands)
    else: ready_at[node] = now + delay(attempts)     // scheduler checks each tick
          node phase = Queued in the supervisor, lock still held
```

`delay(n)` is `backoff_ms` for `backoff:"fixed"`, `backoff_ms * 2^(n-1)` capped
at `limits.max_workflow_backoff_ms` for `"exponential"`. The cap is what keeps
"retry a hung build" from turning into "retry a hung build with a 17-minute
sleep".

Consequences, stated so they are not discovered later:

* Backoff precision is the supervisor's 50 ms poll backstop
  (`supervisor.rs:68`, `supervisor.rs:485-527`). A 500 ms backoff may retry at
  500–550 ms. That is the price of never blocking the pump, and it is the same
  price the deadline escalation already pays.
* A `cancel` during backoff is a `cancel` of a `Queued` slot: it arms
  `TaskCancel`, the driver takes the stop (`take_task_stop`) instead of
  respawning, and the node ends `cancelled`/`130` with `attempts` telling the
  truth. A retry cannot "come back" after a cancel.
* A cancel or deadline cannot fire between "the driver decided to retry" and
  "the slot went back to `Queued`": `task_retry` and `take_task_stop` run under
  the same `Mutex`, and the driver re-checks `take_task_stop` immediately before
  every spawn, exactly as the window loop does today (`parallel.rs:112-116`).

### 5.4 One live attempt group per node

The invariant: **a node never has two live process groups at once.**

A timed-out or cancelled attempt is reaped, but its group may still hold
descendants — `finish_task` deliberately moves the pending `SIGKILL` to a
per-group deadline rather than dropping it (`supervisor.rs:1019-1031`). Starting
attempt 2 while attempt 1's grandchildren still write to the same files would
recreate, inside one node, the race the whole design refuses to hide.

So before respawning, the driver checks `killpg(previous_pgid, 0) == ESRCH`
(the existing `group_alive` helper, `supervisor.rs:787`). While it is alive, the
node stays in backoff and the loop keeps ticking; the escalation deadline is at
most one grace (2 s) away. This can add up to `grace` to a retry of a timeout,
which is bounded, visible in `attempts_detail.started_ms`, and buys the only
honest version of "the retry started from a clean slate". A `spawn_error`
whose fork never happened never had a pid, so the check is trivially satisfied;
a child-side `spawn_error` was reaped normally and is gated like any other
attempt.

---

## 6. D5 — Conditions (minimal, honest)

### 6.1 Shape

One condition per node, over **one** dependency's result:

```json
{"condition":{"task":"status","exit_code":{"eq":0}}}
{"condition":{"task":"diff","stdout_contains":"src/"}}
{"condition":{"task":"probe","stdout_equals":"yes\n","stderr_contains":"warning"}}
{"condition":{"task":"probe","exit_code":{"in":[0,2]}}}
```

| key | meaning |
| --- | --- |
| `task` | required; must appear in this node's `depends_on` (an absent reference is `INVALID_CONDITION`) |
| `exit_code` | object with exactly one of `eq`(int), `ne`(int), `in`(array of int, `1..=8` entries) |
| `stdout_contains` | substring of the dependency's captured stdout |
| `stdout_equals` | exact string equality with the captured stdout |
| `stderr_contains` | substring of the dependency's captured stderr |

All present keys must hold (**AND**). Empty condition object (`{}`) is
`INVALID_CONDITION`: a condition that asserts nothing is a client bug, and
treating it as "true" would hide it. `exit_code.in` is a set of codes, so a
repeated code is accepted and not deduplicated — repeating it does not change
the predicate.

Evaluation happens **in the scheduler, in the parent**, on the `TaskResult` the
executor already assembled. No extra process, no shell evaluation, no `test`
invocation — the dependency already ran; its status and captured bytes are
facts.

### 6.2 Semantics that must be documented

* A false condition ⇒ `skipped`, `termination:"skipped"`, `status:125`,
  `reason:"condition_false"`, and the node's dependents follow
  `failure_policy`, exactly as if the node had failed. A chain of conditions
  therefore short-circuits instead of running consumers on missing input. The
  alternative ("a skipped node is transparent, dependents run") was rejected:
  it would run a consumer with no data and report a failure that belongs to the
  gate.
* The node echoes `condition` and, when evaluated, its outcome
  (`condition.outcome:"true"|"false"`), so the response explains a skip without
  the client re-deriving it.
* Matching is over the **captured** bytes, interpreted as UTF-8. Two honest
  consequences, both documented in `help workflow`: a stream that was cut by
  `max_capture_bytes`/the group budget may miss a match that exists in the full
  output; and a stream that is not valid UTF-8 (base64 on the wire) never
  matches `stdout_contains`/`stdout_equals` — the condition is false, not an
  error, because the command ran fine and only its encoding is unusual.
* The final attempt's streams are what conditions see. A condition cannot
  inspect an earlier attempt.
* `exit_code` for a signed dependency uses the protocol convention
  (`128 + signal`), so `{"eq":0}` is the only way to say "succeeded" without
  knowing about signals.

### 6.3 Explicitly out of scope

No arbitrary expressions (no `&&`, no `!`, no arithmetic), no JSON-path or
`jq`-like extraction from stdout, no file/directory predicates ("presence of
file" from roadmap §9.1 is listed as a *possible* condition source; it is
deferred because it needs a filesystem story — which cwd, which root, which
timeout — that the executor does not have), no cross-node expressions, no
retry-on-condition. A DSL arrives, if ever, with Fase F on the client side
(§10), not inside the shell's protocol.

The MVP covers what agents actually do with a gate: "run the tests only if the
tree is clean", "run the report only if the scan produced output", "treat exit 2
as success for a linter".

---

## 7. D6 — Cancellation

### 7.1 Whole workflow

`{"op":"cancel","target":"w1"}` — resolved by the existing path
(`supervisor.rs:1283-1295`) once `in_flight == "w1"` and the group is a
workflow: `stop_group_task(None)` arms every `Queued` slot and escalates every
`Running` one (`supervisor.rs:1375-1394`). What the client observes:

* running attempts: `cancelled`/`143`, reason `group_cancel`, after TERM→grace→
  KILL on their own pgids;
* ready-but-unspawned and dependency-waiting nodes: `blocked`/`130`, reason
  `group_cancel`;
* nodes in backoff: `blocked`/`130`, reason `group_cancel`, with `attempts`
  preserved;
* group `state:"cancelled"`; the aggregate follows §2.4 — `143` when a node was
  running, `130` when none had started;
* the ack is written immediately, out of order, exactly as an `exec` cancel is.

A node that was `skipped` before the cancel keeps `skipped`: the first verdict
wins everywhere, which is what `record_stop`'s "first stop wins" already
guarantees (`supervisor.rs:929-934`).

### 7.2 One node

`{"op":"cancel","target":"w1","task":"tests"}` — the existing third resolution
path (`supervisor.rs:1362-1423`) already does exactly this against a slot by id.
For a workflow it means:

* running: `cancelled`/`143`, reason `task_cancel`, its group TERM→KILL;
* queued or in backoff: `cancelled`/`130`, reason `task_cancel`, and **no
  further attempt** (§5.2: a stop is not retryable);
* unknown id ⇒ `CANCEL_UNKNOWN_ID`; already final ⇒ `CANCEL_TOO_LATE`; same
  codes, same messages, no new vocabulary.

### 7.3 Cascade, and why it is mostly policy rather than code

A cancelled node is not a success, so its dependents are handled by
`failure_policy` — `skipped` under `skip` (the default), `blocked` under `fail`,
runnable under `continue`. That is the cascade, and it needs no new mechanism:
the graph rule of §4.3 already covers "a dependency did not succeed".

Deliberately **not** implemented: "cancel the whole subtree". A dependent is not
cancelled just because an ancestor was; it is *left out*, and the response says
so with `dependency_failed` rather than pretending the client asked to cancel
it. Cancelling the whole run is one keystroke away (`target` without `task`).

### 7.4 Cleanup guarantees are parallel's, unchanged

Every attempt is spawned into its own process group and recorded through
`task_running` → `add_group` (`supervisor.rs:887-910`), and a group the registry cannot record is refused *before* the fork, exactly as parallel
refuses it (`parallel.rs:261-264`). Therefore:

* a `close`, transport EOF, fatal signal or `PROTOCOL_FD_LOST` during a
  workflow runs the ordinary `cleanup` (`supervisor.rs:1492`): TERM every
  recorded group, wait, KILL what is left;
* `finish_task`'s move of a pending KILL to `GroupStop` keeps the escalation
  after a leader is reaped, so a `SIGTERM`-ignoring grandchild still dies;
* a workflow cancelled before it started (queued behind another request) never
  spawns: `take_queued_cancelled` is read before `begin_group` and every node is
  answered `blocked`/`130` reason `group_cancel` — the `parallel::cancelled`
  shape (`parallel.rs:146-160`), with `started_tasks:0`.

Known escapes, inherited and not made worse here: a command-substitution child
resets `job_pgid` and leaves its own group (pre-existing `exec` gap,
`parallel-executor-design.md` §3.4), a background `&` job started by a task
shell stays in the task group only because of the U6 guard already landed, and a
child that calls `setpgid` itself escapes. The roadmap (§7.2) refuses to promise
against the last one, and so does this document.

---

## 8. D7 — Resource locks

### 8.1 Contract

`"lock":"cargo-target"` on a node means: at most one node of this workflow holds
the name at a time. A node acquires its lock **atomically with its window slot**
and holds it until the node is final — all attempts and every backoff included.
It is a scheduler-level counter, not a kernel object, not a file, and not
visible outside the request. Three consequences:

* **No head-of-line blocking becomes deadlock.** A node never holds one lock
  while waiting for another (one `lock` per node, acquired once), so the
  classic hold-and-wait cycle cannot exist. A lock waits for a holder that is
  bounded by its own `timeout_ms` and by the group deadline.
* **Holding across backoff is deliberate.** Releasing between attempts would let
  another `cargo` start, thrash the same target dir and change what the retry
  measures. The cost is that a long backoff under a lock delays that lock's
  other nodes; the benefit is that a retry observes the same toolchain state.
  Trade-off taken, documented, and visible in `attempts_detail.started_ms`.
* **Deterministic choice.** When several nodes want the same free lock, the
  earliest in `order` wins, because the spawn window scans `order` (§4.2). No
  starvation beyond the bound of one node's duration.

### 8.2 Why not `flock`

| | scheduler semaphore | `flock` on a lock file |
| --- | --- | --- |
| scope | one workflow request | every process that opens the same lock path with `flock`, including other sessions and other users |
| stale state | none: the counter dies with the request | a `SIGKILL`ed holder leaves a file whose meaning must be re-derived; `flock` releases on close, but a *lock file* plus a pid or timestamp is the usual next step and it is a new failure class |
| inheritance | nothing to inherit | the fd would be opened before `fork`/`exec`, so a task's grandchildren inherit the lock (an exec child stops at `O_CLOEXEC`, a shell child does not) — a grandchild holding the lock outlives the attempt and deadlocks the next one |
| cleanup | the driver drops it in the same thread that reaps | needs a path policy (`XDG_RUNTIME_DIR`? `/tmp`? per repo?), permissions, and removal on every error path |
| what it buys | "our executor does not spend two window slots on work that Cargo will serialize anyway" | the same, plus a promise we cannot test (cross-session) |
| claim risk | honest: works inside one request only | invites "locks work across sessions", which needs a daemon (roadmap §14 G) |

The kernel shortcut also cannot express the one thing the scheduler needs most:
**acquire before spawn**. A `flock` is taken by the child, after the fork, so
the window slot is consumed while the node blocks on a lock — the exact fake
parallelism §8.1 exists to prevent.

And the honest claim about Cargo: `lock:"cargo-target"` does **not** make Cargo
faster, and it does not remove Cargo's own lock on `target/`. It stops *this
executor* from pretending that `cargo check`, `cargo test` and `cargo clippy` in
one target dir run in parallel when they do not (roadmap §8.1 forbids that
benchmark). A client that wants real overlap gives each node its own
`CARGO_TARGET_DIR` through `env` and omits the lock — two supported answers, one
documented sentence each.

### 8.3 Not a filesystem lock

A named lock is a scheduling hint between nodes of one request. It does not
prevent two *independent* processes (another agent, a human `cargo build`, a
node without the lock) from touching the same paths, and it changes nothing
about isolation. The same-file conflict test (§11, row 13) exists to keep this
paragraph true.

---

## 9. D8 — Observability

### 9.1 Per node

| field | content | bound |
| --- | --- | --- |
| `attempts` | attempts started, including a failed fork | `0..=max_workflow_attempts` |
| `attempts_detail[]` | `{n, started_ms, duration_ms, status, termination, stdout_bytes, stderr_bytes}` per attempt | one entry per attempt, ≤ 8 |
| `duration_ms`/`duration_us` | **sum of attempt spans**, backoff and queue time excluded | bounded by the group deadline |
| `stdout*`/`stderr*` | the **last** attempt's streams, same truncation/binary rules | `max_capture_bytes` / group budget |
| `pgid` | the last attempt's leader | one integer |

`started_ms` is the offset from the workflow's first spawn, so
`(started_ms, duration_ms)` per attempt **is** a timeline: a client can render
"node X ran 0–12 ms, failed, retried at 512 ms, ran 512–512441 ms" from data it
already has. No global `trace` array is added: it would duplicate per-node data,
grow with graph size, and be the first field someone asks to stream.

### 9.2 Superseded attempts keep sizes, not streams

An attempt that was retried reports its byte counts but not its captured text.
Rationale, in order:

1. one response budget (8 MiB) serves every node and every attempt. Eight
   attempts × 128 nodes of retained logs is exactly the unbounded-log shape
   roadmap §11 forbids, and a flaky test's 6 MB of retry output would starve the
   other 127 nodes' streams;
2. the earlier attempt's *size* is enough for a consumer to know it produced
   diagnostics and to re-run the node with `retry` disabled if it needs them;
3. one stream shape per node keeps the response readable by a client that
   already knows `parallel`.

Seam: a future `"retain_attempts":true` request field (unknown keys are ignored
today, so it is additive) would keep superseded streams and charge them to the
budget first.

### 9.3 Bounds, stated as numbers

Worst case at the default limits: 128 nodes, 8 attempts each → at most 1024
`attempts_detail` entries, each roughly 100 bytes of JSON → ~100 KB of metadata,
plus the stream budget (8 MiB raw, up to 4/3 of that rendered for base64
payloads, the same documented overrun as parallel). Neither number is
proportional to anything a command prints, and nothing is written to disk: the
session keeps no log of commands or output (`agent-runtime-mvp-design.md` §7).

---

## 10. D9 — Non-goals and their seams

| Not in scope now | Why | Seam |
| --- | --- | --- |
| Code Mode (JS/DSL, `Promise.all`, transforms) — Fase F | it is a *client* of this protocol, not a language inside the shell; roadmap §10.1 explicitly does not require JavaScript | a workflow response is already a typed array of results; a client-side program `reduce`s it. `select`/`tail`/`grep` projections are roadmap §10.3, future additive fields |
| `kind:"aggregate"` / data edges / `stdin_from` (roadmap §9.3) | needs transformer semantics, a sandboxing story for expressions, and a budget for derived payloads | additive per-task fields; the response's `env_delta`/streams are already the payload |
| Filesystem sandbox, mounts, namespaces, cgroups (roadmap §11) | out of scope for the product; interpreter-context isolation ≠ FS isolation | none is implied: no field promises it, and §11 row 13 *proves* its absence |
| Persistent workflow daemon, cross-session locks, workflow resume after a crash | needs a daemon and durable state; that is Fase G material (roadmap §14), out of scope here | the request/response shape is already stateless; a daemon would replay a request into the same op |
| Streaming node events | needs drainer discipline and stable event ordering (`parallel-executor-design.md` §2.5) | unknown `type`s are ignored; `task_event` lines become additive |
| Per-lock capacity > 1, lock priorities, node weights | no measured need; every extra knob is a contract | `lock` may become an object (`{"name":…,"slots":N}`) additively |
| `sharky workflow` CLI | a second entry point over the same core; the protocol path must exist first (`parallel-executor-design.md` §8) | thin client of the same `run` function in-process |
| Cross-attempt output retention, attempt-level cancellation | bounded-budget reasons above (§9.2) | `retain_attempts` field |
| `continue-on-error` per node (a `optional:true` flag) | `failure_policy:"continue"` plus conditions already express it | additive task field |

Threat model, repeated because a DAG invites overclaiming: the client is trusted
(it can already `kill` the agent); nodes cannot reach the protocol channel
(child fds are files, control fds are closed by `child_guard`); two nodes writing
the same path race unless *the client* adds a dependency or a lock between them,
and the executor neither detects nor prevents it. Node output is data, never
instructions (roadmap §11).

---

## 11. D10 — Tests (roadmap §9 and §12.3 mapped)

New file `rust/tests/agent_workflow.rs`, using the existing `Session` helper
(`tests/common/agent.rs`: `spawn`, `send`, `recv`, `recv_within`,
`close_stdin`). Deadlock discipline is the established one: one write per read.
Pure decode tests go in `tests/agent_protocol.rs`.

| # | requirement | concrete scenario |
| --- | --- | --- |
| 1 | cycle (§9.1) | 3-node cycle plus one independent node; assert `WORKFLOW_CYCLE`, the message contains a real path (`a -> b -> c -> a`), nothing ran (no capture files, no `started_tasks`), session still answers `exec "echo alive"` |
| 2 | missing ref / self / duplicate dep | three separate requests; each `INVALID_DEPENDENCY`, message naming the offending `tasks[i].depends_on[j]`; nothing runs |
| 3 | empty graph, duplicates, oversize (§12.3) | `tasks:[]` → `INVALID_REQUEST`; duplicate id → `DUPLICATE_TASK_ID`; 129 nodes → `TOO_MANY_TASKS`; `depends_on:"a"` (string) → `INVALID_DEPENDENCY` |
| 4 | failure propagation, `skip` | diamond `a → b,c → d`, `b` fails; `skip`: `d` is `skipped`/`125`/`dependency_failed`, `c` completes, aggregate is `b`'s status, group `completed` |
| 5 | failure propagation, `continue` | same graph; `d` runs and completes; `b` still carries its non-zero status; aggregate non-zero (a `continue` run never erases the failure) |
| 6 | failure propagation, `fail` | same graph; `d` is `blocked`/`130`/`failure_policy`, `c` is cancelled if it was running, group `failed` |
| 7 | `fail-fast` interaction | a **leaf** fails in a wide graph: `policy:"fail-fast"` stops everything (blocked/130, reason `fail_fast`), while `wait-all` + `failure_policy:"fail"` lets the other leaves finish — the §4.4 difference in one test |
| 8 | retry with backoff | `retry:{max_attempts:3,on:["failure"],backoff_ms:200}`, command fails twice then succeeds (state file in `cwd`); assert `attempts:3`, `status:0`, `attempts_detail[*].started_ms` differences ≥ 180 ms, and the **group** wall clock ≥ 400 ms while the sum of attempt spans is small (proves the scheduler waited without blocking) |
| 9 | retry defaults | no `retry` and a failing command → `attempts:1`; `retry` without `on` and a plain exit 1 → `attempts:1` (default `on` excludes `failure`); `on:["timeout"]` with a command that exits 1 → `attempts:1`; a `timeout` outcome with `on:["timeout"]` → `attempts:2` |
| 10 | retry bounds and cancel during backoff | `retry.backoff_ms:5000`, cancel the node after the first attempt; assert the node is `cancelled`/`130`/`task_cancel`, `attempts:1`, the response arrives well under 5 s (the backoff was a deadline, not a sleep), and no second process ever existed |
| 11 | cascade cancel | 2 chains of 3; cancel the whole workflow mid-run: running nodes `cancelled`/`143`, everything else `blocked`/`130`/`group_cancel`, group `cancelled`; then `killpg(pgid,0)` → `ESRCH` for every reported pgid within grace + slack |
| 12 | no orphans (§12.3) | node `sh -c 'trap "" TERM; sleep 30'` plus one `sh -c 'sleep 30 & wait'`; cancel; both groups gone within grace + 1 s (the existing escalation proves TERM→KILL) |
| 13 | **no FS sandbox, explicit** | two independent nodes (`no depends_on`, no lock) append `A`/`B` markers to the same file through a shared `cwd`; assert the file holds exactly the 4 lines with both markers — the executor did not isolate, serialize or lock anything. Variant: the same two nodes with `lock:"x"` still both write (a lock serializes the *scheduler*, it does not virtualize the filesystem) |
| 14 | resource lock serialization | 3 nodes with `lock:"L"` and 3 without, `max_concurrency:4`, each writing start/end timestamps to a shared log; assert the three locked intervals **do not overlap** and the unlocked ones do (overlap ≥ 2), and `started_tasks:6`. Second assertion: a **lock wait costs no window slot** — `max_concurrency:2`, `A` holds `L` for 300 ms, `B` wants `L`, `C`/`D` are unlocked; assert `C` and `D` both complete while `A` is still running (a lock-waiting node consuming a slot would starve them) and `B` starts only after `A` is final |
| 15 | lock held across retry | locked node fails once then succeeds with `backoff_ms:300`; a second node on the same lock must start only after the first is final (proves the hold spans backoff) |
| 16 | conditions | `exit_code eq/ne/in`, `stdout_contains`, `stdout_equals`, `stderr_contains`; a false condition → `skipped`/`125`/`condition_false` and its dependent follows the policy; a malformed condition (`{}`, unknown key type, `task` not in `depends_on`, absent id) → `INVALID_CONDITION` |
| 17 | condition over binary/truncated output | dependency prints `\x00\xff`; `stdout_contains` is false (documented), not an error; dependency truncated by a small `--max-capture-bytes`; a condition matching text past the cut is false |
| 18 | 100-node graph (§12.3) | 100 nodes: 4 chains of 25 (depth 25) + one fan-in report; each `true`-ish; `max_concurrency:8`; assert 100 results in `tasks` (request order), `order` a valid topological order, all `status:0`, wall clock < 3 s, session `fd` count identical before/after (`/proc/<pid>/fd`), no `<defunct>` children, session still answers |
| 19 | output budget (§12.3) | 3 nodes printing 2 MiB each with `--max-parallel-output-bytes` small; `output_truncated:true`, `stdout_bytes` real, later nodes empty-but-counted, every line the client reads parses as JSON |
| 20 | parent state isolation | session `X=1; cd /tmp`; nodes do `X=2; cd /; export Y=3`; after the workflow `exec "echo $X $PWD ${Y-unset}"` → `1 /tmp unset`; a node running `exit 9` leaves the session alive with node status 9 |
| 21 | protocol edges | `ready.protocol.ops` contains `workflow`; features/limits present; `cancel` with `task` on an `exec` target still `INVALID_REQUEST`; `cancel` on an unknown workflow task → `CANCEL_UNKNOWN_ID`; unknown request keys ignored; a queued workflow cancelled before it starts answers every node `blocked`/`130`, `started_tasks:0`. The `ready.protocol.ops`/`features`/limits negotiation assertions of this row land with U2, not U1: `WORKFLOW_AVAILABLE` stays `false` until the executor ships. |
| 22 | single response | the entire workflow is one line; nothing is written to the protocol channel by a node (`echo` inside a node lands only in its `stdout`) |
| 23 | regression | the full suite, with `tests/agent_parallel.rs`, `tests/agent_supervisor.rs`, `tests/job_control.rs`, `tests/signal_lifecycle.rs` and `tests/hazards.rs` as the tripwires for the §4.5 supervisor delta and the §4.1 extraction |

Non-test artifact: a measurement script (not a gate) for roadmap §12.4 —
selector overhead (`group duration − Σ node durations`) at 1/8/32 concurrency,
peak RSS, fd count, zombies, and a `cargo fmt/test/clippy` run **with**
`lock:"cargo-target"` and **with** per-node `CARGO_TARGET_DIR`, reported as two
separate measurements, never as one "3×" claim.

---

## 12. D11 — Implementation order (narrow units, each with its tests)

| Unit | Content | Tests in the same change |
| --- | --- | --- |
| **U1** | `protocol.rs`: `DecodedRequest::Workflow`, `decode_workflow`, `decode_workflow_task`, two-pass DAG validation, Kahn + cycle path, condition validation; `json.rs`: `WorkflowResult`/`WorkflowTaskResult`, `FailurePolicy`, `TaskState::{Skipped,Blocked}`, `Termination::Skipped`, `TaskReason::{DependencyFailed,ConditionFalse,FailurePolicy}`, `GroupState::Failed`, new `ErrorCode`s, `Limits` additions, `ready` ops/features/limits, `WORKFLOW_AVAILABLE`. `WORKFLOW_AVAILABLE` stays `false` in U1: the request-side decode/validation and the `Limits` numbers land here, while the `ops`/`features` advertisement and the row-21 negotiation assertions land with U2, so features list only what the driver honors. `TaskResult::encode_into` split into a field encoder so workflow extras append without touching parallel's bytes | rows 1, 2, 3, 16 (decode half), 21 (decode/validation half) — pure, no fork |
| **U2** | `agent/attempt.rs`: extract `Captures`, `spawn_attempt`, `Child`, `run_child`, `StatusRecord`, `read_record`, `finish_attempt` from `parallel.rs` with zero behavior change; `parallel.rs` becomes a caller. Then `agent/workflow.rs`: the §4.2 loop, `begin_group(GroupKind::Workflow)`, wait-all only, per-node results, `order` echo; `supervisor::{begin_group, task_retry, GroupKind}` | `agent_parallel.rs` unchanged and green (the refactor gate); rows 4 (without policy), 18, 20, 22 |
| **U3** | Policies and retries: `failure_policy` × `policy` matrix, `StopReason::FailurePolicy`, retry decision (§5.2), deadline-based backoff, `attempts`/`attempts_detail`, conditions evaluated in the scheduler | rows 4, 5, 6, 7, 8, 9, 10, 12 (retry/backoff timing), 16 (evaluation half), 17 |
| **U4** | Locks: the §8.1 semaphore, acquire-with-slot, hold across attempts and backoff, deterministic choice | rows 13, 14, 15 |
| **U5** | Cancel and the remaining failure paths: queued-workflow cancel, per-task cancel, `targets_queued` for `workflow`, `stop_group_task` for `GroupKind::Workflow`, promptness proofs | rows 10 (cancel half), 11, 12, 19, 21 (cancel half), 23 |
| **U6** | `help workflow`, `help agent` cross-links, `docs/` cross-links, `features` truthfulness review, measurement script | `tests/help.rs`; manual script run |

Ordering constraints: U1 before U2 (the validated request is the scheduler's
input); U2's extraction lands and is verified **before** `workflow.rs` exists;
U2 before U3 (retry needs the scheduler); U3 before U4 (a lock must interact
with backoff, which U3 introduces); U5 last among the code units because every
cancel path needs the final slot lifecycle.

Each unit keeps the full suite green. Nothing merges before the roadmap allows
it.

---

## 13. Riskiest part and mitigation

**Riskiest: the retry lifecycle in the supervisor table (the U2/U3 boundary).**
It is where three invariants that parallel guarantees by construction meet a
node that runs more than once:

1. **a slot's deadline and escalation are armed once** — a retry must get a
   *new* deadline from its *new* spawn, and must not inherit the previous
   attempt's `pending` (a stale `SIGKILL` armed against a reaped pid would fire
   on a reused pid number);
2. **one verdict per slot** — `finish_task` must be called exactly once per
   reaped pid, while `task_retry` reopens the same slot for attempt 2;
3. **one live process group per node** — attempt 1's descendants can outlive its
   leader (§5.4), and starting attempt 2 too early reintroduces, inside one node,
   the file race the design refuses to hide.

Failure modes if this is done casually: a `SIGKILL` delivered to a recycled pid;
a second attempt whose result is assembled from the first attempt's captures; a
retry that spins because the old group never empties; a backoff implemented with
`thread::sleep` that freezes cancel acks and deadline escalation for up to 60 s;
a retry after a group timeout that mints attempts nobody waits for.

Mitigations, in order of importance:

* `task_retry` is the **only** Done→Queued transition, clears `stop` and
  `pending`, and returns the previous pid so the driver must look at it;
* the driver re-checks `take_task_stop` immediately before every spawn (the
  existing window-loop discipline, `parallel.rs:112-116`), so a cancel that
  lands between two attempts is consumed rather than overwritten;
* the "group empty" gate reuses `group_alive` (`supervisor.rs:787`) and the
  existing 50 ms pump, so it cannot block the protocol and cannot deadlock
  against the escalation that is already scheduled;
* `stop.is_none() && parallel_stopped().is_none()` is a hard precondition of
  every retry, which removes the group-deadline ambiguity of
  `supervisor.rs:594-596` from the retry decision entirely;
* rows 8, 9, 10 and 12 are the gate: they fail loudly on a blocked pump (timing),
  on an extra attempt (count), on a missing escalation (orphan) and on a
  duplicated result (capture identity).

**Second risk: the extraction in U2** touches the only tested spawn/reap code.
Mitigation: zero behavior change, `agent_parallel.rs` byte-identical and green
before `workflow.rs` is written, and the extraction is one commit that adds no
feature.

**Third risk: lock accounting.** A lock released on reap but acquired on spawn
can leak if an attempt spawn fails after the lock was taken. Mitigation: acquire
and release are two statements in the same driver struct, `spawn_refused` returns
the lock immediately (it is the same code path as a normal node end), and row 14
with a forced `SHARKY_WORKFLOW_TEST_SPAWN_FAILURE`-style debug hook proves a
refused node frees its lock.

---

## 14. Accepted limitations (stated so they are not discovered later)

1. **No filesystem isolation and no cross-process locking.** Two nodes on the
   same path race; a `lock` serializes nodes of *this* request only. Row 13
   exists to keep this honest.
2. **`depends_on` is order, not data.** No node inherits another's env, cwd or
   output; there is no aggregate/transform node (§10).
3. **A workflow owns the session for its whole duration**, like parallel: other
   requests queue as frames, and only `cancel` acks are out of order. A workflow
   with a long backoff and no group timeout therefore delays the session — set
   `timeout_ms`.
4. **Backoff precision is the 50 ms pump tick**, and a retry of a timed-out
   attempt may wait up to one grace (2 s) for the previous group to empty.
5. **Superseded attempts lose their stream text** (sizes only), by budget (§9.2).
6. **Conditions read only the captured prefix and only UTF-8.** A truncated or
   base64 stream never matches a text predicate.
7. **`max_concurrency` bounds attempts, not processes**; descendants can exceed
   it.
8. **A condition-false node counts as a non-success for its dependents**, so a
   gate short-circuits the chain by design.
9. **No streaming, no partial results**: a node's output exists only in the
   final response.
10. **A retry always re-runs the whole `command`**; the executor cannot resume or
   checkpoint a node.
11. **`sharky workflow` CLI, process-isolation mode, PTY per node and a workflow
    daemon are not implemented**; their seams are additive and named in §10.
12. **Known process escapes are inherited, not introduced**: command-substitution
    children (pre-existing `exec` defect), daemonizing children that call
    `setpgid`.
