# Sharky and fish major update

Date: 2026-11-03

Scope: compare Sharky `v3.5.4` at commit `e9ac1e8` with fish `4.9.3` and turn
the gaps into a release plan. This is a capability and maturity audit, not a
claim that Sharky should copy fish's syntax wholesale.

## Executive decision

Sharky already has the visible shell features that make fish attractive:
history autosuggestions, syntax colors, completion, a selection menu, vi
editing, prompt theming, job control and a native execution core. The largest
gap is the system behind those features. Fish treats variables, functions,
events, completions, configuration, help and the reader as extensible runtime
objects. Sharky currently treats most of them as fixed maps or fixed editor
branches.

The proposed major release is **Sharky 4: fish-class interactive UX and shell
maturity, with the current Bash-like language kept as the default**. A full
fish-language mode is a separate compatibility project. Fish is intentionally
not POSIX-compatible and changes variable expansion, command substitution,
arrays, blocks and function syntax; replacing Sharky's parser would be a
language migration, not a feature update.

## Baseline

### Sharky today

The local repository is a Rust shell with a frozen Ruby oracle. The Rust tree
has a native lexer/parser/executor, raw-mode editor, history, completion,
prompt, process groups, traps, process substitution, arrays, extglob,
bracketed paste, vi mode, JSON state capture and Koral terminal marks.

The supported language is a Bash-like subset documented in the root README and
the Rust man page. The current design deliberately keeps configuration
declarative (`config.sharky` and `theme.sharky`) so a config typo cannot execute
arbitrary shell code.

Evidence from the tree:

| Area | Current evidence |
| --- | --- |
| Runtime | `rust/src/shell.rs`, `exec.rs`, `lexer.rs`, `parser.rs` |
| Reader | `rust/src/editor.rs`, `term.rs`, `highlight.rs` |
| State | `rust/src/history.rs`, `z.rs`, `profiles.rs` |
| Config | `rust/config/src/lib.rs`, `examples/config.sharky`, `examples/theme.sharky` |
| Verification | 82 conformance cases, 212 CLI tests, 59 interactive tests, 2 job-control tests, 1 robustness test, 26 unit tests |
| Distribution | Linux-focused static musl release, Arch packages, GitHub release workflow |

The README still records three deliberate language limits: word splitting is
fixed to space/tab/newline instead of `IFS`, ANSI-C escapes above `0x7f` are
stored as UTF-8 characters, and `TIMEFORMAT` is outside the scope. `PORTING.md`
also records a known shortcut: every shell variable is exported to children.

### fish today

The current fish documentation is `4.9.3`. Its interactive contract includes
syntax error highlighting, autosuggestions from history/completions/paths,
completion descriptions and a searchable pager, configurable Emacs and vi
bindings, a kill ring, multiline editing, history search and directory
history. The prompt is implemented by functions (`fish_prompt`,
`fish_right_prompt`, `fish_mode_prompt`), and the terminal title is also
programmable.

The language is list-first. Variables can be universal, global, function or
block-local; `PATH` is a path list; command substitutions split on newlines;
word splitting is not performed. Functions and completions can be autoloaded
from user, system and vendor paths. Event handlers can react to variable
changes, jobs, startup and exit. The shell ships data-oriented builtins such
as `string`, `math`, `count`, `contains`, `path`, `status`, `argparse`,
`command`, `builtin`, `functions`, `complete`, `bind`, `help` and
`fish_add_path`.

The repository signals a mature project: 23,425 commits, 34.4k stars, 507
open issues and 63 open pull requests at the time of this audit. It has
dedicated source, tests, benchmarks, build tools, documentation, localization,
macOS and BSD support directories, generated docs and a contributor workflow.

Sources:

