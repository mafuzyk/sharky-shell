# Changelog

All notable changes to sharky are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses semantic
versioning. sharky is in alpha: version numbers mark releases, while alpha is
the project's ongoing maturity, delivered through the normal release channel
rather than a separate preview track.

## [Unreleased]

## [0.1.0-alpha.1] - 2026-11-24

### Added

- **Version Reset & Packaging Baseline (`epoch=1`)**:
  - Reset release numbering from `4.0.0-alpha.5` to `0.1.0-alpha.1` as the clean alpha baseline (`epoch=1` on AUR so `pacman`/`yay`/`paru` upgrade cleanly).
- **Bash 4/5 Parity**:
  - Call stack introspection and execution metadata (`BASH_SOURCE`, `FUNCNAME`, `BASH_LINENO`/`LINENO` per AST line, `caller`), working-directory and dispatch builtins (`cd`, `pwd`, `source`, `builtin`), regex/system variables (`BASH_REMATCH`, `RANDOM`, `SECONDS`, `BASHPID`, `UID`, `EUID`), `shopt` options, CLI flags, `trap ERR`/`RETURN`, caret syntax errors, full `extglob`, `declare -n/-i/-l/-u`, parameter transformations `${var@Q/@U/@u/@L/@A/@a}`, and `coproc` with dynamic FD duplication.
- **Agent Runtime & MCP Hardening**:
  - Normalized stringified MCP arguments and defined array schema items in tool schemas.
  - Added structured `output.parse`, `fs_delta`, `sharky_snapshot`/`sharky_restore`, `sharky_job_start`/`poll`/`send`/`stop` with `wait_until`, `--policy <file>`, and `--socket --multi-client`.
- **Interactive UX (Zsh / Fish / Nushell)**:
  - Zsh-compatible `precmd`/`preexec`/`chpwd` hooks and `complete`/`compgen` Tab integration.
  - Fish-style pager descriptions, `Alt+Left`/`Alt+Right` directory history navigation, and `prevd`/`nextd` builtins.
  - Nushell-inspired structured pipes (`from-json`, `to-json`, `where`, `select`, `table`).

## [4.0.0-alpha.5] - 2026-11-22

### Fixed

- **Terminal Resize (`SIGWINCH`) Prompt Redraw & Narrow `{fill}` Handling (`editor.rs`, `prompt.rs`, `repl.rs`, `signal.rs`, `term.rs`, `shell.rs`)**:
  - Handled `SIGWINCH` without `SA_RESTART` and added it to `ppoll` waiting (`wait_for_input`) so the interactive editor wakes immediately on terminal resize, computes reflowed upper prompt lines plus typed-buffer cursor rows, clears upward cleanly, and re-expands `{fill}` for the new column width without duplicating prompt rows or losing typed input (`trap - WINCH` restores the watcher and forked children reset `SIGWINCH` to `SIG_DFL`).
  - Omitted the right-hand segment of `{fill}` prompt lines when `left + 1 + right > columns` so narrow splits keep the left segments intact instead of wrapping mid-pill, restoring the right side automatically when widened.

## [4.0.0-alpha.4] - 2026-11-22

### Added

- **Declarative Prompt Engine Primitives (`prompt.rs`, `editor.rs`, `sharky-config`)**:
  - Added `reverse` / `invert` style modifier mapping to SGR `7`, enabling powerline/inverted badge transitions natively in `theme.sharky`.
  - Added `\uXXXX` and `\u{XXXXXX}` Unicode escape decoding in quoted theme strings (allowing single-quoted or double-quoted Nerd Font glyphs without raw PUA bytes in config files), plus `wave prompt.os.glyph` for OS icon overrides.
  - Added `{fill}` / `{fill:<ch>}` terminal-width alignment tokens across top-row prompt lines (computed against `visual_width`), plus `{os_icon}` (`/etc/os-release`), `{cwd_home}`, `{duration_ms}`, `{shell}`, and `{mem}` (`/proc/meminfo`).

### Fixed

- **Quoted-Directory Globs, Directory-Only Slashes, and Mixed Brace Expansion (`shell.rs`)**:
  - Stripped internal `QUOTED_GLOB_MARKER` (`\u{1}`) escapes from non-glob directory prefixes in `glob()` and handled quoted characters inside `[...]` classes in `glob_chars()`, fixing `mv "$d"/*.mkv` and similar globs under quoted directories containing `( ) [ ] + @ ! * ?`, backslash escapes, or `~` expansions.
  - Supported trailing-slash directory-only globs (`*/`, `"$d"/*/`, `*/*/`, `*//`) matching real directories and directory symlinks while preserving trailing slashes.
  - Lifted brace expansion to operate on structured `Word` parts before parameter expansion, tilde revelation (`{~,foo}`), word splitting, and globbing (enabling `"$d"/*.{mkv,txt}`, `"$f"{,.bak}`, `{"$d"/*.mkv,"$d"/Season*/*.mkv}`, zero-padded numeric ranges like `{01..03}` / `{-01..02}`, and `{foo}{a,b}`), while keeping quoted or variable-expanded braces literal.

## [4.0.0-alpha.3] - 2026-11-22

### Added

- **Native Prompt System v2 (`prompt.rs`, `sharky-config`)**:
  - Rich `StyleSpec` for `wave` values and inline `{style:content}` spans, supporting `@colour` or `#RRGGBB`, optional `on @bg` / `on #RRGGBB` backgrounds, and `bold`, `dim`, `italic`, `underline` attributes without SGR bleed (`selection` and syntax styles also support rich specs).
  - Template Engine v2 adding conditional `[...]` groups, backslash escapes, configurable `wave prompt.symbol.glyph` / `wave prompt.error.glyph`, POSIX `gethostname`/`USER`/`LOGNAME`/UID fallbacks, and new tokens (`{cwd_full}`, `{git_dirty}`, `{venv}`, `{nix}`, `{ssh}`, `{jobs}`, `{time}`, `{error_status}`).
  - Built-in zero-cost presets activated with `wave preset <name>`: `sharky`, `neolambda`, `pure`, `robbyrussell`, and `powerline`, materialized as a base layer so user `coral`/`wave` rules always override regardless of order.
  - Right prompt (`wave rprompt "..."` and `tide rprompt on|off`) rendered right-aligned in the line editor with a 2-column safety margin and automatic hide on input overflow.
  - Discovery topics `sharky config explain presets` and `sharky config explain prompt`.
- **Oh My Posh Support**:
  - Added `tide prompt native|starship|omp` and `tide omp_config "<path>"` (preserving `tide starship on`), running `oh-my-posh print primary|right --shell=universal --escape=false` over a unified non-blocking `O_NONBLOCK` + `poll` runner with fallback to native prompts and validation in `sharky doctor`.

## [4.0.0-alpha.2] - 2026-11-21

### Fixed

