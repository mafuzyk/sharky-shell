# Agent Runtime MVP — Architecture and Contract

Status: historical design proposal for the Fase C MVP, now implemented and
extended. The delivered surface is summarized in `README.md` ("Agent
interface"); the later layers are specified in `docs/code-mode-design.md`,
`docs/parallel-executor-design.md` and `docs/workflow-executor-design.md`.
Target: roadmap Fase C (`SHARKY_AGENT_RUNTIME_CODE_MODE_ROADMAP.md` §6, §7,
§11, §12.2, §14 Fase C). Repository baseline for the original proposal:
`sharky-fasea`, detached HEAD `092e570`.

This document decides the shape of the Agent Runtime MVP: one CLI invocation, one
wire protocol, one isolation strategy, one supervisor delta, and one test gate.
Everything here is local; nothing merges or pushes until the roadmap completes.

Citations are `path:line` inside this worktree. Every claim about existing
behavior was read from the source. Where the implemented contract moved past
this proposal, the text says so explicitly.

---

## 0. Decision summary

| # | Question | Decision |
| --- | --- | --- |
| D1 | CLI surface | `sharky agent --stdio [--no-config]` — a pre-dispatch subcommand, never a flag on the existing parser |
| D2 | Framing | JSONL, exactly one message per line, `schema_version:1`, request/result/error separated, stable error-code taxonomy |
| D3 | Stdin isolation | Control channel moved to a high-numbered `O_CLOEXEC` fd; fd 0 permanently `/dev/null`; per-request payload delivered as an unlinked temp **file** on fd 0 |
| D4 | Stdout/stderr isolation | Per-request unlinked temp files on fd 1/2, restored by dev/ino identity check; **no drainer thread** — files cannot deadlock, pipes would |
| D5 | Lifecycle | Serial session; `exec` evaluates in-process (state persists), `exit`/`set -e` abort are session-terminal; EOF, `close`, and fatal signals share one cleanup path |
| D6 | Deltas and snapshots | `env_delta` per request from the existing `env_delta()` helper; `state` returns summary by default, env/variables/functions only when explicitly requested |
| D7 | Cancellation | `cancel` signals the request's process group (TERM → grace → KILL); enabled by a request-scoped `setpgid` plus a single gated wait hook |
| D8 | Module layout | New `rust/src/agent/` module (5 files); `cli.rs`, `shell.rs`, `exec.rs`, `signal.rs` receive small, gated additions only |
| D9 | JSON parsing | Hand-rolled strict request parser (~300 lines) + hand-rolled base64; no new dependency, no dev-dependency, frozen test corpus |
| D10 | Non-goals & Phase G delivery | At original MVP scope, parallel, workflows, sockets, PTY, streaming and sandbox were deferred behind reserved seams. By `4.0.0-alpha` (Phases D–G), parallel, workflow, Code Mode, single-client socket, optional `--mcp`, `tty`, `stream`, cross-session `flock` locks, opt-in `sandbox`, and `--state-file` resume are implemented. |

---

## 1. Why the naive path fails (baseline, cited)

1. Batch stdin reads to EOF before anything runs:
   `cli.rs:190` calls `term::read_stdin_lines()`, which is
   `io::stdin().read_to_string` (`term.rs:453-457`). No request/response is
   possible on the legacy pipe, exactly as roadmap §6.1 states.
2. `--json -c` isolates by **forking** (`cli.rs:313-393`): capture temp files
   (`cli.rs:315-335`, `cli.rs:484-509`), `dup2` in the child (`cli.rs:346-347`),
   delta written from the child (`cli.rs:349-360`), parent `waitpid`
   (`cli.rs:372`). That proves the mechanism but cannot be reused directly for a
   session: state changes in the child would be lost, which is the opposite of
   what §6.1 requires.
3. The existing capture reader is lossy for binary data:
   `String::from_utf8_lossy` (`cli.rs:481`). `--json -c` therefore cannot carry
   bytes *faithfully*; the session contract must, per §6.5.
4. `Shell::run()` runs the EXIT trap and sets `exit_trap_ran` on every call
   (`shell.rs:487-511`, `shell.rs:532-536`). A session that called `run()` per
   request would run the EXIT action after the *first* request and never again.
   The REPL already avoids this by driving `eval_with` (`shell.rs:641`) and
   calling `finish` only when it leaves (`repl.rs:110-113`, `repl.rs:137-149`).
   The session must do the same.
5. Foreground children are **not** placed in their own process group unless the
   shell is interactive: `spawn` guards `setpgid` with `self.interactive`
   (`shell.rs:2159-2171`), and the pipeline fork does the same
   (`shell.rs:1044-1077`). Only `run_background` always calls
   `setpgid(child, child)` (`shell.rs:872-896`). Consequences for the MVP:
   cancel-by-pgid would signal the agent together with the child.
6. `exec` really replaces the process: `exec_builtin` calls
   `exec::exec_now_plain` (`shell.rs:1518-1575`, call at `shell.rs:1567`). In a
   session that would kill the protocol mid-response.
7. `read` and every external command take fd 0 as-is; the builtin reads byte by
   byte from fd 0 (`shell.rs:9574-9600`, also `shell.rs:9811`). Whatever sits on
   fd 0 is what they consume, so fd 0 must never be the control channel.
8. Redirect targets are arbitrary numbers: `Action::Read`/`Action::Dup` reach
   `move_fd` → `dup2` on any fd (`shell.rs:10179-10184`,
   `shell.rs:10313-10345`). A control fd is therefore clobberable by user input
   and must be defended, not merely hidden.

---

## 2. D1 — CLI surface

**Decision:** `sharky agent --stdio [--no-config] [--redact-env]`, dispatched
from `cli::run` before the flag parser, in the same place as the other
subcommands. The later `--socket PATH` transport is described in §15.

```text
sharky agent --stdio [--no-config] [--redact-env]
sharky agent --socket PATH [--no-config] [--redact-env]
```

Rejected alternatives:

* `sharky --json-lines`: the existing parser treats an unrecognized flag as a
  hard error (`cli.rs:79-82`) and already spends `--json` on a different
  contract (`cli.rs:117-124` rejects `--json` without `-c`/`state`). Overloading
  that namespace forces every future reader of the flag soup to reason about two
  incompatible meanings of "JSON mode".
* `sharky agent` with an implicit stdio default: an accidental
  `sharky agent < file` would hang on a protocol the user did not ask for. An
  explicit transport flag keeps `--socket PATH` (now delivered, §15) unambiguous.
* Treating `agent` as a script name: the positional branch would do exactly that
  (`cli.rs:83-93` → `shell.run_script`), so pre-dispatch is mandatory, not
  cosmetic.

Dispatch rules:

* `crate::agent::try_cli(&args)` returns `Option<i32>`, matching
  `crate::diagnostics::try_run` (`diagnostics.rs:28-47`) and
  `crate::help::try_cli` (`help.rs:142-162`).
* It activates only when `args[0] == "agent"`. Unknown/invalid agent flags print
  `sharky: agent: ...` to stderr and exit `2`, the established usage status.
* `--stdio` or `--socket PATH` is required; they are mutually exclusive.
  `--no-config` and `--redact-env` are accepted by both transports.
* Login profiles are never read in agent mode (no `profiles::login` call, cf.
  `cli.rs:166-171`); `--login` is rejected with exit 2 rather than silently
  printing profile output into the protocol stream.
* `sharky -c`, script, batch stdin, REPL, `state --json`, `--check` paths are
  untouched: `cli.rs` gains one early-return line and nothing else.
* A help topic `agent` is added to `CLI_TOPICS` and `cli_text`; the unit test
  enforces that the topic exists. `sharky agent --help` prints the same text.
  Discovery is also available as `sharky capabilities --json` and
  `sharky help [TOPIC] --json`.

Preconditions checked before serving: with `--stdio`, stdin must not be a TTY.
A TTY stdin exits 2 with an explicit message, so nobody ends up typing shell
commands into a JSONL parser. `--socket` refuses an existing path (including
a stale socket) and an overlong path, and binds mode 0600.

---

## 3. D2 — Framing and protocol

### 3.1 Envelope rules

* One UTF-8 JSON value per line, terminated by `\n`. No pretty printing, no
  concatenated frames, no length prefix.
* Multi-line shell code is an escaped JSON string (`"command":"if x; then\n ..."`).
  The session never splits a command on newlines itself.
* Every message, in both directions, carries `schema_version` (integer). This
  MVP supports exactly `1`. A request with another version is answered with
  `UNSUPPORTED_SCHEMA_VERSION` and the session stays alive.
* `id` is an opaque string (≤ 256 bytes) chosen by the client, unique within the
  connection. Responses always echo it. Protocol-level errors that cannot be
  attributed (invalid JSON) use `"id":null`.
* Unknown object keys are ignored (forward compatibility). Unknown `type` values
  in responses must be ignored by clients, which is what makes later streaming
  events additive. Unknown `op` values are answered with `UNKNOWN_OP` and are
  never fatal — a newer client talking to an older shell learns what is missing
  instead of breaking the session.
* The protocol carries no `tokens`, no environment echo, and no command echo.

### 3.2 `ready` (first line, emitted once, `id` absent)

```json
{"schema_version":1,"type":"ready","session":"s-4242-8f1c",
 "pid":4242,"shell_version":"4.0.0-alpha",
 "protocol":{"versions":[1],
   "ops":["exec","state","cancel","close","parallel","workflow",
          "read_capture","code"],
   "features":["stdin_payload","env_delta","binary_base64","timeout",
     "cancel","process_groups","parallel","workflow","argv",
     "exec_isolated","env_redaction","output_projection",
     "capture_store","code_mode","pty","streaming_output",
     "cross_session_locks","opt_in_sandbox","session_resume","mcp"],
   "limits":{"max_line_bytes":1048576,"max_stdin_bytes":1048576,
             "max_capture_bytes":4194304,"max_id_bytes":256},
   "cancel_grace_ms":2000}}
```

`features` is the capability contract of roadmap §6.8: a client must not assume a
field is honored unless the corresponding feature is listed. This is also the
mechanism that lets `tty`, `stream`, and later ops appear without a version bump
(§11). The value above is abridged; `sharky capabilities --json` prints the
authoritative current list, including per-op subfeatures and limits.

### 3.3 Requests

```json
{"schema_version":1,"id":"r1","op":"exec","command":"echo $x"}
{"schema_version":1,"id":"r2","op":"exec","command":"cat","stdin":"aGkK","stdin_encoding":"base64"}
{"schema_version":1,"id":"r3","op":"state","include":["env"]}
{"schema_version":1,"id":"r4","op":"cancel","target":"r1"}
{"schema_version":1,"id":"r5","op":"close","wait_ms":2000}
```

| op | fields | meaning |
| --- | --- | --- |
| `exec` | `command` or `argv` (mutually exclusive strings/array), `stdin` (string), `stdin_encoding`, `timeout_ms`, `env_delta` (bool), `isolated` (bool), `output` projection; `cwd` and `env` only with `isolated` | parse and evaluate in the session shell, or `execve` `argv` directly |
| `state` | `include` (array: `env`, `variables`, `functions`, `aliases`, `traps`, `jobs`; default `[]`) | read-only snapshot; all six sections are implemented |
| `parallel` | `tasks[]`, `max_concurrency`, `policy`, group `timeout_ms` | fan out isolated interpreter copies (§11 and `docs/parallel-executor-design.md`) |
| `workflow` | DAG task spec with `depends_on`, retries, conditions, failure policies and locks | schedule dependent tasks (`docs/workflow-executor-design.md`) |
| `read_capture` | `capture_id`, stream, line or byte window | page raw captured output from the bounded in-memory store |
| `code` | `code` program plus optional `args` and bounds | run a bounded programmatic orchestration script (`docs/code-mode-design.md`) |
| `cancel` | `target` (id of a request), `grace_ms` (optional, ≤ `cancel_grace_ms`) | terminate the target's process group |
| `close` | `wait_ms` (optional) | clean shutdown, then EOF |

`exec` with an empty or whitespace-only `command` is a valid no-op result with
`status:0` and `duration_ms:0` (it is not a protocol error).

### 3.4 Responses

`result` (the only response that carries command status):

```json
{"schema_version":1,"id":"r1","type":"result",
 "status":0,"termination":"exited","duration_ms":3,"duration_us":3250,"cwd":"/tmp",
 "stdout":"hi\n","stdout_encoding":"utf8","stdout_bytes":3,"stdout_truncated":false,
 "stderr":"","stderr_encoding":"utf8","stderr_bytes":0,"stderr_truncated":false,
 "env_delta":{},"pgid":4243}
```

* `status` keeps the `--json -c` convention: the exit status, `128+signal` for a
  signaled command (`cli.rs:372-376`), so an existing consumer can compare both
  modes field by field.
* `duration_ms` is unchanged and stays the integer milliseconds of
  `--json -c`. `duration_us` is an **additive** integer-microsecond reading from
  the same `Instant`, so `duration_ms == duration_us / 1000` (integer division).
  It exists because a trivial command rounds `duration_ms` to `0`, which is too
  coarse for protocol profiling; a consumer that ignores unknown fields sees no
  change. It is a result field, not a `ready.protocol.features` entry, and does
  not change `schema_version`.
* `termination` ∈ `exited` | `signaled` | `timeout` | `cancelled`. `signaled`
  adds `"signal":"SIGTERM"`. This is how roadmap §7.1's demand ("differentiate
  exit code, terminating signal, timeout, cancellation, spawn error, protocol
  error") is satisfied without overloading `status`.
* `stdout_encoding` ∈ `utf8` | `base64`. Invalid UTF-8 switches to base64 with
  the exact bytes preserved — the gap left by `from_utf8_lossy`
  (`cli.rs:481`).
* `stdout_bytes` is the **real** captured size, so truncation is visible and
  measurable (§11: "truncation EXPLICIT, with byte count").
* `cwd` and `env_delta` reuse today's semantics: `cwd` is the post-execution
  working directory, `env_delta` maps changed variables to values and removed
  ones to `null` (`cli.rs:395-436`).

### Non-UTF-8 process inputs (known limitation)

Command-line arguments, environment names and values, and the initial working
directory are Unix byte strings. The lexer, word model, variables and JSON
protocol currently represent them as Rust `String`, so a value that is not
valid UTF-8 is decoded with `U+FFFD` replacement. When that happens sharky now
prints one explicit warning to stderr naming the affected item kind
(`command-line argument`, `environment variable`, `environment variable name`
or `working directory`). The warning never prints the offending bytes or the
replacement characters, so a hostile path cannot inject text into diagnostics.

The conversion itself is still lossy: a glob match or a `printf '%s'` of such
an argument shows replacement characters where bash would preserve the raw
byte. Agents that must handle arbitrary byte names should use the agent
protocol's typed `argv`, `stdout_encoding:"base64"` and byte-offset capture
windows; those paths already preserve bytes. A full `OsString` refactor of the
lexer and variable model is out of scope for this change.

`state`:

```json
{"schema_version":1,"id":"r3","type":"state","cwd":"/tmp","last_status":0,"pid":4242,
 "jobs":[{"id":1,"pgid":4244,"pids":[4244],"command":"sleep 100","state":"running"}],
 "options":{"errexit":false,"nounset":false,"pipefail":false,"noclobber":false,"xtrace":false},
 "env":{"PATH":"..."}}
```

`env` appears only when `"env"` was requested; `variables` returns
`{name:{"value":..., "exported":bool, "readonly":bool}}`, arrays return
`{"indexed":{name:[null|string,...]},"assoc":{name:{key:value}}}` so an unset
element (`null` hole) is never confused with the string `"null"` (roadmap §6.7).
Private `Shell` fields (`funcs`, `arrays`, `assoc`, `traps`, `readonly_vars` —
`shell.rs:41-60`, `shell.rs:100-120`) are read through new `pub(crate)`
accessors; no field becomes `pub`.

`ack` (an accepted `cancel` or `close`; no command ran for the ack itself):

```json
{"schema_version":1,"id":"r4","type":"ack","op":"cancel","outcome":"delivered"}
```

`cancel` answers `delivered` and never writes a `closed` event (§8.2). `close`
uses the following **single contract**: exactly two lines, in this order, with
no other line between them.

```json
{"schema_version":1,"id":"r5","type":"ack","op":"close","outcome":"closed"}
{"schema_version":1,"id":"r5","type":"closed","status":0}
```

The first line says the op was accepted. The second is the terminal event: the
`closed` event echoes the `close` request's `id`, carries the status the process
is about to leave with, and is followed by EOF. A client that stops at the
first response may leave that last line unread; a client waiting for
`type:"closed"` must read both lines. There is no mode in which `close` writes
the ack without the `closed` event, and no second `closed` event is written.

A session that ends by itself — `exit N`, or a `set -e`/failed-unset abort
(§6.3) — writes no ack. Its last `result` is followed by exactly one terminal
event with the **null** id:

```json
{"schema_version":1,"id":null,"type":"closed","status":N}
```

EOF between requests writes no `closed` event, because there is nobody left to
answer (§6.5). `schema_version` and the `id` vocabulary are unchanged on every
path above.

`error` (protocol-level only — the command did **not** run, or its result is
unknown):

```json
{"schema_version":1,"id":"r1","type":"error",
 "error":{"code":"DUPLICATE_ID","message":"id r1 was already used","fatal":false}}
```

The distinction is the roadmap's: `result.status` is the command's, `error` is the
protocol's. A session that answers `error` for an `exec` guarantees no shell code
ran for it.

### 3.5 Error taxonomy (stable)

| code | when | fatal |
| --- | --- | --- |
| `INVALID_JSON` | line is not a single well-formed JSON value | no |
| `INVALID_REQUEST` | JSON but not an object, or a field has the wrong type / a required field is missing | no |
| `UNSUPPORTED_SCHEMA_VERSION` | `schema_version` ∉ {1} | no |
| `UNKNOWN_OP` | `op` not in `ready.protocol.ops` | no |
| `DUPLICATE_ID` | `id` already used on this connection | no |
| `INVALID_ID` | missing/empty/over-long `id` | no |
| `MESSAGE_TOO_LARGE` | line exceeds `max_line_bytes` (the rest of the line is discarded up to the newline) | no |
| `STDIN_TOO_LARGE` | decoded payload exceeds `max_stdin_bytes` | no |
| `INVALID_ENCODING` | `stdin_encoding:"utf8"` with invalid UTF-8, or a malformed base64 payload | no |
| `QUEUE_FULL` | the pending-request queue is full (the frame is answered on the spot; the id is not recorded, so the request may be retried once the queue drains) | no |
| `PARTIAL_REQUEST` | EOF with a non-empty unterminated buffer that is not a complete JSON value | **yes** (then exit 1) |
| `CANCEL_UNKNOWN_ID` | `cancel.target` was never seen | no |
| `CANCEL_TOO_LATE` | target already finished | no |
| `SESSION_CLOSED` | any request after the session became terminal | **yes** (EOF follows) |
| `PROTOCOL_FD_LOST` | control fd no longer identifies the original pipe/pty (dev/ino mismatch) | **yes** |
| `INTERNAL` | a bug guard (I/O failure writing the payload file, etc.); message is diagnostic only | **yes** |

Codes are part of the contract; `message` is human text and may change. Adding a
code is forward compatible; changing one is a `schema_version` bump.

### 3.6 Limits and edge behavior

* `max_line_bytes` default 1 MiB, `max_stdin_bytes` 1 MiB, `max_capture_bytes`
  4 MiB per stream. All are echoed in `ready.limits`. Flags
  (`--max-line-bytes`, `--max-capture-bytes`) exist for tests and heavy users;
  the defaults are the documented contract.
* Duplicate detection keeps every accepted `id` for the process lifetime, capped
  at 8192 ids; beyond the cap the oldest ids stop being tracked and the `ready`
  payload advertises the reduced guarantee. Documented, not silent.
* Invalid JSON on a line: discard the line, answer `INVALID_JSON` with
  `"id":null`, keep serving. Resynchronization is line-based by construction.
* Partial EOF: a trailing line without `\n` that parses as a complete value **is
  executed** (JSONL readers accept a final unterminated line); anything else is
  `PARTIAL_REQUEST` and no shell code runs for it.
* Response ordering: strictly the order requests were accepted. A request that
  arrives while an `exec` is running is queued and answered after it (see D7 for
  the one exception that is honored out of order: `cancel`). The queue holds at
  most 1024 frames; a valid `cancel`, a valid `close` and the trailing EOF still
  fit beyond that bound, and any other overflow frame is answered immediately
  with non-fatal `QUEUE_FULL` (the id is not recorded, so retrying after the
  running request is answered is valid).
* The protocol stream is flushed per line; a write that returns `EPIPE` means the
  client is gone and starts the cleanup path (D5).

---

## 4. D3 — stdin isolation (roadmap §6.3, P0)

**Decision:** the control channel is never fd 0, never fd 1, and never fd 2.

Startup sequence, executed before any shell code runs and before configuration is
loaded:

1. `proto_out = fcntl(F_DUPFD_CLOEXEC, 900)` from fd 1; `proto_in` from fd 0;
   `proto_err` from fd 2; `null_fd` = `open("/dev/null", O_RDWR|O_CLOEXEC)`.
2. `dup2(null_fd, 0/1/2)` — from this point on, anything the agent process or a
   leaked child writes to stdio lands in `/dev/null`, never in the protocol.
3. All protocol I/O is raw `read`/`write` on the high descriptors. `O_CLOEXEC`
   keeps them out of every `execve`, including `exec_replace`
   (`exec.rs:184-190`).

Two layers of defense beyond CLOEXEC, because redirects can target arbitrary
numbers (`shell.rs:10313-10345`):

* The control descriptors live at 900+, outside the range any realistic
  redirection uses. `F_DUPFD` guarantees "first free ≥ 900".
* After every request the session `fstat`s the control descriptor and compares
  `(st_dev, st_ino)` with the value recorded at startup. A mismatch is
  `PROTOCOL_FD_LOST`, fatal: it is better to stop than to answer a client on a
  descriptor a command now owns.
* Forked shell-code children close the control descriptors: a
  `crate::agent::child_guard()` call sits beside each of the eight
  `reset_child_traps()` call sites (`shell.rs:896, 1075, 1704, 1780, 2170, 6493,
  6572, 6712`). It is a no-op unless a session is active, so REPL/batch are
  unaffected. Rationale: an `exec`'d program is stopped by CLOEXEC, but a
  subshell that inherits the fd could still `echo >&900` by accident.

**Per-request stdin payload:** an unlinked temp file (`create_new`, mode `0600`,
`O_CLOEXEC`, then `unlink` — the exact discipline of `capture_file`,
`cli.rs:484-509`), filled with the payload, `lseek(0)`, then `dup2` onto fd 0 for
the duration of the request (dup2 clears CLOEXEC on the copy, as intended).

Why a file and not a pipe: the session is single-threaded and evaluates in the
foreground. Writing more than the pipe capacity (64 KiB) before the command reads
would block the only thread forever. A file has no writer, so `read` sees EOF
where the payload ends — which is also the correct semantic for `read` and `cat`.

**Restore discipline.** Before the request:

1. `dup` whatever fd 0/1/2 point at with `F_DUPFD_CLOEXEC` from 10 (or record
   that the descriptor is closed), so the destination belonging to the user
   survives the swap without being reachable by number from shell code;
2. `dup2` the payload (fd 0) and the capture files (fd 1/2) onto the targets.
   Capture and payload descriptors themselves always live above fd 10: a request
   can leave a standard descriptor closed, and the next `open` would otherwise
   hand that number to a capture, making `dup2(1, 1)` a no-op and letting the
   restore repoint the file the result is read from.

After the request:

1. flush the Rust stdio handles (the same precaution as `cli.rs:337-338`,
   `cli.rs:361-362`; `println!` is line-buffered, so unflushed bytes would
   otherwise land on the *next* descriptor);
2. replay the captured stdout/stderr into the saved destination (when it is a
   real destination: not the session's `/dev/null` and not an unlinked regular
   file, which can only be a stale capture left by `exec 2>&1`). The replay is
   what keeps `exec >log` observable across requests, because the request itself
   ran with the capture on fd 1 and bash would have written the same bytes to
   the log. A destination that went away (`EPIPE`) is not fatal;
3. `fstat` the current fd 0/1/2 and compare `(st_dev, st_ino)` with the capture
   installed there; only when the identity matches, `dup2` the saved descriptor
   back (or close the target when it was closed before the request). When it
   does not match, the user replaced that descriptor with `exec 0<file` /
   `exec >log` inside the request, and bash semantics say that redirection
   **persists**: the request's own redirect is left exactly where it is;
4. close the saved copies.

Persisting the saved destination is what makes the redirect survive the *next*
request too: request N+1 saves the user's log, runs with the capture on fd 1 so
its output still reaches the response, replays those bytes into the log and puts
the log back. A descriptor the request closed stays closed, by the same rule.

This is the same save/apply/restore shape as `with_actions`
(`shell.rs:1382-1403`), kept in the agent module because the targets here are
fixed (0,1,2) and the identity check is the extra requirement.


Acceptance for §6.3: `read` and `cat` must not consume control frames. They
cannot: fd 0 holds a payload file or `/dev/null`, and the control reader owns a
separate descriptor.

---

## 5. D4 — stdout/stderr isolation (roadmap §6.5, P0)

**Decision:** per-request unlinked temp files for stdout and stderr, read back
after the request, with explicit truncation metadata and base64 for binary
payloads. **No drainer thread, no pipes in the request path.**

Isolation: `dup2(out_file, 1)`, `dup2(err_file, 2)`. Every builtin `println!`,
every Rust `std::io::stdout()` write, every external program, every pipeline
member and every background job inherits those descriptors, so all of them are
captured. Nothing can reach the protocol stream because fd 1/2 are files and the
protocol lives on fd 900+ (D3).

### 5.1 Why files rather than pipes for the MVP

The roadmap (§7.1) demands "no deadlock when stdout AND stderr produce a lot of
content simultaneously; drain both concurrently". Two implementations satisfy it:

| | pipes + drainer thread | unlinked temp files |
| --- | --- | --- |
| deadlock | only if both streams are drained concurrently on separate threads | impossible — `write(2)` to a file never blocks on the reader |
| threads | one, or two, alive across `fork` | none |
| fork safety | the shell forks constantly (`shell.rs:1044, 1671, 1755, 2143, 6491, 6570, 6692`); forking a process whose other thread may hold the allocator lock is exactly the hazard roadmap §7.2 flags | unaffected |
| memory | bounded by the drain budget | bounded by the read budget |
| incremental output | natural | needs post-hoc read |

The MVP chooses the temp-file row. It satisfies the no-deadlock and no-leak
requirements, keeps the interpreter single-threaded (which the whole signal
design assumes — `signal.rs:283-296` masks around `fork`), and needs no new
blocking discipline. The cost is that output is available only at the end of a
request; that is a documented MVP property, and streaming is a later feature
behind the `type` union (§D10).

This is also a deliberate divergence from `run_json_command` only in *reading*:
`read_capture` + `from_utf8_lossy` (`cli.rs:474-482`) loses bytes, so the session
gets its own `capture.rs` reader that returns `(bytes, truncated, size)` and
chooses `utf8`/`base64`.

### 5.2 Background jobs

A background job started inside a request keeps the request's fd 1/2, which are
the now-unlinked files. When the request ends the parent restores its own fd 1/2
to `/dev/null`; the job keeps writing to its (unlinked, unreachable) file. Its
output is *not* reported in a later response — by design: a job that outlives its
request has no owner to deliver to. It therefore cannot corrupt the protocol,
which is the requirement.

`reap_jobs()` prints notices to stdout (`shell.rs:3338-3340`); in a session fd 1
is `/dev/null` between requests, so notices are lost rather than leaked. The
session calls `reap_jobs()` between requests purely to prevent zombies. Surfacing
notices as data (`take_job_notices`, `shell.rs:3343-3345`) is a post-MVP
improvement; the seam is an additive `notices` array.

### 5.3 Truncation and binary rules

* Read at most `max_capture_bytes` from the start of each file, but always report
  the real size from `fstat` (`stdout_bytes`/`stderr_bytes`) plus
  `stdout_truncated`/`stderr_truncated`.
* Valid UTF-8 → `encoding:"utf8"`, string value. Invalid UTF-8 anywhere → the
  whole stream is base64 with `encoding:"base64"` (no mixed encoding, no lossy
  replacement).
* ANSI escapes are preserved verbatim. The session does not interpret them; a
  TTY-aware client decides. `tty` is not claimed in `ready.protocol.features`.
* A capture file that cannot be created → `INTERNAL`, fatal, because the session
  can no longer guarantee that output will not reach the protocol. A capture
  whose name cannot be unlinked is the same failure: a reachable name would keep
  the request's output readable after the response, so the descriptor is closed
  and the error is reported instead of ignored.

---

## 6. D5 — Session lifecycle

### 6.1 States

```
start -> ready -> serving (exec|state|cancel)* -> terminal -> cleanup -> exit
```

`serving` is serial: exactly one `exec` runs at a time. `state` and `cancel` are
handled from the queue.

### 6.2 Per-request evaluation

The session calls a new `Shell::session_eval(source) -> Result<i32, ParseError>`
that does what `eval_with` does (`shell.rs:641-660`) and **not** what `run` does
(`shell.rs:487-511`): no `finish`, no EXIT trap, no `shlvl_tail` bookkeeping
change. The session then stores `shell.last_duration_ms` itself (as the REPL does
at `repl.rs:102`), clears `signal::take_interrupt()`/`clear_interrupt()` before
each request (mirroring `repl.rs:93`), and calls `reap_jobs()` between requests.

`exit_trap_ran` (`shell.rs:86`) is therefore false for the whole session and the
EXIT action runs exactly once, in `finish`, during cleanup.

### 6.3 `exit` and aborts

`exit` sets `exit_requested` (`shell.rs:2220-2227`); `errexit` and a failing
unset-parameter expansion set it too (`shell.rs:955-968`, `shell.rs:1596-1607`).
In a session that is **session-terminal**, matching script semantics rather than
interactive ones:

1. the request's `result` is written first (`termination:"exited"`, real status);
2. `shell.finish(status)` runs the EXIT action once;
3. a `closed` event is written (`{"id":null,"type":"closed","status":N}`);
4. the process exits `N`; further requests are not read.

Rejected alternative: continue serving after `exit` by clearing the flag. That
would silently turn `exit` into `return` and `set -e` into a no-op — a lie an
agent would eventually build on. Documented in `help agent` and in this file.

`trap` keeps its normal semantics inside a request (`service_traps`,
`shell.rs:5250`); only `EXIT` is deferred to session close. `trap ... ALRM` and
`trap ... CHLD` need no special handling because the MVP uses no timers and no
extra handlers (D7 uses a self-pipe read by the shell, not a handler-owned
timer).

`jobs`/`$!`/`wait`/`kill %n` work inside a request: the job table is the normal
one (`shell.rs:100-113`, `shell.rs:4308-4337`). `fg`/`bg` keep working as
"wait for"/"continue" because `set_foreground` is a no-op without terminal
control (`shell.rs:753-755`, `shell.rs:3650-3666`); there is no terminal
hand-off, documented as such.

### 6.4 `exec` (the builtin) in session mode

Rejected, explicitly, per §6.6. `exec_builtin` calls `exec::exec_now_plain`
(`shell.rs:1596-1612`) and would replace the interpreter mid-response. In session
mode the command finishes with status `126` and stderr
`sharky: exec: process replacement is not available in agent session mode`.
Redirect-only `exec` (`exec 3>f`, `exec >f`) still applies its actions in-process
(`shell.rs:1730-1745`) — that path never reaches `exec_now_plain` and must keep
working, because it is the normal way to persist a redirect.

### 6.5 Transport EOF

* EOF between requests → cleanup, exit with the last status (`shell.finish(last_status)`),
  `closed` is not written (nobody to write to).
* EOF while a request is in flight is legitimate (one-shot clients half-close):
  the request completes, its result is written, then EOF is observed on the next
  read and cleanup runs.
* EOF with an incomplete trailing frame → `PARTIAL_REQUEST`, no execution (D2).

### 6.6 Cleanup guarantees

One function, used by `close`, EOF, fatal signal and `PROTOCOL_FD_LOST`:

1. mark the session terminal so no new request runs;
2. for the in-flight/queued requests: no response is invented;
3. for every process group recorded for the session (D7): `SIGTERM`
   (`killpg`), wait up to `wait_ms` (default 2000 ms) polling with the same
   non-blocking wait used by the service hook, then `SIGKILL`;
4. `shell.finish(last_status)` → EXIT action;
5. `close` the control descriptors; exit.

"No leaked processes" is therefore a property of the group list, not of a hope:
D7 records every group the session created. Daemonizing children that call
`setpgid` themselves escape the group, which roadmap §7.2 explicitly refuses to
promise against; the doc says so.

---

## 7. D6 — `env_delta` and snapshots

* Before each `exec`, clone `shell.env` (the cheap, already-used pattern at
  `cli.rs:314`).
* After it, call the existing pure helper `env_delta(&shell, &before)`
  (`cli.rs:395-436`) — no fork, no temp file, no `write_delta_fd`
  (`cli.rs:447-459`). `attach_json_exec`/`publish_json_exec`
  (`shell.rs:375-383`) stay owned by the `--json -c` path and are not used by
  the session. This keeps one implementation of delta semantics for both modes.
* `_` is already excluded by that helper (`cli.rs:399-402`), so request noise does
  not appear as a change.
* Subshell and child-process changes never appear: they happen in a different
  `Shell` (`shell.rs:1671-1720`), and the parent's `env` is untouched — the
  contract in §6.7 holds without extra work.
* `env_delta:false` on a request suppresses the field entirely (privacy, §11).
* `state` is opt-in for anything sensitive: default returns `cwd`,
  `last_status`, `pid`, `jobs` summary and `options` only; `env`, `variables`,
  `functions`, `aliases`, `traps` require an explicit `include` entry. A full
  snapshot is by definition a secret-bearing artifact (§6.7, §11) and is never
  produced implicitly.
* No value is ever written to disk by the session; no log of commands or output
  is created. Presenting returned strings as data, not instructions, is a client
  responsibility that the protocol supports by keeping them inside quoted JSON
  fields (§11).

---

## 8. D7 — Cancellation and timeouts

### 8.1 What must change in the supervisor (minimum)

Four edits, all gated so REPL/batch behavior is bit-identical:

**S1 — request-scoped process groups.** New `Shell` field
`isolate_groups: bool` plus `set_isolate_groups(bool)`. In the three fork paths
that today gate `setpgid` on `self.interactive`, the guard becomes
`self.interactive || self.isolate_groups`:

* `spawn` parent (`shell.rs:2159-2168`) and child (`shell.rs:2170-2180`);
* pipeline members (`shell.rs:1044-1077`);
* `run_subshell` parent (`shell.rs:1671-1690`) and child (`shell.rs:1701-1712`).

`set_foreground`/`tcsetpgrp` remain behind `foreground_control()`
(`shell.rs:753-755`), so no terminal is touched. `run_background` already always
sets its own group (`shell.rs:872-896`) and needs no change.

**S2 — group recording.** `crate::agent::note_group(pgid)` is called from the
four places where a foreground group's identity is known (`spawn` parent,
pipeline leader branch, `run_subshell` parent, `run_background` parent). No-op
outside a session. The list is per session, capped (1024 groups), and is what
cleanup and `cancel` iterate.