- [fish introduction](https://fishshell.com/docs/current/)
- [fish interactive use](https://fishshell.com/docs/current/interactive.html)
- [fish language](https://fishshell.com/docs/current/language.html)
- [fish command options](https://fishshell.com/docs/current/cmds/fish.html)
- [fish design](https://fishshell.com/docs/current/design.html)
- [fish command-specific completion](https://fishshell.com/docs/current/cmds/complete.html)
- [fish functions](https://fishshell.com/docs/current/cmds/function.html)
- [fish configuration and autoload paths](https://fishshell.com/docs/current/language.html#configuration-files)
- [fish contributing and testing](https://fishshell.com/docs/current/contributing.html)
- [fish repository](https://github.com/fish-shell/fish-shell)

## Feature comparison

Status meanings: **present** means the user-visible contract is already there;
**partial** means the feature exists but lacks fish's extensibility or depth;
**missing** means there is no equivalent; **strategic** means adding it would
change the language contract and needs an explicit migration decision.

### Interactive reader

| Capability fish provides | Sharky status | Gap to close for v4 | Priority |
| --- | --- | --- | --- |
| Autosuggest from history | Present | `find_suggestion` only searches history and only at the end of the buffer. | P1 |
| Autosuggest from valid paths and completion results | Missing | Feed the same completion engine into suggestion ranking and accept one word or the whole suggestion. | P1 |
| Semantic syntax highlighting | Partial | Sharky colors lexical roles and unterminated constructs, but does not mark unknown commands, invalid redirects, or valid paths with semantic certainty. | P1 |
| Command-specific completions | Partial | Sharky completes commands, variables, files, tilde paths and git branches. It has no completion registry, descriptions, option metadata or user completion files. | P0 |
| Completion pager with search | Partial | The inline menu supports selection and cycling, but not a fish-style filter/search field, page navigation or descriptions. | P1 |
| History pager | Missing | Ctrl-R is one reverse incremental search line. Add a reusable pager that searches, pages, filters and returns a selected entry. | P1 |
| History search by token under cursor | Missing | Add the fish `alt-up`/`alt-down` style token search once the reader has token spans. | P2 |
| Kill ring | Partial | Sharky keeps one kill string. Fish keeps a ring and exposes copy/paste bindings. | P1 |
| System clipboard | Missing | Add optional terminal clipboard hooks and `fish_clipboard_copy`/`fish_clipboard_paste`-style reader actions without making a clipboard tool mandatory. | P2 |
| Key binding registry | Partial | `anchor` accepts fixed actions and one key sequence. Fish `bind` supports modes, sequences, erase/list/query and user functions. | P0 |
| Full vi reader | Partial | Sharky has a practical normal/insert subset. Fish also has visual mode, registers, richer motions and mode-specific bindings. | P2 |
| External editor for the command line | Missing | Add a `commandline`/`edit_command_buffer` equivalent using `$VISUAL` or `$EDITOR`. | P1 |
| Multiline editing | Present | Keep the current buffer and redraw tests; extend pager and selection behavior over multiline spans. | P1 |
| Bracketed paste | Present | Keep the byte-preserving behavior and add paste-aware syntax validation before submit. | P1 |
| Directory history | Missing | Add `dirprev`, `dirnext`, `dirh`, `cdh`, `prevd`, `nextd`, and bindings from an empty command line. | P1 |
| Directory stack | Missing | Add `dirs`, `pushd`, and `popd`; keep `z`/`zi` as a separate frecency feature. | P2 |
| Right prompt and mode prompt | Missing | Support left/right/mode prompt output and width-aware redraw. | P1 |
| Programmable terminal title | Partial | Sharky emits Koral OSC marks but has no `fish_title`/`fish_tab_title` hook. | P1 |
| Terminal-aware key discovery | Missing | Add a `key_reader` diagnostic command or a debug mode for escape sequences. | P2 |

### Shell language and runtime

| Capability fish provides | Sharky status | Gap to close for v4 | Priority |
| --- | --- | --- | --- |
| List-valued variables | Partial | Sharky has indexed/associative Bash arrays, but ordinary variables are strings and expansion uses Bash-style field splitting. | Strategic |
| `PATH` as a path list | Partial | Sharky prepends strings to a colon-delimited `PATH`; add a safe path view and `fish_add_path`-style normalization first. | P0 |
| No implicit word splitting | Strategic | This conflicts with the documented Bash-like contract and existing differential oracle. Do not silently change it in v4. | Strategic |
| Newline-based command substitution | Strategic | Sharky currently follows its Bash-like splitting rules. A fish-compatible mode needs a separate expansion policy. | Strategic |
| Variable scopes | Partial | Sharky has function-local variables and one environment map; it lacks explicit local/global/function/universal scopes and scope lookup rules. | P0 |
| Universal variables | Missing | Persist user-selected values across shell instances with locking, atomic writes, corruption recovery and `--no-config` isolation. | P0 |
| Function definitions | Present | Functions execute in the current runtime and have positional arguments. | P1 |
| Function descriptions and wrapped completions | Missing | Store metadata with functions so help, type and completion can use it. | P1 |
| Function autoloading | Missing | Search `SHARKY_FUNCTION_PATH`-style directories for `<name>.sharky` or a documented script format and load on first use. | P0 |
| `functions`, `funcsave`, `funced` | Missing | Add inspect/erase/save/edit commands around the function store. | P1 |
| Event handlers | Missing | Add an event registry and `emit`; support startup, variable, job, prompt and exit events with deterministic lifecycle rules. | P0 |
| Fish block syntax (`end`, `switch`) | Missing | This is a language rewrite and should not be mixed into the Bash-compatible parser. | Strategic |
| Fish command substitution `(cmd)` | Missing | Parenthesized subshells already exist in Sharky, but bare fish command substitution is a grammar conflict. | Strategic |
| Data builtins | Missing | Native `string`, `math`, `count`, `contains`, `path`, `random` and `argparse` reduce dependency on external tools in completions and scripts. | P1 |
| Runtime query builtins | Partial | `type` exists; add `status`, `command`, `builtin`, `functions`, `help` and a stable builtin registry. | P0 |
| Shell introspection variables | Partial | Add stable equivalents for shell pid, current command, script path, line number, command substitution state and job-control state. | P1 |
| Feature flags and debug categories | Missing | Add `--debug`, `--debug-output`, `--no-execute`, startup profiling and opt-in feature flags before the ecosystem grows. | P2 |
| `eval` and explicit command dispatch | Partial | `source` exists; add `eval` and `command` so functions can bypass aliases/functions deliberately. | P1 |
| Help for every builtin | Missing | Generate man/help text from the builtin registry and support `builtin -h`/`help NAME`. | P0 |

### Configuration and extension ecosystem

| Capability fish provides | Sharky status | Gap to close for v4 | Priority |
| --- | --- | --- | --- |
| Safe declarative config | Present | Keep it as the default and document its security boundary. | P0 |
| Executable user config | Missing by design | Do not execute arbitrary config implicitly. Add an explicit opt-in script hook if users need runtime functions. | P1 |
| `conf.d` snippets | Missing | Load sorted, shadowable snippets from user/system/vendor locations with an explicit trust and error policy. | P0 |
| Function search path | Missing | Add user/system/vendor function directories and a documented precedence order. | P0 |
| Completion search path | Missing | Add user/system/vendor completion directories with lazy loading. | P0 |
| Completion scripts | Missing | Define a stable completion file format and a `complete` builtin; include metadata, predicates and descriptions. | P0 |
| Universal state file | Missing | Version the format, lock writes, fsync/rename atomically, reject unsafe permissions and recover from a torn file. | P0 |
| Prompt themes and chooser | Partial | Sharky has named colors and a template plus optional Starship. Add built-in prompt presets and a terminal chooser before a web UI. | P1 |
| Web configuration | Missing | Optional P2. It is useful, but it should consume the same config API and never become a startup dependency. | P2 |
| Abbreviations | Partial | `shoal` and `abbr` support fixed command-position text. Fish also supports regex, command filters, dynamic functions and cursor markers. | P1 |
| Path management | Partial | `current` and profile scanning exist. Add `fish_add_path` semantics: normalize, deduplicate, prepend/append, dry-run and persistent user paths. | P0 |
| Plugin manager | Missing | Keep out of the core major release. A stable function/completion path is enough for third-party extensions. | P2 |

### History and privacy

| Capability fish provides | Sharky status | Gap to close for v4 | Priority |
| --- | --- | --- | --- |
| Persistent history | Present | Keep the 0600 file and atomic write behavior. | P0 |
| Duplicate suppression | Present | Sharky suppresses exact duplicates; preserve this contract. | P0 |
| Search modes and filters | Partial | Add contains/prefix/exact, case policy, reverse order, max results and null-delimited output. | P1 |
| History append/merge/save/clear commands | Partial | `history -c` exists; add explicit subcommands and multi-session merge with locking. | P1 |
| Session-specific history | Missing | Add a named history session variable and file policy. | P1 |
| Private mode | Missing | Add `--private` and a runtime flag that neither reads old history nor writes new history. | P0 |
| Timestamps, cwd, duration, exit status | Missing | Store structured records while providing a migration reader for the current line format. | P1 |
| History eligibility hook | Missing | Add an explicit event/function hook so users can suppress sensitive commands. | P2 |

### Operational maturity

| Maturity area | Sharky today | Fish benchmark | v4 requirement |
| --- | --- | --- | --- |
| Supported systems | Linux-focused, static musl release | Linux, macOS and BSD packaging/source paths | Define a portability seam and publish a support matrix; add macOS/BSD CI before claiming support. |
| Tests | Strong local coverage for the current scope; Ruby differential oracle | Dedicated checks, interactive scenarios, benchmarks and broad platform CI | Keep the current tests, add parser/property/fuzz tests, completion fixtures, config migration tests and platform PTY jobs. |
| CI | Linux test, clippy and musl release | Multiple build/test/documentation paths | Add format, docs, audit, sanitizer/fuzz smoke, cross-target and release reproducibility jobs. |
| Documentation | README, man page, porting notes and changelog | Tutorial, language guide, interactive guide, FAQ, generated command docs and help | Add a tutorial, language guide, command reference generated from metadata, migration guide and compatibility table. |
| Diagnostics | Precise parser/config errors in many paths | `help`, per-builtin `-h`, debug categories, startup/command profiling | Add stable diagnostics, debug categories, `--no-execute`, startup profile and structured error codes. |
| Localization | English-only user-facing strings | Translation tooling and catalogs | Separate message IDs from formatting and add localization only after the builtin/help registry is stable. |
| Packaging | Arch packages and one static Linux artifact | Native packages/installers across several systems | Add signed checksums/provenance, install/uninstall tests, shell registration docs and a migration-safe config path. |
| Release process | Frequent focused releases around v3.5 | Long-lived project with signed releases and a large contributor surface | Publish a v4 compatibility policy, deprecation window, release checklist and rollback path. |
| Security | Safe declarative config; JSON capture hardened | Mature config/vendor path conventions and security policy | Keep non-executing config default, add permission checks for persistent state and document trust boundaries. |

## What must not be copied blindly

1. **Fish syntax cannot be added as a collection of aliases.** The parser and
   expansion model are different. Fish uses `end`, `switch`, list expansion,
   newline command-substitution splitting and no implicit word splitting. The
   current Sharky parser is deliberately Bash-like and is checked against a
   Ruby oracle. A hidden mode switch would produce confusing scripts and
   incompatible status/argument behavior.

2. **Executable configuration needs an explicit trust boundary.** Fish runs
   `config.fish`, `conf.d` files and autoloaded scripts. Sharky currently makes
   configuration data-only. v4 can add an opt-in `--allow-config-scripts` (or
   a separate profile directory) after documenting the security model; the
   safe declarative config should remain available for automation.

3. **Fish's process model is not a checklist.** Sharky already has native
   process substitution and Bash-style traps. Fish intentionally relies on
   different primitives in some areas. Preserve behavior that is a Sharky
   strength when it does not conflict with the v4 contract.

4. **A plugin manager is not maturity.** Stable search paths, file formats,
   metadata, help and tests create an ecosystem without making the core own a
   package manager.

## Recommended v4 contract

### Compatibility

- Keep the current Bash-like language as `sharky` default.
- Version the language contract separately from the binary (`language v1` or a
  feature flag) so future fish syntax can be introduced without silent
  semantic changes.
- Preserve `config.sharky` and `theme.sharky`; add new directories and files
  without making old configs executable.
- Stop exporting every variable implicitly. Introduce explicit export state,
  ship a compatibility switch for one major release, and document the change.
- Preserve current `PATH` string behavior in scripts while exposing a path-list
  view to new builtins and the reader.

### Runtime interfaces

The current `Shell` stores execution, variables, functions, reader settings,
jobs and prompt state in one large module. Before adding many builtins, create
small internal interfaces with tests at each seam:

1. **Value store:** scalar/list/path values, export flag, scope stack and
   universal persistence.
2. **Function store:** definitions, metadata, autoload lookup, save/edit and
   event registration.
3. **Builtin registry:** name, synopsis, help, completion metadata and handler.
4. **Completion engine:** candidates, descriptions, replacement span, source,
   priority and lazy providers.
5. **Reader/pager:** buffer spans, modes, kill ring, search, selection and
   terminal redraw.
6. **Config resolver:** declarative files, optional scripts, conf.d order,
   trust checks and migration diagnostics.

These interfaces let v4 add fish-like behavior without making every feature a
new branch in `shell.rs` or `editor.rs`.

### Delivery order

| Milestone | Deliverable | Exit gate |
| --- | --- | --- |
| 4.0-alpha1 | Versioned builtin registry, help output, explicit export state, private history, atomic universal store format | Existing 301 tests remain green; migration fixtures cover old configs/history. |
| 4.0-alpha2 | Function/completion paths, autoloading, `complete`, `bind`, `functions`, `status`, `command`, `builtin`, `fish_add_path` | User and vendor fixtures load deterministically; no arbitrary config runs by default. |
| 4.0-beta1 | Semantic highlighter, completion descriptions, searchable pager, token history search, kill ring, right prompt and title hooks | PTY suite covers resize, wide characters, signals, paste, pager cancellation and terminal restoration. |
| 4.0-beta2 | Directory history/stack, data builtins, event handlers, prompt presets, migration tooling and full docs | Script and interactive docs are generated from the same metadata as `help`. |
| 4.0-rc1 | Portability and release hardening | Linux is stable; macOS/BSD support is either tested and supported or explicitly deferred. Reproducible artifacts and signed checksums exist. |
| 4.0.0 | Public major release | Compatibility table, migration guide, deprecations and rollback instructions are published. |

## File and test impact

The minimum complete implementation will reach these existing areas:

- `rust/src/shell.rs`: runtime state, builtins, function lookup, scopes,
  events, path handling and compatibility flags.
- `rust/src/lexer.rs` and `parser.rs`: only for new commands or an explicitly
  versioned language mode; do not mix fish grammar into the default parser.
- `rust/src/editor.rs`, `highlight.rs` and `term.rs`: semantic roles, pager,
  kill ring, key sequences, clipboard hooks, right prompt and title redraw.
- `rust/src/history.rs`: structured records, sessions, private mode, merge,
  locking and migration from the current escaped-line file.
- `rust/src/prompt.rs`: executable hooks/presets, right prompt, mode prompt,
  title and width-aware rendering.
- `rust/config/src/lib.rs`: new declarative instructions, validation and
  migration diagnostics while preserving the current data-only behavior.
- `rust/src/cli.rs`: `--private`, `--no-execute`, `--init-command`, debug and
  profiling options, plus explicit compatibility flags.
- `rust/tests/`: unit, CLI, PTY, conformance, migration, persistence,
  completion, fuzz/property and platform-specific tests.
- `README.md`, `rust/README.md`, `rust/man/sharky.1`, `PORTING.md`,
  `CHANGELOG.md`, packaging and CI: user contract, release policy and support
  matrix.

## Acceptance criteria for calling Sharky mature

- The builtin registry generates `help`, man-page sections and completion
  metadata from one source of truth.
- Config, function, completion and universal-variable files have documented
  precedence, permission checks, atomic writes and corruption behavior.
- The reader has a reusable pager, searchable history, kill ring, semantic
  errors, completion descriptions, right prompt and programmable title.
- Every new stateful feature has a non-PTY unit contract and a PTY regression
  where terminal behavior matters.
- The current Bash-like conformance suite remains green unless a change is
  explicitly listed in the v4 migration guide.
- CI checks formatting, clippy, docs, tests, release packaging and at least
  one sanitized or fuzzed parser/expansion path.
- Linux support is release-grade; other targets are either tested or clearly
  marked unsupported in the package and documentation.
- Startup, command execution, history and pager behavior have bounded work and
  cancellation paths, with benchmark thresholds recorded in CI.

## Immediate next slice

The first code slice should be deliberately small and high leverage:

1. Add a builtin metadata registry and `help`/`builtin`/`command`/`status`
   queries.
2. Add a versioned universal-variable file with atomic, locked persistence and
   `--private` history mode.
3. Add function and completion search paths with one autoload fixture each.
4. Add completion candidates with descriptions and route the existing inline
   menu through the new provider interface.
5. Add a pager abstraction and migrate Ctrl-R to it.

This slice establishes the seams that the rest of the Fish parity work needs,
while retaining the current language and keeping the diff reviewable.

## Sources

- [Sharky README](../../README.md)
- [Sharky Rust README](../../rust/README.md)
- [Sharky porting notes](../../rust/PORTING.md)
- [Sharky man page](../../rust/man/sharky.1)
- [fish introduction](https://fishshell.com/docs/current/)
- [fish interactive use](https://fishshell.com/docs/current/interactive.html)
- [fish language](https://fishshell.com/docs/current/language.html)
- [fish for Bash users](https://fishshell.com/docs/current/fish_for_bash_users.html)
- [fish command options](https://fishshell.com/docs/current/cmds/fish.html)
- [fish `set`](https://fishshell.com/docs/current/cmds/set.html)
- [fish `complete`](https://fishshell.com/docs/current/cmds/complete.html)
- [fish `function`](https://fishshell.com/docs/current/cmds/function.html)
- [fish `abbr`](https://fishshell.com/docs/current/cmds/abbr.html)
- [fish `history`](https://fishshell.com/docs/current/cmds/history.html)
- [fish `status`](https://fishshell.com/docs/current/cmds/status.html)
- [fish `fish_config`](https://fishshell.com/docs/current/cmds/fish_config.html)
- [fish contributing and testing](https://fishshell.com/docs/current/contributing.html)
- [fish repository](https://github.com/fish-shell/fish-shell)
