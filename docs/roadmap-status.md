# Roadmap status

Scope: the master plan at `SHARKY_AGENT_RUNTIME_CODE_MODE_ROADMAP.md`,
checked against this `4.0.0-alpha` working tree and the release binary rebuilt
from it. This is a status snapshot, not a restatement of the roadmap.
"Delivered" means the surface exists in this tree; it does not mean every item
in a roadmap checklist has an independent acceptance test, or that a phase
gate is closed.

## Phases

| Phase | Status | Evidence in this tree |
| --- | --- | --- |
| A: semantic safety (P0) | Core fixes delivered; gate evidence partial | Core fixes for `set -C`, `readonly`, `$!`/`wait`, `PIPESTATUS`, `source`/`return`, builtin flags, scopes and redirections are in `rust/src/`. All six advertised redirection forms (`|&`, `>&-`, `<&-`, `<&N`, `>&N`, `>|`) are accepted by the lexer/parser, and `|&` matches bash for simple commands, groups and explicit-redirection cases (`fc7f1c7`, `2787273`, `rust/tests/redirect_parity.rs`). Regression tests include `rust/tests/noclobber.rs`, `readonly.rs`, `job_control.rs`, `bash_parity.rs`, `pipestatus.rs`, `source_return.rs` and `redirect_parity.rs`. `rust/conformance/cases/` has 82 Ruby-oracle cases. Non-UTF-8 environment names, values and cwd now warn once on the real stderr before agent stdio isolation; conversion is still lossy, not a lossless byte contract (`rust/tests/non_utf8_honesty.rs`). Heredocs, captures and `<(...)` FIFOs share the `TMPDIR` -> `/tmp` -> `/var/tmp` -> cwd fallback, skipping empty values (`rust/src/agent/capture.rs` `temp_dirs`, `rust/src/shell.rs`). The full 300-case differential import and fuzz gate are not reproduced here. |
| B: UX / compatibility (P1/P2) | Core surface & Fish-like UX delivered; optional integrations deferred | `capabilities --json`, `agent guide --json`, `completions list --json`, `help TOPIC --json` (including structured `flags`), `z`/`zi`, history search/delete/merge, Starship prompt integration and the line editor are present. Fish-like interactive UX includes prefix-filtered `Up`/`Down`, word-by-word autosuggest acceptance (`Alt+Right`/`Ctrl+Right`), multi-position command abbreviations, live `Role::Error` command highlighting, interactive `autocd` (`shopt autocd` / `tide autocd`), `"did you mean"` typo hints on status 127, directory-only `cd`/`pushd` and case-insensitive `Tab` completion, and `{cwd_short}`/`{git}`/`{duration}`/error `{symbol}` prompt placeholders (`rust/tests/interactive.rs`, `rust/src/{editor,highlight,prompt}.rs`). All roadmap builtins are in `rust/src/shell.rs` dispatch and help: `pushd`, `popd`, `dirs`, `ulimit`, `hash`, `let`, `disown`, `times` and `declare -F` (`rust/tests/builtins_extra.rs`). Deferred outside this scope: direnv hook, zoxide database compatibility, fzf integration, advanced import/export, plugin hooks, right prompt and Alt-C. The full TTY/login integration gate is not claimed closed. |
| C: Agent Runtime MVP | Core surface delivered; gate evidence partial | `sharky agent --stdio` serves one JSONL session with request IDs, persistent shell state, per-request stdin, isolated stdout/stderr, typed errors, cancellation, deadlines, bounded captures and `state` snapshots. Tests include `agent_gate.rs`, `agent_session.rs`, `agent_protocol.rs` and `agent_capture.rs`. `sharky help agent` and `sharky agent guide --json` document the contract. The full §12.2 session matrix is not reproduced here. |
| D: Parallel Executor | Core surface delivered; gate evidence partial | `op:"parallel"`, isolated interpreter copies, concurrency limits, per-task cancellation, fail-fast/wait-all policies and independent captures. Tests: `agent_parallel.rs`; design: `docs/parallel-executor-design.md`. The full §12.3 stress matrix is not reproduced here. |
| E: Workflow Engine | Core surface delivered; gate evidence partial | `op:"workflow"`, DAG dependencies, retries with backoff, conditions, failure policies and named locks. Tests: `agent_workflow.rs`; design: `docs/workflow-executor-design.md`. |
| F: Code Mode | Core surface delivered; gate evidence partial | Native `op:"code"` and `sharky code -e`, with bounded scripts, loops, branches, projections, capture paging and array transforms (`reduce`, `group_by`, type-checked `sort_by`). Tests: `agent_code.rs` and `agent_codemode.rs`; design: `docs/code-mode-design.md`. |
| G: advanced integrations | Core surface delivered (`single-client socket` deliberate limit) | Delivered: `sharky agent --socket PATH` (mode 0600, single client, immediate unlink), per-request `"tty": true` PTY support, `"stream": true` chunk streaming with monotonic `seq`, cross-session OS advisory `flock` mutexes (`"lock"`), opt-in `"sandbox"` containment (`PR_SET_NO_NEW_PRIVS`, `RLIMIT_*`, optional Linux namespaces), `--state-file`/`--resume` JSON session persistence, and native stdio MCP server (`sharky agent --mcp`, 10 tools over NDJSON and `Content-Length` framing). Verified by `rust/tests/agent_phase_g.rs` (10 tests). Single-client socket transport remains a deliberate limit; multi-client daemons and network TLS are out of scope. |

## Cross-cutting items