**S3 — the wait hook.** `exec::wait_member` (`exec.rs:238-261`) blocks in
`waitpid(pid, WUNTRACED)`. In session mode it becomes:

```
loop {
    if agent::active() {
        agent::service();                       // pump control, enforce deadlines
        match waitpid(pid, WNOHANG|WUNTRACED|WCONTINUED) {
            StillAlive => poll([control_fd, child_pipe], timeout=50ms),
            other => return ...                  // same mapping as today
        }
    } else {
        /* existing blocking waitpid path, unchanged */
    }
}
```

`agent::active()` is one relaxed atomic load, so the REPL and every `-c`/batch
run take the untouched branch. `child_pipe` is a self-pipe that `note_child`
(`signal.rs:179-190`) writes one byte into in session mode, so a child exit wakes
the poll immediately and the 50 ms timeout is only a backstop — no per-command
latency is added. Stopped-child semantics are preserved by keeping the
`!self.foreground_control()` continue in `wait_foreground_member`
(`shell.rs:1655-1661`), which now loops over the new wait.

**S4 — child guard.** D3's `child_guard()` beside the eight
`reset_child_traps()` sites.

Nothing else changes: `wait_checked` (`exec.rs:192-204`) and `wait_scan`
(`exec.rs:261-273`) stay as they are, and the single-threaded ownership of reap
stays intact (roadmap §7.2's "two waiters" hazard is not introduced — there is
still exactly one thread that calls `waitpid`).