- **Fork-Isolated Tasks Under MCP & `sharky code` CLI (`EBADF`)**: Fixed `serve_mcp` and `run_code_cli` passing `null_fd` as the supervisor's control output descriptor (causing `supervisor::child_guard()` to close fd 3 before `dup2` onto stdin). Added `supervisor::close_in_children` so MCP stdio and dummy pipe ends stay isolated from forked tasks without closing `null_fd`.
- **`sharky agent guide` & `help agent` Alignment**: Fixed `exec_pty_stream_sandbox` recipe `r3` to use real protocol keys (`max_file_bytes`, `cpu_seconds`), clarified `state` always-present fields (`cwd`, `last_status`, `options`, `pid`) vs `include` sections (`env`, `variables`, `functions`, `aliases`, `traps`, `jobs`), and documented all five `sandbox` keys (`no_network`, `read_only_root`, `max_file_bytes`, `max_procs`, `cpu_seconds`). Added protocol decoding tests for all `agent guide` recipes.
- **Code Mode & MCP Ergonomics**: Enabled automatic `isolated: true` defaulting for `sh(cmd, {cwd, env})` matching `run()`, added descriptions on `env` and `output` properties in the MCP tool schema, and accepted `--no-config` as a no-op on `sharky status` and `sharky doctor`.
- **Shell Parity (`[[ ]]`, `coproc`, `wait`)**: Shared POSIX file-type/permission evaluation (`access(2)`, `-S`, `-p`, `-b`, `-c`, dangling-symlink `-e`, `-nt`/`-ot`/`-ef`) and added `-v` and `-o` to `[[ ]]`; rejected unquoted `coproc` with syntax error exit `2`; updated `wait` error usage text to `usage: wait [id ...]`.

## [4.0.0-alpha] - 2026-11-21

### Added

- **Agent Onboarding & Discovery**:
  - `sharky agent guide [--json]` outputs a machine-readable and plain-text onboarding guide covering CLI entrypoints, golden rules, ops, Phase G options, Code Mode functions/methods, and native MCP tool schemas before shell startup.
  - `sharky capabilities --json` adds `onboarding` recipes and `mcp_tools`, and dynamically reflects `cross_session_locks` and `opt_in_sandbox`.
  - `sharky completions list --json` prints machine-readable builtins, CLI subcommands/topics, configuration topics, agent ops, Code Mode helpers, and MCP tools.
  - `sharky help [TOPIC] --json` now extracts a structured `flags` array (`{"flag", "takes_value"}`) alongside topic prose.
- **Native MCP Stdio Server (`sharky agent --mcp`)**:
  - Optional JSON-RPC 2.0 MCP transport supporting both line-delimited JSON (NDJSON) and `Content-Length` framing over stdio while keeping child stdio isolated on `/dev/null`.
  - Exposes 10 tools (`sharky_capabilities`, `sharky_exec`, `sharky_session_open`, `sharky_session_exec`, `sharky_parallel`, `sharky_workflow`, `sharky_code`, `sharky_read_capture`, `sharky_cancel`, `sharky_session_close`) over persistent named sessions or the default session.
- **Phase G Agent Runtime Integrations**:
  - Per-request pseudo-terminal allocation via `"tty": true` (`raw`, non-echo `termios`, `24x80` window, non-blocking PTY master draining, keeping non-isolated session `cwd`/`env` updates intact).
  - Incremental output streaming via `"stream": true`, emitting ordered `stdout_chunk` and `stderr_chunk` frames with monotonic `seq` prior to the terminal `result`.
  - Cross-session OS advisory `flock` mutexes (`"lock": "<name>"`) across concurrent `exec` requests and `workflow` DAG tasks.
  - Opt-in per-request OS containment via `"sandbox": true | {no_network, read_only_root, max_file_bytes, max_procs, cpu_seconds}` (`PR_SET_NO_NEW_PRIVS`, `RLIMIT_*`, and optional Linux user/mount/net namespaces failing closed on setup error).
  - Session state persistence and resumption across restarts via `--state-file PATH` / `--resume PATH` (atomic `0600` JSON snapshot of `cwd`, exported `env`, scalar variables, and `aliases`).
- **Fish-like Interactive UX**:
  - Prefix-filtered `Up`/`Down` history navigation that returns cleanly to the typed draft and resets when the buffer is edited.
  - Word-by-word autosuggestion acceptance on `Alt+Right`, `Ctrl+Right`, and `Alt+F`, alongside `Alt+Left`/`Ctrl+Left` and `Alt+B` word motion.
  - Multi-position abbreviations (`abbr`/`shoal`) in command positions after `;`, `|`, `|&`, `&`, `&&`, `||`, `(`, `{`, newlines, and wrappers (`sudo`, `command`, `builtin`, `exec`, `time`, `nohup`, `env`), plus expansion when followed by whitespace while remaining inactive inside quotes and regular arguments.
  - Live command-position `@error` syntax highlighting (`Role::Error`) via `Shell::classify_command_word` with a bounded `highlight_cache` invalidated on `PATH` changes, `hash -r`, and `reload`.
  - `autocd` enabled interactively by default (`tide autocd` and `shopt -s/-u autocd`), plus interactive `"did you mean '...'"` typo hints on status 127.
  - Smarter `Tab` completion: lists current directory entries on an empty non-command word, limits `cd`/`pushd` arguments to directories (including directory symlinks), supports multi-segment `git checkout`/`switch`/`merge`/`rebase` branch lookup, and falls back to case-insensitive matching while replacing the active token cleanly.
  - Prompt placeholders `{cwd_short}`, `{git}` (zero-subprocess `.git/HEAD` lookup), `{duration}` (>= 1s), and error-colored `{symbol}` (`prompt.error` / `error`).
- **4.0.0-alpha Version Bump**: Bumped crate (`rust/Cargo.toml`, `Cargo.lock`), NixOS/Home Manager definitions, man page, and PKGBUILDs to `4.0.0-alpha`.

### Changed

- **Shell Speed Optimizations**: Added lazy child environment materialization (`ensure_child_env`), no-redirect fast paths in `run_assignments_inner` and `with_actions`, literal fast path in `expand_word`, zero-alloc `glob_fields` when unquoted, allocation-free `has_glob` bracket checks, reverse-tail scanning for `self.env` variable lookups/updates and `$_` (`remember_underscore`), borrowed history metadata lookup (`History::entry_meta`) checked before disk stats in `autosuggest`, direct reference lookup in `highlight::render`, and `libc::poll` deadline waiting in `starship prompt`.

### Fixed

- `profiles::login()` now re-runs `discover_profile_paths()` after profile evaluation when the session has not exited, preventing `/etc/profile` from clobbering discovered startup paths.
- Hardened descriptor-inheritance tests (`cli`, `agent_session`, `agent_socket`) against parent-leaked high descriptors (`>= 3`), and updated the stale `let` builtin doc comment in `rust/src/shell.rs`.

### Previously Unreleased (now included in 4.0.0-alpha)

### Changed

- Heredocs, captures and process-substitution FIFOs now share the same temporary-directory fallback list (`TMPDIR`, `/tmp`, `/var/tmp`, cwd). A bogus `TMPDIR` no longer aborts capture creation: the first usable directory is used instead.