| Item | Status | Notes |
| --- | --- | --- |
| Structured discovery | Delivered | `capabilities --json` (with `onboarding` recipes, `mcp_tools`, `redirection_forms`, `agent_transports`, `cross_session_locks`, `opt_in_sandbox`), `agent guide [--json]`, `completions list --json`, `help TOPIC --json` (with `flags` schemas), `sharky agent --help`. |
| Process supervision | Delivered for agent requests | Process groups, TERM to KILL escalation, reap and cleanup: `rust/src/agent/supervisor.rs`, `agent_supervisor.rs`, `agent_gate.rs`. |
| Versioned protocol and typed errors | Delivered | `schema_version` 1, typed request/response values, stable error codes in `rust/src/agent/protocol.rs` and `agent_protocol.rs`. |
| Non-UTF-8 paths and arguments | Partial; warnings delivered | Invalid UTF-8 in environment names/values, cwd and command-line arguments warns once without echoing bytes, including on the real stderr of an agent session before stdio isolation. JSON switches a stream to base64. Lossless round-trip is not claimed. |
| `TMPDIR` fallback for temp files | Delivered | `rust/src/agent/capture.rs` `temp_dirs` tries non-empty `$TMPDIR`, then `/tmp`, `/var/tmp` and cwd. Heredocs, capture files and `<(...)` FIFOs share it (`rust/tests/builtins_extra.rs`). |
| Missing roadmap builtins | Delivered | `declare -F`, `$(<f)`, `read` flags, `jobs -p/-l`, `pushd`, `popd`, `dirs`, `ulimit`, `hash`, `let`, `disown` and `times` are present in `rust/src/shell.rs` dispatch and `capabilities --json`. |
| Sandbox and lock guarantees | Delivered (opt-in) | Default `exec` runs with caller privileges; `"sandbox": true` or explicit config enforces `PR_SET_NO_NEW_PRIVS`, `RLIMIT_*` and optional Linux user/mount/net namespaces (failing closed on setup errors). `"lock"` provides cross-session advisory `flock` exclusion. |
| Interpreter performance | Optimized (~2x faster than bash on core loops) | Eliminated unnecessary `child_env()` builds, empty redirect action vectors, literal `expand_word` clones, unquoted `glob_fields` re-allocations, front-to-back scans on tail-appended `self.env` variables (`var`/`set_var_unchecked`/`remember_underscore`), redundant keystroke allocations in `autosuggest`/`highlight`, and `starship` busy-wait polling. Observed 15-run release medians on this host: startup **1.18 ms** (bash 2.66 ms, fish 3.24 ms), `[[ ]] + ((i++)) x1000` **2.05 ms** (bash 4.51 ms), `while [ $i -lt 1000 ]` **2.75 ms** (bash 5.91 ms, fish 38.00 ms), `1000 func+local+case` **5.89 ms** (bash 11.87 ms, fish 79.66 ms), and `200 $(printf a)` **73.13 ms** (bash 83.72 ms). |
| Dogfooding and measurement | Evidence recorded | Both in-repo benchmarks exited 0 against `rust/target/release/sharky`; observed figures below. |

## Dogfooding evidence

The figures below are wall-clock observations from `SHARKY_BIN` pointing at
`rust/target/release/sharky` on this host (`mafu-nix`, Linux `7.2.15-zen1`
`x86_64`). They are not CI gates and will vary with hardware and load.

- `scripts/agent-dogfood-bench.sh` (exit 0): for 24 hostile filenames, the
  bash baseline used 25 round trips and 101.1 ms with 0/24 correct (24 quoting
  failures). One `op:"code"` turn used 1 round trip and 23.1 ms (4.37x faster)
  with 24/24 correct and 25 audit calls. A 5000-line, 234423-byte log projected
  to 815 bytes on the wire (287.6x reduction), with `read_capture` windows of
  931/945/945 bytes. Descriptors stayed `8 -> 8` over 30 projected execs plus 3
  `read_capture` calls; `capabilities --json` matched `ready.protocol` (`ops=8`,
  `features=26`).
- `scripts/agent-dogfood-robustness.sh` (exit 0): cancel returned status 143
  in 201.8 ms with 0 survivors. An unbounded `yes` producer emitted
  2223437559 raw bytes reduced to 135 bytes on the wire (`wall=1420.5 ms`). A
  20971720-byte binary was delivered as 4096 bytes of base64, and a 30000-line
  (1550170-byte) log projected to 245 bytes. All 10 bash differential cases were
  equal, 4 consecutive failures were followed by clean execution and `state`
  inspection, 18-byte stdin framing was exact, 3 isolated execs and 3 parallel
  cwd overrides left parent cwd/env intact, descriptors stayed `8 -> 8`, and
  zombies stayed `0 -> 0`.

## Remaining gaps

Non-UTF-8 inputs remain lossy (warned once), single-client socket remains by
design, the full 300-case upstream differential inventory is not imported, and
optional ecosystem hooks (`direnv`, `zoxide` db, `fzf` integration, right
prompt, `Alt-C`) remain out of scope for `4.0.0-alpha`.

## Evidence commands

```sh
sharky --help
sharky agent --help
sharky agent guide --json
sharky capabilities --json
sharky completions list --json
sharky help agent --json
cargo test -j 2 --workspace --no-fail-fast
SHARKY_BIN=/path/to/release/sharky bash scripts/agent-dogfood-bench.sh
SHARKY_BIN=/path/to/release/sharky bash scripts/agent-dogfood-robustness.sh
```

This file records source, test and benchmark facts observed in this working
tree, not a claim that historical roadmap checklists are completely closed.