### 8.2 `cancel` semantics

* Targets an `id`.
  * in flight → record the intent, `killpg(current_pgid, SIGTERM)`, answer
    `ack{"outcome":"delivered"}` immediately, and let the in-flight `exec`
    finish with `termination:"cancelled"`, `status:143` (`128+SIGTERM`), whatever
    the command's actual exit is — cancellation is the reported cause.
  * queued (accepted, not started) → drop it and answer it later with
    `termination:"cancelled"`, `status:130`, empty streams, `duration_ms:0`.
  * already finished → `CANCEL_TOO_LATE`, no signal sent.
  * never seen → `CANCEL_UNKNOWN_ID`.
* Escalation: `SIGTERM` to the group; the deadline is
  `Instant::now() + grace_ms` (default `cancel_grace_ms` = 2000). Each
  `service()` call checks the deadline and sends `SIGKILL` to the group when it
  passes. Escalation is therefore driven by the wait loop the shell is already
  in — no threads, no timers, no signal handlers that could re-enter the
  interpreter.
* Scope: the **request's** group, never `getpgrp()` of the agent. This is the
  whole reason for S1; without it, a cancel would take the agent down with the
  command (§1 item 5).
* `timeout_ms` uses the same machinery with `termination:"timeout"` and
  `status:124` (the `timeout(1)` convention), and also kills the group — a
  deadline that only stops reading is not a deadline (§7.1).
