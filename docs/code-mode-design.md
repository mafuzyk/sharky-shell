# Native Agent Code Mode — Architecture and Wire Contract

Status: **implementation specification (Phase F)**.
Target: `/home/mafu/SHARKY_AGENT_RUNTIME_CODE_MODE_ROADMAP.md` (Phase F, built on top of Phase E commit `aefb2af`).
Everything in this worktree (`sharky-fasea`) remains local; nothing merges or pushes.

---

## 0. Design Goals and Decision Summary

Modern coding agents pay high latency, token, and quoting costs when forced to turn multi-step tool workflows into fragile `bash -c` round trips:

1. Passing arbitrary paths/args through shell strings invites quoting bugs and command injection.
2. One-shot commands (`exec`) mutate or abort the shared shell session unless explicitly isolated.
3. Noisy build/test logs burn context window tokens or truncate before failure lines appear.
4. Discovering shell capabilities and syntax support forces guesswork or trial-and-error calls.
5. Inspecting-then-acting on N items requires N LLM round trips unless the runtime supports bounded local orchestration.

Sharky addresses these cleanly in three additive layers (no `schema_version` bump; `schema_version: 1`):

| Layer | Capabilities | Implementation |
| --- | --- | --- |
| Layer 1 (F1 & F2) | Typed `argv`, `exec.isolated`, `--redact-env`, `output` projection, Session Capture Store (`capture_id`), `op:"read_capture"` | `protocol.rs`, `json.rs`, `shell.rs`, `attempt.rs`, `projection.rs`, `store.rs`, `agent/mod.rs` |
| Layer 2 (F3) | Machine-readable capability and syntax contract discovery: `sharky capabilities --json` and `sharky help [TOPIC] --json` | `cli.rs`, `help.rs` |
| Layer 3 (F4) | Native programmatic Code Mode orchestrator (`op:"code"` and `sharky code`) with bounded pure-Rust expression/statement runtime and mandatory `audit` envelope | `agent/code.rs`, `cli.rs`, `protocol.rs`, `json.rs` |

---

## 1. Layer 1 — Typed Execution & Output Controls

### 1.1 Typed `argv` Execution (`feature: "argv"`)

`op:"exec"`, `parallel.tasks[]`, and `workflow.tasks[]` accept `"argv": [String, ...]` as a **mutually exclusive** alternative to `"command"`:

- Having both `command` and `argv`, or having neither, is rejected with `INVALID_REQUEST`.
- `argv` must be a non-empty array of strings, `argv[0]` must be non-empty, and no element may contain a NUL byte (`'\0'`).
- Execution resolves `argv[0]` via `exec::resolve` against the current shell/task `PATH` and invokes `execve` directly:
  - **No shell parsing, no globbing, no parameter/command expansion, no shell builtins**.
  - Metacharacters (`$VAR`, `$(cmd)`, `` `cmd` ``, `;`, whitespace, quotes, leading `-`) are passed verbatim as raw argument bytes.
  - POSIX shell conventions apply for exec errors: missing binary exits `127`, non-executable / bad format exits `126`.
  - Full session supervision applies (`stdin`, `cwd`/`env` where permitted, `timeout_ms`, process-group isolation, `cancel`, stdout/stderr capture).

### 1.2 Isolated Single Commands (`feature: "exec_isolated"`)

`op:"exec"` accepts optional `"isolated": true|false` (default `false`):

- When `isolated: true`:
  - The request accepts optional `"cwd"` and `"env"` overrides (`{"KEY": "VAL" | null}`). Supplying `cwd` or `env` on `exec` with `isolated: false` returns `INVALID_REQUEST`.
  - Evaluation happens inside a forked copy-on-write child (`attempt::spawn_attempt`), isolated from the parent `Shell`: `cd`, `export`, `unset`, `set -e`, traps, and `exit` affect only the child process and **never mutate or exit the parent session**, leaving the parent session's `$?` untouched.
  - Parent supervision monitors the child's process group for `timeout_ms` and `cancel`.
  - If `cwd` setup fails or fork fails, the response reports `status: 126` and `termination: "spawn_error"` cleanly without crashing the session.

### 1.3 Environment Redaction (`feature: "env_redaction"`, CLI `--redact-env`)

`sharky agent --stdio [--redact-env]` enables opt-in secret masking on session protocol responses:

- Matching is case-insensitive substring detection on variable names containing: `TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `API_KEY`, `APIKEY`, `PRIVATE_KEY`, `AUTH`, `CREDENTIAL`, `COOKIE`, `SESSION_KEY`.
- Matching values are replaced with `"<redacted>"` in:
  - `state` (`env` and `variables`: scalars, indexed elements, associative values).
  - `env_delta` on `exec`, `exec_isolated`, `parallel`, and `workflow` responses (unset entries remain `null`, presence/deletion stays visible).
- **Honest limitation:** `--redact-env` does **not** scan or scrub arbitrary bytes written to `stdout` or `stderr`.

### 1.4 Runtime Output Projection (`feature: "output_projection"`)

`exec`, `parallel.tasks[]`, and `workflow.tasks[]` accept an optional `"output"` specification:

```json
"output": {
  "strip_ansi": true,
  "grep": "FAILED|panicked at",
  "context": 3,
  "head_lines": 10,
  "tail_lines": 40,
  "max_bytes": 16384
}
```

Validation & semantics:

- `grep` must be non-empty and a valid `regex-lite` regular expression (`INVALID_REQUEST` at decode time on invalid syntax).
- `context` is `0..=100` (default `0`) and requires `grep` (`INVALID_REQUEST` if `context > 0` without `grep`).
- `head_lines` and `tail_lines` are integers in `0..=100000`.
- `max_bytes` is `>= 1`.
- Processing order on valid UTF-8 streams:
  1. Optional `strip_ansi` removes CSI, OSC, and 2-byte ESC sequences.
  2. Line split tracks `total_lines`.
  3. Optional `grep` (+ merged `[m - context ..= m + context]` windows) records `matched_lines`.
  4. Optional `head_lines` / `tail_lines` slice; if both are set and `head_lines + tail_lines < selected.len()`, a single omission marker line `... (<N> lines omitted)` separates head and tail.
  5. Result is UTF-8 char-boundary truncated to `min(spec.max_bytes, max_capture_bytes, remaining_group_budget)`.
- **Non-negotiable invariants:**
  - `stdout_bytes` / `stderr_bytes` always report the **true raw byte count** on disk before projection.
  - `projection` object (`stdout_total_lines`, `stdout_returned_lines`, `stdout_matched_lines`, etc.) makes filtering explicit.
  - Projection **never alters** `status`, `termination`, `signal`, or `error`.
  - Non-UTF-8 binary streams bypass line transforms, slice only to byte limits, and emit valid `base64`.
  - **Workflow note:** `workflow` node `condition` (`stdout_contains`, `stdout_equals`, `stderr_contains`) evaluates against the dependency's delivered `stdout`/`stderr` after capture/projection.

### 1.5 Bounded Session Capture Store & `op:"read_capture"` (`feature: "capture_store"`)

To make projected or truncated logs inspectable on demand **without re-running commands** and **without leaking file descriptors**:

- Every non-empty finished attempt (`exec`, `exec_isolated`, `parallel` task, or final committed `workflow` node attempt) stores raw `stdout` and `stderr` bytes in an in-memory FIFO ring buffer owned by `Session`:
  - `limits.max_capture_store_entries = 32`
  - `limits.max_capture_store_bytes = 16777216` (16 MiB)
  - Per-stream retention cap: `4 MiB` (`STORE_STREAM_CAP` / `max_bytes / 2`).
  - Zero persistent open file descriptors in the store (`/proc/<pid>/fd` remains flat).
  - Superseded workflow retry attempts are **not** stored; only the final committed attempt is recorded.
- Responses with stored output carry `"capture_id": "cap-N"`.
- `op:"read_capture"` retrieves slices from the store:
  - Line mode: `{"capture_id":"cap-1","stream":"stdout"|"stderr","line_start":1,"line_count":50,"grep":"...","context":2,"strip_ansi":false,"max_bytes":65536}`.
  - Byte mode: `{"capture_id":"cap-1","stream":"stdout"|"stderr","byte_offset":0,"max_bytes":4096}` (mutually exclusive with line options).
  - Returns `type:"capture"` with `data`, `encoding` (`"utf8"`|`base64`), `total_bytes`, `stored_bytes`, line metadata, and `truncated`.
  - Evicted or unknown IDs return non-fatal `CAPTURE_NOT_FOUND` (session keeps serving).

---

## 2. Layer 2 — Structured Discovery (F3)

- `sharky capabilities --json`: read-only CLI subcommand dispatched before shell initialization (zero config, profile, or history reads/writes). Emits `schema_version`, `shell_version`, live `protocol` (`ProtocolInfo::default()`), `builtins`, `cli_subcommands`, honest `syntax_support` (`supported` / `partial` / `unsupported`), and explicit `isolation_guarantees`.
- `sharky help [TOPIC] --json`: emits structured JSON `{schema_version: 1, kind, topic?, text?, topics?}` for index or specific topics (`builtin` | `cli` | `config`).

---

## 3. Layer 3 — Native Programmatic Orchestrator (F4)

- `op:"code"` and one-shot `sharky code -e '<program>' | <file> | -` provide a bounded, dependency-free micro-runtime operating over `json::Value` and first-class closures:
  - **Builtins:** `sh(cmd [, opts])`, `run(argv [, opts])`, `parallel(tasks [, opts])`, `map_parallel(items, max_concurrency, fn)`, `workflow(spec)`, `state([sections])`, `read_capture(capture_id [, opts])`.
  - **Pure methods:** arrays support `reduce(init, fn)` (left fold with `(acc, item)` closure), `sort_by(fn)` (stable sort by a numeric or string key, leaving `sort` unchanged; all closure keys must be numbers or all must be strings, otherwise the call fails with a runtime error instead of producing an arbitrary order), `group_by(fn)` (object keyed by the closure result stringified like `str`, each bucket keeping input order), plus the existing `map`, `filter`, `any`, `all` and `find`. Closure-based methods call `tick()` once per element and share the step and call budgets; they never spawn processes. Mandatory `audit` still reports every child outcome.
  - **Execution:** shares the session's `Shell`, `supervisor`, `CaptureStore`, and task attempt machinery (never spawns an external runtime).
  - **Bounds:** `max_steps` (default `10000`, cap `100000`), `max_calls` (default `64`, cap `256`), `timeout_ms` deadline, recursion depth `32`. Fatal signals and the deadline are checked at every step; supervisor cancellation (`cancel`/`timeout` verdicts) is polled every 32 evaluation steps (`SERVICE_STRIDE`) and at every builtin entry, then propagated to active child processes.
  - **Mandatory `audit` envelope:** every `type:"code"` response includes `audit: {calls, failed_calls, cancelled_calls, first_failure_status, steps, captures}`. Script transforms cannot conceal non-zero child outcomes.

---

## 4. Explicit Security & Honesty Boundaries

1. **No filesystem sandbox:** `exec` (`isolated: true`), `parallel`, `workflow`, and `code` isolate **shell interpreter state** via `fork`, NOT disk writes. Concurrent tasks targeting the same files can race unless callers use separate directories or `workflow.lock`.
2. **Process-local locks only:** `workflow.lock` serializes nodes inside a single `op:"workflow"` request; there is no cross-session or cross-process file lock.
3. **Truthful failures & truncation:** Neither `output` projection nor `op:"code"` can rewrite `audit.failed_calls`, raw `*_bytes`, `status`, or `termination`.

---

## 5. Measured Dogfooding Benchmarks (F5)

The non-gating benchmark `bash scripts/agent-dogfood-bench.sh` was run twice
from this worktree, exit 0 both times, against `rust/target/release/sharky`
(cargo release profile: `lto = true`, `codegen-units = 1`, `strip = true`,
`panic = "abort"`) on an AMD Ryzen 5 5600GT (6 cores, 13.5 GiB RAM), host
`mafu-nix` (Linux 7.2.15-zen1, x86_64, Python 3.14.10). The numbers below are
from the second run. These are wall-clock observations on one loaded machine,
not a controlled microbenchmark: timings fluctuate with machine load, and the
bash baseline models naive quoting rather than a competent agent. The script
asserts correctness and stability but is a measurement aid, not a gate.

1. **Dynamic fan-out over hostile filenames.** 24 files with shell-hostile
   names: the naive bash baseline took 25 round trips, 72.1 ms, 0/24 correct
   with 24 quoting failures, while one `op:"code"` turn took 16.8 ms,
   24/24 correct with `audit.calls = 25` and 0 failed calls (4.29x wall time,
   25 -> 1 turns). This shows typed `argv` avoids quoting bugs and that local
   fan-out collapses agent round trips; it does not measure latency on a
   quiet, isolated host.
2. **Noisy log triage.** A 5000-line, 234423-byte raw stdout was projected to
   815 bytes of JSON stdout on the wire (287.6x reduction) in 22.0 ms, with
   20/5000 lines returned and 10 matching lines. `read_capture` paged windows
   of 931/945/945 bytes at line starts 1/2500/4981. FD count stayed 8 -> 8
   over 30 projected execs plus 3 `read_capture` calls in 122.4 ms; RSS went
   4032 -> 12916 KiB. This demonstrates projection keeps context small and
   capture paging needs no retained descriptors; RSS is a single observation,
   not a leak proof.
3. **Discovery, isolation and secret hygiene.** `capabilities --json` reported
   Notice: historical baseline snapshot measured on shell `3.7.0` with 8 ops and 20 features (`4.0.0-alpha` expands `features` to 26 with Phase G and MCP).
   Isolated `exec` returned child status 9 with `cwd=/` while the parent kept
   its cwd, `EXPORTED_VAR=parent`, no leak and `last_status=7`; non-isolated
   `exec` mutated the parent cwd to `/` and set `NONISO=1` as expected.
   `--redact-env` masked `MY_API_KEY` and `DB_PASSWORD` in `env_delta`,
   `state.env` and isolated `env_delta` while leaving `OK_VAL` visible. This
   confirms the documented contract; it does not establish a filesystem
   sandbox.
4. **Truthful audit.** An `op:"code"` program whose `value` claimed `ok=true`
   still reported `calls=3`, `failed_calls=3`, `first_failure_status=1` and
   statuses `[1, 7, 9]`. This confirms transforms cannot hide non-zero child
   outcomes; the 3-call case is a small correctness check, not a stress test.