### Fixed

- Code Mode `sort_by(fn)` now validates that every closure key is a number or every key is a string before sorting. Mixed key types (and null/bool keys) fail loudly with a structured runtime error instead of producing a non-transitive ordering, while `sort` and homogeneous `sort_by` calls keep their existing behavior.
- `command -p` now execs the program found through the default PATH; it no longer re-resolves it through the current PATH in the child or reuses a cache entry from a different path set.
- Non-UTF-8 environment variables, environment variable names and working directories now warn on the real stderr in `sharky agent` sessions, before descriptor isolation; command-line argument warnings stay in the CLI startup path.
- Fixed a per-session `/dev/null` descriptor leak in `sharky agent --stdio`: the stdio startup path no longer opens a second null descriptor that the shared session setup never used.

### Added

- `sharky capabilities --json` now discloses `redirection_forms` (`|&`, `>&-`, `<&-`, `<&N`, `>&N`, `>|`) and `agent_transports` (`stdio`, `socket`). These are additive fields; `schema_version` remains 1 and all existing fields keep their names.
- `disown` builtin with `-a`, `-h` and `-r`, removing jobs from the table and from hangup signalling, or keeping them listed with `-h`, with loud errors for unknown options and missing jobs.
- `let` and `times` builtins: `let` evaluates arithmetic expressions with assignment and bash-compatible statuses; `times` reports shell and waited-for child CPU time in bash's two-line format.
- Heredocs and here-strings now fall back from an unusable `TMPDIR` to `/tmp`, `/var/tmp` and the current directory, keeping unlinked temporary files safe when the preferred directory is missing.
- `hash` builtin with a PATH-derived command cache shared by parent and child commands, supporting `hash -l/-t/-d/-r/-p`, explicit names, and loud errors for missing or invalid entries.
- `ulimit` builtin backed by `nix::sys::resource`, with soft/hard selection,
  resource flags, `unlimited`, `-a` listing and loud errors for bad options.
- `pushd`, `popd` and `dirs` builtins backed by `DIRSTACK` and `PWD`, including
  `cd -` semantics, rotation by `+N`/`-N`, `dirs -c/-p/-l/-v`, and loud errors
  for empty or out-of-range stacks.

- Code Mode arrays now support `reduce(init, fn)`, `sort_by(fn)` and
  `group_by(fn)` as pure, bounded transforms. They aggregate child results in
  one turn, charge one step per element, keep the existing `audit` envelope, and
  add no process-spawning builtins.
- `sharky agent --socket PATH` serves the same JSONL session protocol over one
  Unix stream socket as an alternative to `--stdio`. The socket is bound with
  mode 0600, accepts exactly one client, is unlinked immediately after accept
  and on every exit path, and never replaces an existing path. Both transports
  share the same descriptor isolation, supervisor and session loop. This is a
  local transport only: it adds no sandbox, authentication, PTY or multi-client
  support.
- `sharky agent --stdio` session protocol with persistent shell state,
  descriptor isolation (`fd >= 900`, unlinked stream captures restored by file
  identity), process-group supervision (`cancel`, `timeout_ms`), base64 binary
  stream support, `state` inspection, `parallel` multi-task execution
  (`wait-all`/`fail-fast`, per-task cancel and output budget), and `workflow`
  DAG scheduling (`depends_on`, `failure_policy`, non-blocking deadline-based
  `retry`, parent-side `condition` predicates, and intra-request named `lock`s),
  documented under `sharky help agent`, `parallel` and `workflow`. The same
  protocol now also carries typed `argv`, isolated `exec`, environment
  redaction, runtime `output` projection, `read_capture` and `code`, all under
  the existing `schema_version`, which did not change.
- Agent `exec`, `parallel.tasks[]` and `workflow.tasks[]` accept a typed
  `argv` array as a mutually exclusive alternative to `command` (exactly one
  is required), resolving `argv[0]` and calling `execve` directly, so nothing
  is parsed, globbed or expanded and each argument reaches the child byte for
  byte. `exec` gains `isolated`, which evaluates a single command in a forked
  copy-on-write child, so `cd`, assignments, option changes and `exit` cannot
  mutate or terminate the parent session. `sharky agent --stdio --redact-env`
  masks secret-looking variables (tokens, passwords, keys and similar names)
  in `state` and in the `env_delta` of every response while still showing
  whether a variable was set or removed. `output` projects `stdout` and
  `stderr` with `strip_ansi`, `grep` plus `context`, `head_lines`,
  `tail_lines` and `max_bytes`; the reported byte counts describe the raw
  stream, not the delivered slice, and status, termination and signal are
  never altered. A bounded in-memory session capture store keeps the raw
  output of finished attempts (32 entries, 16 MiB total, 4 MiB per stream and
  no open descriptors), and `op:"read_capture"` serves slices by line (with
  `grep`, `context` and `strip_ansi`) or by byte offset, answering an unknown
  or evicted id with a non-fatal `CAPTURE_NOT_FOUND`.
- `sharky capabilities --json` and `sharky help [TOPIC] --json` expose a
  machine-readable contract for agents. The capabilities command runs before
  shell initialization and prints the schema and shell versions, the live
  protocol, builtins, CLI subcommands, per-item syntax support (`supported`,
  `partial`, `unsupported`) and the isolation guarantees, while JSON help
  answers from the same registry the text topics use.
- `op:"code"` and `sharky code (-e PROGRAM | FILE | -)` add a programmatic
  orchestrator as a bounded, pure-Rust micro-runtime over the session's own
  shell, capture store and task machinery. Programs call `sh`, `run`,
  `parallel`, `map_parallel`, `workflow`, `state` and `read_capture` under a
  step budget, a call budget, a deadline and a recursion depth of 32. Fatal
  signals and the deadline are checked at every step; supervisor cancellation
  is polled every 32 evaluation steps and at every builtin entry, then
  propagated to child processes. Every response carries a mandatory `audit`
  envelope (calls, failed and cancelled calls, first failure status, steps,
  captures), so a program cannot conceal a non-zero child execution. The
  runtime and its exit statuses are documented under `sharky help code`.

### Changed

- The `set` and `readonly` builtin help and the man page now describe the
  `noclobber` option and the readonly attribute.
- Non-UTF-8 command-line arguments, environment names and values, and initial
  working directories now produce one explicit stderr warning naming the item
  kind instead of converting silently with `U+FFFD` replacement. The conversion
  remains lossy because the shell word model stores `String`; the warning makes
  the limitation visible without printing the offending bytes or replacement
  characters. The remaining gap is documented in
  `docs/agent-runtime-mvp-design.md`.

### Fixed

- `set -C` / `set -o noclobber` and the `>|` override work: a plain `>` no
  longer silently truncates an existing regular file, and `>|` forces the
  write. Append, here-documents and device, FIFO or symlink targets keep their
  previous behaviour.