* A second `cancel` for the same id is a no-op returning
  `ack{"outcome":"delivered"}` (idempotent), which matters for retrying clients.

### 8.3 Signals aimed at the agent itself

`Shell::new` installs fatal handlers for INT/TERM/HUP (`shell.rs:246-262`), so:

* `SIGTERM`/`SIGHUP` → the request unwinds at the next command boundary
  (`shell.rs:810-840` checks `fatal_signal`), the result is written, cleanup
  runs, exit `128+n`. Job groups receive TERM then KILL, as in §6.6.
* `SIGINT` → same path with 130. It is *not* mapped to "cancel and continue":
  `Shell::new` already treats INT as fatal, and pretending otherwise would make
  the agent's own signal behavior differ from every other mode.
* `SIGPIPE`: `Shell::new` sets `SIG_DFL` (`shell.rs:241-246`). Protocol writes
  therefore must not inherit it, or a dead client would kill the agent before it
  can reap its children. The writer blocks `SIGPIPE` with `sigprocmask` around
  the single `write(2)`, reads `EPIPE` as "client gone", consumes the pending
  signal with `sigtimedwait(0)`, and restores the mask — the standard POSIX
  idiom, no global disposition change, children keep `SIG_DFL`.

---

## 9. D8 — Module layout and the JSON-parsing question

### 9.1 Files

```
rust/src/agent/mod.rs        try_cli(), run_stdio(), Session driver loop, dispatch
rust/src/agent/protocol.rs   Request/Response structs, encoders, error codes
rust/src/agent/json.rs       strict minimal JSON value parser + base64 encoder
rust/src/agent/capture.rs    temp-file capture, dev/ino identity, truncation, encoding
rust/src/agent/supervisor.rs statics: in-flight request, group list, deadlines,
                             service(), note_group(), child_guard(), install()
```

`main.rs:1-21` gains one `mod agent;`. `cli.rs` gains the early dispatch
(`cli.rs:28-36` neighborhood) and `pub(crate)` visibility for `json_string`
(`cli.rs:525`) and `env_delta` (`cli.rs:395`) so the agent module reuses them
instead of duplicating escaping.

`shell.rs` additions (all small, all additive): `session_eval`,
`set_isolate_groups`, the `isolate_groups` field, snapshot accessors, the `exec`
rejection, the eight `child_guard()` calls, and `note_group` calls. `exec.rs`
gets the S3 branch. `signal.rs` gets the self-pipe write in `note_child` guarded
by a static fd. `help.rs` gets the `agent` topic.

The interpreter remains the only semantics source: the session drives `Shell`
exactly as the REPL does and adds no evaluator.