- The readonly attribute is enforced everywhere: assignments, `unset`,
  `declare`, `export`, array elements, arithmetic and `read` all refuse to
  modify a readonly variable. As in bash the failure discards only the current
  line (or the current pipeline member), leaves the following line running
  with status 1 and, under `errexit`, ends a non-interactive script.
- `PIPESTATUS` is published for foreground pipelines, with one status per
  member in pipeline order.
- `return` now leaves only a function or the sourced file it runs in; outside
  both contexts it reports the bash diagnostic with status 2 instead of ending
  the script. `errexit` no longer leaks into `$(...)`: command substitutions
  start with it disabled, while their final status still reaches the caller.
- Agent session reliability and signal hygiene: `SIGTERM`/`SIGINT`/`SIGHUP`
  terminate idle, `exec` and `parallel` sessions cleanly without leaking child
  groups; command and process substitutions join supervised groups and honor
  cancellation/timeouts in session mode; unwritable persistent redirects no
  longer fail capture restore; duplicate queued IDs and idle self-cancels are
  classified before mutating group state; and `getopts` respects `readonly
  OPTARG`.
- `read` parses `-n`, `-N`, `-a`, `-t`, `-d` and `-p`: `-n` counts UTF-8
  characters, `-a` fills an array, and `-t` uses a poll-based timeout that now
  rejects an overflowing value with an error instead of panicking. `jobs -p`
  and `jobs -l` list job pids, with commands added for `-l`.
- `declare` in a function is local by default; `declare -g` writes through a
  shadowing local to the global binding, and `declare -F` lists defined
  functions.
- `$(<file)` reads a file directly rather than spawning a subprocess.
- Arithmetic handles hex and octal literals, bitwise operators, shifts and the
  ternary `?:` operator.
- Tilde expansion resolves `~user`, `~+` (the current directory) and `~-` (the
  previous directory).
- History expansion keeps `!` literal before punctuation and metacharacters
  and inside double quotes, instead of starting an unintended event designator.
- `wait` returns a reaped job's real status on a later call, waits for every
  listed id, and rejects option-like arguments such as `wait -1` with a usage
  line and status 2 rather than reaping an arbitrary child. `kill -0` works as
  a liveness check.

## [3.7.0] - 2026-11-20

### Changed

- Interactive sessions refresh the shared history file before every primary
  prompt, so a command another shell ran appears in the editor, the autosuggest
  and the `history` listing without `history merge`, which stays as the forced,
  idempotent form of the same path. One `stat` decides whether the file has to be
  read, an unchanged file is neither read nor locked, and a continuation line
  never refreshes. History expansion is guarded by origin: `!!`, `!n`, `!prefix`,
  `!?text?` and `!-n` resolve only the entries a session loaded at startup or ran
  itself, never a command a refresh picked up from another terminal, and after a
  refresh `!n` numbers that expansion list. A `history -c` in one session clears
  what the others had loaded while their own commands survive, and a failed
  append stays expandable. Sessions no longer keep their own view until
  `history merge`.
- Repeating an older command now moves it to the newest position in the history
  view and increments its hit count, so `!!` resolves to the command just
  repeated (`ls; pwd; ls` leaves `!!` = `ls`) rather than the intervening one.
  Two identical consecutive commands are stored as a single record, and the
  view keeps one copy of each command at its newest position.

### Added

- `_` and `SHLVL` are tracked like bash: a normally spawned child environment
  always carries `_` as the resolved path of the command being started, while
  the shell's own `$_` is the last expanded argument of the previous simple
  command (empty after an assignment-only command, unchanged across a pipeline
  or subshell). `SHLVL` starts one above the inherited value (1 when unset or
  invalid), saturates with bash's warning at 1000 and follows assignments. It is
  exported one lower for the final command of a `-c` string, a command
  substitution, a process substitution or a subshell, matching bash's no-fork
  exec path; substitutions created inside a pipeline member keep the normal
  level. The explicit `exec` builtin exports one less too, except inside a
  subshell where the normal level is kept, and it does not rewrite `_`.
- The `history` builtin searches, filters and deletes: `history [search]
  [--max N] [--reverse] [--show-time] [--null] [--case-sensitive]
  [--contains|--prefix|--exact] [PATTERN]` lists matching entries newest first
  with a case-insensitive substring match by default, `--max 0` prints nothing,
  `--show-time` prefixes each timed entry with a UTC timestamp line and
  `history delete --exact PATTERN` removes exact matches by rewriting the file
  under the history lock, preserving legacy bytes and the v2 record fields.
- Auto-clean bounds the shared history file itself: a vacuum caps it at 262144
  records by file position, independently of any session's in-memory
  `tide history` limit (default 1000). A small in-memory limit never truncates a
  file other sessions share; duplicate commands collapse to their newest record
  and legacy lines keep their original bytes when rewritten.
- History records use a one-line v2 format,
  `h2 \t <when-us> \t <cwd> \t <npaths> \t <paths...> \t <cmd> \t <hits>`
  (TAB-separated, with escapes inside each field), carrying the command's
  timestamp, working directory, path hints and hit count. Legacy v1 lines
  coexist and migrate lazily: loading rewrites nothing, and a legacy record
  keeps `when = None` and its original bytes. Downgrading to a 3.6.0 binary is
  non-destructive: that binary reads v2 lines as opaque commands and its vacuum
  keeps them opaque, so the 3.6.0 history file is never lost or corrupted.
- The ghost autosuggestion completes the token under the cursor instead of
  requiring a whole-line prefix: it scans history newest first and prefers an
  exact case-sensitive token match over a case-insensitive one. A record whose
  referenced paths no longer resolve relative to where the command ran is
  skipped; a command whose output path did not exist at record time, and legacy
  records without paths, stay eligible. Ctrl-R ranks a non-empty query by
  substring and conservative frecency (hits times an age weight of 4 within a
  day, 2 within 30 days and 1 beyond, with legacy records counted as old),
  breaking ties on the newer timestamp and then the file position, and pins the
  newest exact case-sensitive prefix match first so repeated Ctrl-R finds the
  command just typed; an empty query and ordinary Up/Down recall stay
  chronological.
- The flake now exposes `devShells` with the Rust toolchain, `nixosModules` and
  `homeManagerModules` for declarative setup, and `passthru.shellPath` on the
  package so it can be used as `users.users.<name>.shell` on NixOS.
- Signal traps now execute instead of only being stored. A handler records the
  signal in an atomic bitmask and the run loop dispatches it at a command
  boundary, after the foreground command has finished and the terminal is back
  with the shell, as bash does. `trap -l` lists the signal names, `trap -p`
  lists installed actions and `trap -p -- SIG` narrows the listing; an EXIT
  action sees the status the shell leaves with in `$?`, `trap ''` ignores a
  signal in children that inherit the disposition, and `trap -` returns the
  signal to where it started. ERR, DEBUG and RETURN are rejected with a
  diagnostic instead of being silently stored.