### 9.2 Request parsing: hand-rolled, and why

Emitting JSON is solved (`json_string`, `cli.rs:525-543`). Parsing requests is
new, and it must be *strict* — otherwise the defined-error taxonomy of D2 is
fiction.

| option | cost | verdict |
| --- | --- | --- |
| `serde` + `serde_json` | 3–4 crates in the shipped binary, a lockfile/flake hash change, and a dependency policy that today is `nix` + `regex-lite` + `sharky-config` (`Cargo.toml:15-19`) | rejected |
| `serde_json` as a **dev**-dependency for differential tests | still changes `Cargo.lock`/`cargoHash` and requires network at build time in the nix sandbox | rejected for the MVP; noted as an optional future feature |
| hand-rolled strict parser | ~300 lines + a frozen corpus; total control over error codes and depth/size limits | **chosen** |

Scope of the parser: one value per line (the framer already bounded the line),
depth ≤ 32, a single pass over bytes, strict UTF-8 validation first, `\u`
escapes including surrogate pairs, rejection of trailing bytes, control
characters in strings, leading zeros, and non-integer values where the schema
requires an integer. Duplicate keys: last wins (documented). Numbers are only
needed as `i64` (`timeout_ms`, `wait_ms`, `grace_ms`).

Base64 encoding (output only) is ~30 lines; decoding is needed only for
`stdin_encoding:"base64"` and is the same table in reverse.

Risk control: a frozen corpus in `rust/tests/agent_protocol.rs` with hand-written
expectations for each RFC 8259 example, each error code, and each boundary
(depth 32/33, 1 MiB/1 MiB+1, 256/257-byte ids). The corpus is the durable
artifact; a differential run against `serde_json` behind a non-default feature is
listed as a possible follow-up, explicitly **not** part of the MVP build.

---

## 10. D9 — Test strategy

All of it is plain Rust integration tests over pipes — no PTY, no fake server,
no external runner. The binary is reached through `env!("CARGO_BIN_EXE_sharky")`
like `tests/cli.rs:7-15`, with `SHARKY_CONFIG=/nonexistent/...` for isolation.
PTY (`nix::pty::openpty`, as in `tests/interactive.rs:41`) is used *only* for the
negative test that a TTY stdin is refused.

New files: `rust/tests/agent_session.rs`, `rust/tests/agent_protocol.rs`, and a
helper `rust/tests/common/agent.rs` (a `Session` struct: spawn with piped
stdin/stdout, `send(json)`, `recv() -> Value-ish line`, `recv_within(ms)` using
`nix::poll` on the child stdout, `close()`). Deadlock discipline in every test:
write one request, read one line; never read more lines than responses expected.

### 10.1 Roadmap §12.2 scenarios (the MVP gate)

| # | Scenario | Test shape |
| --- | --- | --- |
| 1 | `x=5` in A, `echo $x` in B → `5` | two frames on one connection |
| 2 | `cd /tmp` in A, `pwd` in B → `/tmp` | same, assert `cwd` in both results |
| 3 | declared function persists; failure status visible in B | `f(){ echo f; }` then `f`; `false` then `echo $?` → `1` |
| 4 | a response arrives **before** EOF of the channel | send one frame, keep stdin open, read the result, never close |
| 5 | `cat`/`read` do not consume the next request | `exec "read x; echo got:$x; read y"` with no payload → `got:` and `1` from the final `read y`; then `exec "echo next"` → `next`; also `exec "cat"` returns at once |
| 6 | builtin and external process do not print outside the framing | `exec "echo a; echo b >&2; /bin/echo c"`; assert every stdout line of the child process parses as JSON and `id` matches |
| 7 | invalid JSON, wrong version, duplicate id, partial EOF → defined errors, state intact | one frame per case, then `echo ok`; assert codes and survival (partial EOF expects exit 1) |
| 8 | `exit`/`exec`/`trap`/jobs documented semantics | `exit 7` → result 7 + `closed` + process exit 7; `exec sleep 1` → 126 + message; `trap 'echo bye' EXIT` then close → `bye` on the *stderr*/capture of the closing sequence asserted once; `sleep 5 &` then `jobs`/`wait` |
| 9 | sessions close cleanly without leaked processes | `sleep 30 & echo $!`, `close`, then `kill(pid,0)` → `ESRCH` within the grace window |

In every row `status` is the **last** command's status, per shell semantics.
A literal `exec "read x; echo got:$x"` therefore reports `0`, because it ends
in the successful `echo`; row 5 appends `read y` so the failing read's status
`1` is the last one and becomes visible. The property under test — that
`read`/`cat` never consume the control channel — holds for both forms.

### 10.2 §6.3/§6.5 P0 scenarios

* `exec "read line"` while three more frames are already queued in the pipe:
  the queued frames are still served in order (proves fd 0 is not the control
  channel even when data is pending there).
* `exec "cat"` with `stdin:"hello\n"` → stdout `hello\n`; the next frame is
  unaffected.
* Payload larger than a pipe (256 KiB, below `max_stdin_bytes` is false → use the
  configured limit in the test) proves the file transport cannot stall.
* `exec "printf '\\x00\\xff'"` → `stdout_encoding:"base64"`, decoded bytes equal.
* `exec "seq 1 200000"` with a small `--max-capture-bytes` →
  `stdout_truncated:true`, `stdout_bytes` = real size, response still valid JSON.
* Background job output never reaches the protocol: `exec "sleep 0.3 & echo go"`,
  wait 600 ms, then `exec "echo done"`; the child's stdout stream contains
  exactly four lines (ready + three results).

### 10.3 §7.1/§7.3 supervisor scenarios reachable in the MVP

* `exec "sleep 10"` then `cancel` after 100 ms → result within grace,
  `termination:"cancelled"`, `status:143`, and the session still answers
  `echo alive`.
* `exec "sh -c 'trap \"\" TERM; sleep 10'"` with `grace_ms:200` → escalates to
  SIGKILL; elapsed bounded by grace + scheduling slack.
* `exec "sleep 5"` with `timeout_ms:100` → `termination:"timeout"`, `status:124`.
* `exec "sh -c 'sleep 10 & wait'"` then cancel → the grandchild dies too
  (group scope), asserted via `kill(pid,0)` on the reported pids.
* 100 short requests in one connection → all `status:0`, no fd growth (compare
  `/proc/<pid>/fd` count before/after), no zombie (`ps` state check).

### 10.4 Regression guarantees

* `cargo test` (the existing 1239) unchanged and green — the gated branches must
  not alter any non-agent path. `tests/cli.rs`, `tests/job_control.rs`,
  `tests/interrupt.rs`, `tests/signal_lifecycle.rs`, `tests/trap.rs` are the
  tripwires for S1–S4.
* `tests/help.rs` covers the new `agent` topic.
* A `--json -c` envelope comparison test (existing fields and values) proves the
  `state`/`-c` contracts did not drift.

### 10.5 Performance (not a hard gate)

A documented script (not a test) records: startup `sharky -c true`, per-request
latency over 1000 `true` requests, 1 MiB round-trip, peak RSS, and fd count.
Numbers are published with the measurement conditions (§12.4 forbids
extrapolating from an empty benchmark). The durable dogfood harness for this
work is `scripts/agent-session-demo.sh` (manual demo, not part of `cargo test`);
its per-request table reports `duration_ms` alongside the wall-clock round trip.

---

## 11. D10 — Explicit non-goals and their seams

| Non-goal | Why now | Seam that keeps the protocol stable |
| --- | --- | --- |
| ~~Parallel executor (§8)~~ **Delivered (Fase D, U1–U6)** | was deferred to its own isolation policy; the delivered design keeps interpreter-copy isolation, not filesystem isolation | Shipped as `op:"parallel"`; envelopes carry opaque `id`s and per-result `status`/`termination`; `ready.protocol.ops`/`features`/`limits` advertise it. Contract and remaining non-goals: `docs/parallel-executor-design.md` |
| ~~Workflow engine (§9)~~ **Delivered (Fase E)** | depends on parallel | Shipped as `op:"workflow"` with a DAG scheduler, policies, retries, conditions and named locks; contract: `docs/workflow-executor-design.md` |
| ~~Code Mode (§10)~~ **Delivered (Fase F)** | depends on both | Client-side orchestration plus native `op:"code"` and `sharky code -e`; contract: `docs/code-mode-design.md` |
| Unix socket transport (§15) | stdio is enough for a local agent | delivered as `sharky agent --socket PATH`, sharing session setup with stdio; exact contract and limits below |
| ~~PTY per command (§11)~~ **Delivered (Phase G)** | initially deferred to avoid terminal noise by default | delivered as opt-in `"tty":true` on `exec`/`task`; advertises `"pty"` in `ready.protocol.features` |
| ~~Incremental/streaming output (§6.5)~~ **Delivered (Phase G)** | initially deferred to capture files | delivered as opt-in `"stream":true` on `exec`, emitting ordered `stdout_chunk`/`stderr_chunk` frames with monotonic `seq` prior to `result` |
| ~~Opt-in sandbox (§11)~~ **Delivered (Phase G)** | default session stays unsandboxed (`filesystem_sandbox:false`) | delivered as opt-in `"sandbox":true\|{...}` (`PR_SET_NO_NEW_PRIVS`, `RLIMIT_*`, optional user/mount/net namespaces failing closed); advertised as `"opt_in_sandbox"` |
| ~~MCP adapter & session resume~~ **Delivered (Phase G)** | native JSONL first, MCP added in `4.0.0-alpha` | `sharky agent --mcp` serves 10 JSON-RPC tools over NDJSON/Content-Length; `--state-file`/`--resume` persists session state atomically |
| ~~Multi-session per process~~ (socket transport, §15) | one `Shell`, one JSONL connection | socket stays single-client (`--socket PATH`); `--mcp` supports named sessions (`sharky_session_open`/`sharky_session_exec`/`sharky_session_close`) |
| Auth/TLS for the socket | stdio inherits the client's descriptors | delivered transport has no authentication beyond mode 0600; no TLS |