- `eval`, `umask` and `getopts` are builtins. `eval` joins its arguments with
  spaces and runs the result in the current shell, so assignments, functions
  and exports persist and `return`/`exit` reach the enclosing construct.
  `umask` reads and sets the kernel file-creation mask in octal or symbolic
  form, with `-S` for the symbolic form and `-p` for a reusable command; the
  mask applies to files the shell creates and to child processes. `getopts`
  keeps its state across calls with `OPTIND`, `OPTARG` and `OPTERR`, handling
  clustered options, attached or following arguments, `--`, silent mode and
  reassignment of `OPTIND`.
- `echo` and `printf` are builtins, so they no longer fork a child per call.
  `echo` handles `-n` and `-e`/`-E` with the Bash escape set. `printf` reuses
  its format while arguments remain and supports the standard conversions and
  flags, including `*` width and precision, `%b`, `%q` and `-v` to store the
  result in a variable.
- `declare`, `local`, `readonly` and `typeset` accept `-p` to print reusable
  declarations with Bash quoting, sparse indices and associative keys in
  Bash's bucket order. `type` gains `-t`, `-a`, `-p` and `-P` with Bash's flag
  interactions; its plain output is unchanged.
- Backtick command substitution (`` `cmd` ``) produces the same `Part::CmdSub`
  as `$(...)`, sharing newline stripping, word splitting, globbing and status,
  while applying Bash's backquote rules that remove one level of backslash
  before `$`, `` ` `` and `\`. An unterminated backtick is a syntax error.
- `test` and `[` are builtins following POSIX and Bash's argument-count and
  operator rules, so a login profile can evaluate its guards before `PATH` is
  set up. `[` requires its closing bracket, and malformed expressions exit 2.

### Fixed

- `eval` now executes each complete statement as it is parsed instead of
  parsing the whole string first. A syntax error later in the string reports
  status 2 without discarding what came before it: `eval $'echo one\nif'` prints
  `one`, assignments and exports above the error persist, and a statement that
  exits skips the rest. The parser reports a reserved word that cannot start a
  command (`done`, `fi`, `then` and peers) as the syntax error bash raises
  instead of running it as a command, and a lexical error still runs the
  complete statements above the offending line.
- A login startup file is read statement by statement too, so an unclosed
  `if`/`while`/`{` no longer discards the valid commands above it; the parser
  replays nothing and reports the file and line before the login continues.
- Command lookup tests `PATH` candidates with `access(2)` and the effective
  IDs, so a file whose execute bit belongs to another user class is skipped in
  favour of a later runnable candidate, and a lone non-executable candidate now
  reports 126 rather than 127, matching bash. A file the kernel rejects with
  `ENOEXEC` runs as a shell script through the shell's own interpreter, with
  `$0` set to the resolved path, the arguments forwarded and the script's exit
  status preserved; a NUL byte or the `ELF` magic in the first block still
  reports 126 and is never executed.
- An untrapped fatal signal no longer skips the EXIT action. `TERM`, `HUP` and,
  in a script, `INT` are caught by a handler that restores the terminal,
  forwards a hangup to the jobs and only records the signal; the shell notices
  it through the interrupted `waitpid`, the command boundary or the prompt
  wait, runs the EXIT action once and leaves with 128+signal, as bash does.
- A background job that finishes while the prompt is idle is reaped and
  announced without a typed command. A SIGCHLD handler records the change, the
  editor's input wait wakes for it, and the bash-compatible notice (the 27
  column status field, `Done`, `Exit N`, `Terminated` and the other signal
  descriptions, `+`/`-` job markers) prints above the line being edited, which
  is then redrawn intact.
- Only exported variables reach a child process, as in bash. `Shell::env` is
  still the single scalar store, so expansion, `declare -p` and `set` keep
  seeing every variable, but the environment handed to a spawn, `exec`, a
  command substitution, a process substitution or the `--json` path is filtered
  by a new export attribute. Variables inherited from the parent start exported;
  `export NAME`, `export NAME=value` and `declare -x` set the attribute;
  `export -n` and `unset` clear it; and a plain assignment keeps the current
  attribute. `VAR=val cmd` stays a temporary overlay for that one command.
  `export -p` now lists only the exported variables in bash's
  `declare -x NAME=value` form.
- A login shell now executes its startup files through `Shell::eval` instead of
  scanning them for literal `NAME=value` assignments. It runs `/etc/profile`
  (which on NixOS sources `set-environment`, `/etc/profile.local` and
  `/etc/bashrc`), then `/etc/sharky/profile.sharky`, then the first existing of
  `~/.bash_profile`, `~/.bash_login` and `~/.profile`, then
  `~/.config/sharky/profile.sharky`. A file with a parse error reports its path
  and line, replays the statements above the error and lets the login continue.
- Command substitution keeps output that is not valid UTF-8: captured bytes are
  decoded lossily (invalid sequences become U+FFFD) instead of the whole
  capture being dropped, and NUL bytes are discarded with one warning per
  substitution as bash does. Preserving raw bytes remains a documented
  limitation.
- A broken pipe (EPIPE) now ends the shell quietly with status 141
  (128+SIGPIPE) by restoring SIGPIPE's default disposition at startup, instead
  of every write returning EPIPE and panicking in Rust.
- Background job-id notices (`[id] pid`) are printed only in interactive
  shells, so they no longer leak into command substitutions or scripts.
- Running a missing command path now exits 127 (command not found); an existing
  but non-executable file still exits 126.
- `wait` on a stopped job uses `WUNTRACED`, reports 128+signal (147 for
  SIGSTOP) and keeps the job in the table, so it stays listed and can be
  resumed or signalled. Previously the job was removed and the wait hung.
- Here-documents are written through a file created with `create_new` and
  unlinked immediately, so a symlink planted at a predictable temp name can no
  longer redirect the write; the descriptor alone serves the redirection.
- The NixOS and Home Manager modules assert `x86_64-linux`, reporting a clear
  error on an unsupported platform instead of evaluating with a missing binary.

## [3.6.0] - 2026-11-19

### Added

- `sharky status` prints plain-text facts before the shell starts: the version,
  the config and theme paths resolved through `SHARKY_CONFIG`, `XDG_CONFIG_HOME`
  and `HOME` with existence markers, the history path resolved through
  `SHARKY_HISTORY`, `XDG_DATA_HOME` and `HOME` with an existence marker, and the
  active compile-time feature flags (`none` in this release). Both subcommands
  exit 2 on malformed arguments; `status` exits 0 otherwise, and `doctor` exits
  0 when all checks pass and 1 when any fails.
- `sharky doctor` runs read-only local checks: stdin/stdout descriptor validity,
  config parsing through `config::load` with a separate check for files that
  load would silently skip, theme parsing, history permissions and writability
  by metadata only, and resolution of `sh`, `ls` and `cat` through
  `exec::resolve`. It prints one `OK`/`FAIL` line per check, exits 0 when all
  pass and 1 when any fails, and performs no fstab checks. Both subcommands are
  dispatched from `cli::run` before the normal shell startup path, so neither
  applies `pearl`, evaluates profiles, writes files or prints environment or
  configuration values; `doctor` builds a bare `Shell` only to resolve tool
  names. `status` reads no configuration files; `doctor` parses `config.sharky`
  and `theme.sharky` read-only through the configuration parser.
- Durable command history: each evaluated command is appended to one shared file
  under a lock, with failed writes kept only in memory and torn tails dropped on
  the next append. Sessions keep their own view until `history merge`, and
  `-P`/`--private` keeps history in memory. The file defaults to
  `${XDG_DATA_HOME:-~/.local/share}/sharky/history` (`SHARKY_HISTORY` overrides).
- Prebuilt Nix flake: `nix profile install github:mafuzyk/sharky-shell` installs
  the verified static binary, man page and example config on `x86_64-linux`.
- Opt-in `wave overwrite` styling colours the literal target of `>`, `2>` and
  `&>` when it names an existing file.
- `shopt` supports exactly one option, `globstar`: `shopt -s globstar` enables
  recursive `**` pathname matching (off by default), `shopt -u globstar`
  restores ordinary single-level globbing and `shopt globstar` reports the
  state. Quoted `**` stays literal, hidden entries are skipped unless the
  component starts with `.`, matches are deduplicated and `./`/`../` prefixes
  are preserved. Other option names and combined or interleaved `-s`/`-u`
  flags are rejected; the setting is per shell and is not persisted.
- `mapfile`/`readarray` reads stdin lines into an indexed array, defaulting to
  `MAPFILE`. The optional `-t` strips the trailing newline while preserving
  carriage returns and backslashes. Invalid options return 2, while invalid or
  associative targets return 1, matching Bash's status codes.
- A portable and Bash-only compatibility corpus under `compatibility/` freezes
  expected stdout, exit status and controlled side effects for small shell
  fixtures and compares a candidate against dash and Bash. The `verify-refs`
  command checks the references against the frozen baseline; `check <binary>`
  measures a built binary's stdout/status/side-effect divergence. stderr is
  not compared and no conformance is claimed.
- `sharky config explain` prints the configuration guide as plain text before the
  shell starts, reading no configuration, profiles or history and leaving no side
  effects. With no topic it lists every topic id; the words after `explain` are
  joined with spaces to select one topic. Each parser directive has its own topic
  (`tide`, `pearl`, `current`, `reef`, `shoal`, `anchor`, `coral`, `wave`, `wake`)
  alongside `tide history`, `files`, `theme` and `overwrite`. Unknown topics and
  mixed options exit 2, and there is no top-level `sharky explain` alias.
- Unset-only parameter operators `${name-word}`, `${name=word}`, `${name+word}`
  and `${name?word}` act only on unset parameters, leaving the colon forms
  (`${name:-word}`, `${name:=word}`, `${name:+word}`, `${name:?word}`) to treat
  unset and null alike. `${name?word}` expands its word before reporting it,
  and an empty `$@`/`$*` counts as unset for `+` and `-`.
- Unquoted expansions are split on `IFS` (space, tab and newline by default):
  repeated delimiters keep empty fields, leading and trailing ones are dropped,
  and an empty `IFS` disables splitting. Literals and quoted expansions stay
  unsplit.
- The `local` builtin declares variables in the current function frame with
  dynamic scope: earlier bindings (scalar, indexed or associative) are saved
  and restored on return, and locals are visible to called functions. `local`
  outside a function, invalid names and unsupported options report failure.
- `sharky help [TOPIC...]` and the `help` builtin share one static registry with
  `--help`. With no topic they print the same index as `--help`; with a topic
  they join every word with spaces and print one entry. Topics are builtin
  names, command-line flags and subcommands, plus the configuration topics of
  `config explain`. An unknown topic exits 1. The subcommand runs before the
  shell starts: no configuration is loaded or created and there are no side
  effects.
- `--check` parses `-c` or a file for syntax only: it exits 0 silently on
  success, or 2 with a line reference on a top-level syntax error. It requires
  `-c` or a file (not stdin), checks only top-level syntax, executes nothing and
  neither reads nor writes configuration, profiles or history. It cannot be
  combined with `-i`, `-l`, `--json` or `state`.
- `command` runs a name without function lookup, so it reaches the builtin or
  external command hidden by a same-named function. `-v` prints how each name
  resolves (bare name for builtins and functions, `alias name='value'` for
  aliases, a path for externals) and `-V` prints a verbose description; missing
  names are skipped silently by `-v` and reported on stderr by `-V`. `-p`
  searches the default PATH instead of the environment PATH for lookup and for
  execution, without changing the environment. The status follows Bash: with
  `-v`/`-V` it is 0 when any name resolves (and with no names), and 1 only when
  every name is missing; otherwise it is the status of the command run (127
  when the name is not found), and an invalid option returns 2.
- `command -V NAME` now prints the function definition after the
  `NAME is a function` line, in Bash's `declare -f` layout: the name and an
  empty parameter list, the body in braces with one statement per line at
  four-space indentation and semicolons between statements. Nested blocks and
  `elif` are formatted the way Bash prints them.

### Fixed

- `"$*"` and `"${a[*]}"` join their elements with the first character of `IFS`
  (nothing when `IFS` is empty) instead of a literal space.
- `${!a[*]}` and `${!m[*]}` join indexed and associative array keys with the
  first character of `IFS` (nothing when `IFS` is empty) instead of a literal
  space.
- `${!x[@]}` and `${!x[*]}` treat a defined scalar as a one-element key list
  holding `0`, so they expand to `0` instead of nothing; an undefined name
  still expands to no keys, and empty or sparse arrays keep listing only their
  real indices.
- `set` accepts clustered short options such as `-euo`, long names under
  `-o`/`+o` (`errexit`, `nounset`, `xtrace`, `histexpand`, `pipefail`, `vi`),
  and keeps reading letters after `o` so `set -oe pipefail` enables both.
  `set +o` without a name lists the active options as reusable `set` commands.
- Array literals parse after `local`, `readonly` and `typeset`, so declarations
  such as `local a=(one two)` no longer fail.
- An assignment-only simple command takes the status of its last command
  substitution, so a failing substitution now trips `errexit` instead of
  reporting success.
- `set -e` and `set -u` no longer exit the interactive shell: errexit failures,
  unbound variables and failing `if` conditions report a status and return to
  the prompt.
- Expansion errors now follow Bash's per-context status: a null or unset
  parameter error exits 127 in a `-c` command string without errexit, while
  scripts, standard input, subshells, errexit shells and interactive lines keep
  status 1 (arithmetic errors stay at 1). An expansion error in an `if`/`elif`
  condition discards the whole compound command: no branch runs, not even
  `else`. Non-interactive shells abort on the error; interactive shells report
  its status and keep running.
- `read` splits fields on `IFS` with Bash's last-name remainder rule: leading
  and delimiter-adjacent IFS whitespace is skipped, a non-whitespace delimiter
  ends a field even when empty, and the last name receives the rest of the line
  with its delimiters. A trailing IFS character ends the remainder, a null IFS
  leaves the line unsplit, and bare `read` stores the untrimmed line in `REPLY`.
- A name no longer holds both a scalar and an array binding: a later assignment
  of either kind wins, scalar writes go into element 0 of an existing array, and
  a stale scalar is no longer exported after the name becomes an array.
- Declaring an indexed array associative, or an associative array indexed, no
  longer discards its elements: the conversion is rejected with status 1, the
  original array is kept and the error is reported for that name while the
  remaining arguments still apply.
- `command -v`, `command -V` and `type` report a name containing a slash only
  when it names an executable file, without searching PATH: non-executable
  files, directories and symlinks to non-executable files are no longer
  reported as found. Running such a path still reports permission denied.
- When both `-v` and `-V` are given to `command`, the last one wins, whether in
  one cluster or across separate words: `command -Vv cd` prints the bare name
  and `command -vV cd` prints the description. Previously any `-V` forced the
  verbose form.

## [3.5.4] - 2026-11-02

### Fixed

- The autosuggest ghost never paints itself in the same style as typed text: a
  theme without `wave suggestion` now gets dim ghost text instead of the
  terminal's normal foreground.

## [3.5.3] - 2026-11-01

> _Added more bugs to fix it later._

### Added

- Named-FIFO fallback for process substitution on hosts without `/dev/fd`.
- `wake` is a builtin, so login profiles can register startup commands.

### Fixed

- `declare -A a=([k]=v ...)` parses and fills the map.
- Associative keys accept quoting and expansion; `"${a[*]}"` joins, while
  `"${a[@]}"` keeps one field per value, including prefixes, suffixes and case
  operators.
- `unset 'a[i]'` leaves a hole instead of renumbering later elements.
- Process-substitution descriptors survive function bodies, and substitution
  children are reaped at prompt idle without taking the terminal foreground.
- Completion escapes file names and never opens a menu in a narrow terminal;
  the fuzzy pager uses the alternate screen.
- Multiline redraw returns from the row it drew, wide characters count two
  columns, and pasted control sequences are inserted verbatim.
- SIGTERM and SIGHUP restore the terminal and disable bracketed paste.
- JSON capture uses private exclusive files, survives `exec`, aborts before
  running when capture cannot be set up, and keeps profiles inside the
  envelope. Non-UTF-8 environment values are replaced, not dropped.
- Arithmetic and substring offsets no longer abort on overflow; arithmetic
  offsets are evaluated, `&&`/`||` short-circuit without side effects, and
  malformed C-style `for` is a status-2 error.
- History selectors respect quoting, and an apostrophe inside double quotes no
  longer suppresses expansion.
- Profiles run as whole units, so `if`/loops are not executed line by line.
- Vi counts work for `e`, `p`, `P` and `dd`; Esc cancels an operator without
  moving the cursor.
- Ctrl-_ reaches undo, transpose follows readline, and `set -o` without a name
  reports active options.
- Extglob handles quoted parentheses, nullable `+()` alternatives, quoted
  metacharacters and globs in directory components.
- Empty quoted arguments keep their field (`set -- ""`, `pre"$@"post`,
  `"${@}"`, `"${*}"`) and zero-element `[@]` expands to zero fields.
- `set -e` ignores the status of a negated pipeline, as Bash does.
- JSON `--json` reports the cwd and environment delta reached before `exec`,
  including when `exec` comes from a login profile.
- A login profile with a parse error stops at that file instead of running its
  lines outside the broken structure; diagnostics carry the real line.
- The FIFO fallback wakes and removes an unconsumed substitution when its
  command ends, waits for slow consumers, reaps substitutions while the prompt
  waits, and never deletes unrelated files from the temp directory.
- Command completion escapes PATH executables, and pasted content ending in
  ESC no longer loses the end marker.
- Sparse array slices select by index, negative offsets follow the last
  defined index, `unset 'a["key"]'` removes associative keys, and `${@:n}`
  keeps one field per argument.
- The process-substitution scanner accepts `case` lists and extglob patterns,
  including cases nested in `if`/`for` and an empty `case ... in esac`.
- Malformed arithmetic is a status-2 error instead of a lexer panic, and an
  arithmetic expansion error fails the command before it applies redirections.
- Quoted glob characters stay literal (`x"*"`), including inside quoted array
  expansions.
- Vertical movement preserves the display column with wide characters and
  tabs, the menu closes on resize, SIGTERM leaves the fuzzy alternate screen,
  and vi `r` leaves the cursor on the last replaced character.

## [3.5.2] - 2026-11-01

### Fixed

- The editor no longer clears the prompt on every redraw. `ESC[J` now starts
  at the end of the prompt, so prompt icons and text stay visible while typing.

## [3.5.1] - 2026-11-01

### Fixed

- The editor no longer moves the cursor one row too far when the prompt plus
  the input exactly fills the terminal width. Long commands stopped erasing the
  line above and Koral's command mark stayed intact.

## [3.5.0] - 2026-10-31

### Added

- Extended parameter expansion: substrings, case conversion, indirect
  expansion (`${!x}`, `${!prefix*}`, `${!prefix@}`) and `${!a[@]}` indices.
- ANSI-C quoting (`$'...'`) with `\n`, `\t`, `\xHH`, `\uHHHH`, octal and
  the other Bash escapes.
- Associative arrays (`declare -A`), array slices and arithmetic indexes.
- C-style `for ((...))`, `case` fallthrough with `;;&` and `;&`, and extglob
  patterns (`?(a|b)`, `*(a|b)`, `+(a|b)`, `@(a|b)`, `!(a|b)`).
- Process substitution (`<(list)`, `>(list)`) with reaped children and closed
  descriptors.
- Interactive history expansion: `!!`, `!n`, `!-n`, `!prefix`, `!?text?`,
  `!$`, `!*`, `!^`, `^old^new^`, controlled by `set -H`/`set +H`.
- `exec` with `-a`, persistent redirections and interactive failure handling;
  `time` and `time -p`.
- Bracketed paste and multiline editing with per-line Up/Down and Home/End.
- History autosuggest with ghost text and Right/End acceptance.
- `wave`-driven syntax highlighting for commands, builtins, strings,
  variables, operators, paths, comments and lexer errors.
- `anchor` keybindings for the full action set, including
  `accept_suggestion` and `fuzzy_files`; `ctrl+x`, `c+x`, `alt+x` and `m+x`
  spellings.
- Completion for commands, variables, files, `~` paths and git branches, with
  an inline menu; Ctrl-T fuzzy file search.
- Agent interface: `--json -c` with captured stdout/stderr, status, duration,
  cwd and environment delta; `state --json [--env]`; `-i` to force the REPL.
- Login shell (`-l`/`--login` or a leading `-` in `argv[0]`) reading sharky
  profiles and importing only literal assignments from standard profiles.
- Vi mode (`set -o vi`): insert/normal modes, `h l 0 $ w b e j k`, `x dd dw
  d$ D cw C p P u r<char>` and count prefixes; `set +o vi` restores emacs.

### Changed

- Navigation keys are bindable through `anchor`; defaults match the previous
  behaviour.
- `set` reports `histexpand` and `vi` when they are enabled.

## [3.0.0] - 2026-10-31

### Added

- Koral terminal integration: OSC 133 prompt and command marks with a
  URL-encoded command line, OSC 7 working-directory marks, and the exit status
  on every command end.
- `sharky-config` crate shared with Koral: parser, `tokenize`, `INSTRUCTIONS`
  and `ACTIONS`, with no dependencies and a stable public API.
- Abbreviations: `shoal` in `config.sharky`, `abbr`/`unabbr` builtins, and
  expansion on space or Enter at command position.
- Ctrl-R incremental reverse history search; Enter runs the match and Ctrl-G
  cancels.
- Multi-line input in the REPL: unfinished compound commands keep buffering
  instead of failing at the end of the line.

### Changed

- The configuration parser moved to `rust/config`; the shell depends on it by
  path. The release binary stays static, with `nix` as the only external
  dependency.

## [2.1.0] - 2026-10-31

### Added

- Starship prompt integration (`tide starship on`), including status, command
  duration and running jobs.
- Shell options: `set -e`, `set -u`, `set -x`, `set -o pipefail`, `set --` and
  bare `set`; `trap` EXIT with listing and removal; `read` and `shift`.
- `[[ =~ ]]`, `-a`, `-L`/`-h` and Ruby-style integer coercion.
- Indexed arrays, here-strings, `&>`/`&>>`, brace expansion and
  `${x/pat/repl}`/`${x//pat/repl}`.
- Builtins `type`, `alias`, `unalias`, `history`, `pwd`, `wait`, `kill`,
  `z`, `zi` and `reload`.
- Startup commands (`wake "fastfetch"`) and automatic `PATH` discovery from
  shell profiles (`tide profile_paths`).
- First run seeds `~/.config/sharky/config.sharky` and `theme.sharky`.

### Fixed

- Malformed input no longer panics or hangs the parser (`;;&`, `for ;`,
  `for ((...))`, unclosed `$(`, `$((` and `${`).
- `&` separates list items on the same line, and background jobs work outside
  the REPL with process groups.
- Multi-line prompts such as Starship are drawn once per input line instead of
  being reprinted on every keystroke.
- `cd` handles no argument and `-`, updates `PWD`/`OLDPWD`, and records visits
  for `z`; `$!` reports the last background pid.

## [2.0.0] - 2026-10-28

### Added

- Rust implementation of sharky, replacing the Ruby shell, with static musl
  releases and AUR packages.

## [1.0.1] - 2026-10-28

### Fixed

- Redirections now expand arguments, assignments and every target before opening
  files, so a failed expansion cannot truncate an existing destination.
- Parser fixes for whole-subshell redirection, escaped globs, multiple prefixed
  assignments, here-document literal bodies and `$X` delimiters, comments inside
  `${...}`, contextual `))`, multiline subshells and newlines after operators.
- Arithmetic fixes for short-circuit operations, negative division/remainder and
  relational/equality precedence; compound commands work inside `$()`.
- Command status is now recorded for pipelines, subshells, conditionals and
  negations; `&&`/`||` mixed lists and suppressed `set -e` contexts behave like
  Bash; EXIT traps run all actions and honor an explicit `exit` status.
- Session, JSON and the Ruby facade now use the compound parser, matching the
  normal `-c` entry point.
- `[[ ]]` rejects unknown operators, supports symlink and lexical predicates,
  and no longer returns true for an unsupported expression.
- `type` recognizes functions defined by the native parser; editor history Down
  moves toward newer entries; docs describe the existing Ruby completion hook.
- Job-control fixes for partially reaped `fg` pipelines, pipefail, HUP cleanup of
  running/stopped jobs, default `fg` selection and done-job pruning.
- Compound stdin reaches external commands; command-substitution stderr reaches
  JSON envelopes; `[[ ]]` expands variables/patterns; `read` handles leading
  whitespace, escapes and line continuations.
- Starship accepts an explicit shell environment, including removed variables,
  and sets the working directory consistently. Its deadline now also covers
  output readers when descendants inherit the pipes.
- Git prompt queries drain stdout and stderr while waiting, preserving dirty
  state for repositories with large status output under one deadline.
- Result environment deltas copy and freeze names and values rather than
  sharing mutable strings with a session.
- Arch packaging uses the cloned source directory and declares its Git build
  dependency. Source tarballs include tests, benchmarks and CI metadata;
  installation checks preserve existing artifacts by building in temporary trees.
- CI fails on Ruby warnings, and the PTY soak requires an actual output line and
  a new prompt rather than accepting editor echo. Documentation now describes
  the supported native language, renderer-only measurements and 1.x security policy.

## [1.0.0] - 2026-10-28

### Added

- First-class `Sharky::Session` and structured `Sharky::Result`; `--json -c` and
  `state --json` use a versioned envelope with environment redaction by default.
- Native Bash-like language subset: conditionals, loops, `case`, functions,
  groups, `!`, arithmetic commands and expansion, indexed arrays, brace
  expansion, parameter expansion, here-documents, here-strings and compound
  redirections.
- Native `.sh` scripts and shebangs, `$0`/`$#`/`$@` positional parameters,
  function arguments, `source`/`.`, `set -e/-u/-x`, `pipefail`, `read`,
  `shift`, `break`/`continue`/`return` and `trap` (EXIT action executes).
- Interactive continuation for compound commands and here-documents; syntax
  highlighting and completion understand the new grammar.
- `Makefile` prefix install/uninstall, source tarball, Arch `PKGBUILD`, man page
  and an install integration test.

### Changed

- CLI `-c` and the interactive loop use the compound parser; the legacy
  `Shell#run` default remains for compatibility.
- Unsupported syntax still exits 2 with a precise message; no Bash fallback.
- Job listings reconstruct compound command text instead of showing blanks.

### Fixed

- Command substitution now runs in a forked subshell, captures stdout, strips
  trailing newlines and reaps the child.
- Compound trailing redirections, `pipefail`, nounset defaults and brace
  expansion regressions found by the Bash oracle matrix.

### Known limitations

- Login-shell installation remains post-v1 until the stability gate passes.
- Signal traps are stored, but only the EXIT trap executes in v1.
- No positional/slice array field splitting parity; no associative arrays,
  `coproc`, process substitution, extglob, programmable completion or full
  `[[ =~ ]]`.
- `pipefail` is not applied to job-control pipelines.

## [0.1.0] - 2026-10-27

### Added

- Unix-first interactive shell written in Ruby.
- Bash-like subset: quotes, variables, tilde expansion, globs, command
  substitution, pipes, redirects, lists, subshells and background jobs.
- Real Unix process execution, process groups and job control.
- Custom line editor with autosuggestions, syntax highlighting, completion,
  fuzzy history/file picker and multiline editing.
- Native prompt, themes, history, `z`, aliases, abbreviations and key bindings.
- Ruby configuration, command functions, scripts and REPL.
- TOML theme colors and opt-in prompt layouts.
- Initial test suite, benchmarks and compatibility documentation.