Documented threat model, in the doc and in `help agent`: the client is trusted
(it can already `kill` the agent); *commands* are not trusted with the protocol
channel and are structurally unable to write to it (D3/D4); shell context
isolation is not filesystem isolation (§11 and §4).

The op set is now larger than this table: `parallel`, `workflow`,
`read_capture` and `code` carry their own response types. Consult
`sharky help <op>` and the linked design documents for those envelopes. The
core `result`, `state`, `ack`, `closed` and `error` shapes below remain the
MVP contract.

---

## 15. Unix socket transport (Phase G, first IPC step)

**Delivered.** `sharky agent --socket PATH [--no-config] [--redact-env]
[limit flags]` is an alternative transport for the same JSONL session as
`--stdio`. Exactly one transport is required; `--stdio` and `--socket` are
mutually exclusive (exit 2 with a usage error). Existing `--stdio` scripts keep
their behavior.

Contract:

1. PATH must be non-empty and fit in `sun_path` (107 usable bytes on Linux).
   An existing path, including a stale socket, is a hard error (exit 2); the
   agent never deletes a file it did not create. The bound socket is mode 0600.
2. The server accepts **one** client, then immediately unlinks the path and
   drops the listener. A second connection attempt fails; there is no queue of
   clients and no daemon-style rebinding.
3. The accepted socket description is duplicated twice with `F_DUPFD_CLOEXEC`
   (both copies read and write the same connected socket). Identities are
   recorded, 0/1/2 are isolated to `/dev/null`, the supervisor is installed,
   and the same `Session::serve` loop used by stdio answers `ready`, requests
   and terminal frames. No evaluator, request handling or state logic is
   duplicated.
4. Client EOF, `close`, a fatal session error and SIGINT/SIGTERM/SIGHUP end the
   process with the stdio status semantics. The path is unlinked on all exit
   paths after bind, and the session cleanup still terminates recorded child
   groups before the process leaves. Diagnostics go to the real stderr before
   descriptor isolation.

Limitations: socket transport adds no authentication or TLS beyond mode 0600;
clients are trusted just like `--stdio`. As of `4.0.0-alpha`, PTY (`"tty":true`),
incremental streaming (`"stream":true`), cross-session advisory `flock`
(`"lock"`), opt-in `sandbox`, session resume (`--state-file` / `--resume`), and
the optional native MCP stdio server (`sharky agent --mcp`) are all implemented.

---

## 12. Risks and open questions

1. **Riskiest:** S3 (the wait hook). Replacing the blocking `waitpid` in
   `exec::wait_member` with `WNOHANG` + `poll` touches the one place where the
   interpreter's reap ownership and the job table meet. The hazard is a status
   consumed twice (roadmap §7.2) or a missed stop/continue. Mitigations: the
   non-agent branch stays byte-identical; the loop keeps the exact
   `!foreground_control()` stop handling; `tests/job_control.rs`,
   `tests/signal_lifecycle.rs`, `tests/hazards.rs` and `tests/interrupt.rs` are
   the gate; and the self-pipe write in `note_child` must be one
   `write(2)` to an `O_NONBLOCK` descriptor (async-signal-safe, and `EPIPE`/`EAGAIN`
   ignored) or a full pipe would silently swallow the wake-up.
2. **Second:** the identity-checked fd restore (§4). If the check mis-detects a
   user redirect as ours, a persistent `exec >log` would be undone. Reverse case
   (ours detected as user's) leaves fd 1 on a closed file and loses output — the
   `INTERNAL` guard must compare *both* dev and ino and must run before the file
   is closed.
3. `exec 900>file`-style collisions are survivable but fatal by design
   (`PROTOCOL_FD_LOST`). A client that hits it gets a defined stop. Accept.
4. `set -e`/`exit` as session-terminal is a product decision, not a technical
   necessity. If a client needs a resilient session, the place to add it is a
   request field (`"stop_on_error":false`) with explicit semantics, never a
   silent change of `exit`.
5. Config load errors reach the client's stderr only (`load_config` uses
   `eprintln!`, `shell.rs:395-399`) and precede `ready`. If a client wants them,
   a `warnings` array on `ready` is additive.

---

## 13. Implementation order (for the implementing workers)

1. `agent/json.rs` + `tests/agent_protocol.rs` (pure, no shell code).
2. `agent/capture.rs` + temp-file identity helpers (pure, unit-testable).
3. `agent/mod.rs` + `session.rs` driver: `ready`, `exec` without stdin payload,
   `close`, EOF, error taxonomy; JSONL loop over an in-process `Shell` with
   `session_eval` and no `finish`.
4. fd isolation (D3/D4) and the `read`/`cat` gate; base64; truncation.
5. `state` + `env_delta` + snapshot accessors.
6. S1/S2 (groups) then S3 (wait hook + self-pipe) then `cancel`/`timeout`, each
   with its §7.1 test in the same change.
7. `help agent`, `docs/` cross-links, performance script.

Each step lands with its tests and must keep the full suite green; the design's
own gate is §10.1 rows 1–9 plus §10.2's P0 rows.

---

## 14. Recommended MVP cut (as originally scoped)

The bullets below are now historical: they record what the Fase C cut left
out. The implemented result is described at the top of this document and in
`docs/roadmap-status.md`.

Steps 1–6 and 7's `help agent` shipped. Originally cut from the MVP, without
touching the protocol:

* streaming and any drainer thread (files only);
* `state` returns every requested section: `include` accepts `env`,
  `variables`, `functions`, `aliases`, `traps` and `jobs`. Sensitive sections
  are still opt-in; `--redact-env` masks their values.
* job notices as data are still not implemented ("Running"/"Stopped" notices
  go to the isolated stdout between requests).
* `wait_ms` is decoded; cleanup has a fixed default of 2 s.

Keep in the MVP because the Fase C gate names them: request IDs, persistent
state, separated stdin/stdout/stderr, typed errors, `env_delta`, `state`,
`cancel` with escalation, `timeout`, and cleanup without leaked processes.
Those features are implemented; the socket transport and the remaining Phase G
capabilities (`--mcp`, PTY, incremental streaming, cross-session `flock`,
opt-in OS sandbox, and `--state-file` session resume) landed by `4.0.0-alpha`.
