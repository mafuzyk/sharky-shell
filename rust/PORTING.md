# Porting sharky to Rust

The Ruby implementation is frozen at tag `v1-ruby`. It is the behaviour oracle:
read it, run it, but do not edit it.

## Layout

- `src/` — the Rust shell. `lexer.rs` (tokens and word parts), `shell.rs`
  (lists, pipelines, expansion, builtins), `exec.rs` (PATH lookup, fork/exec,
  wait), `cli.rs` (arguments).
- `conformance/cases/*.sh` — one command per file.
- `tests/differential.rs` — runs every case through a local Ruby v1 checkout
  (`.ruby-oracle/bin/sharky` by default) and the Rust binary, comparing stdout,
  stderr and exit status. Skipped when no reference binary is present; override
  with `SHARKY_REF=/path/to/sharky`.
- `tests/cli.rs` — black-box behaviour tests against the built binary.

## Commands

Builds and tests run inside a memory-capped scope so a runaway test cannot take
the machine down, and with six parallel jobs:

```sh
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo test -j 6
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo clippy -j 6 --all-targets
systemd-run --user --scope --quiet -p MemoryMax=2G -- cargo build --release -j 6
```

`.cargo/config.toml` already pins `jobs = 6`; keep the scope wrapper for cargo
test runs. A hang inside a shell test used to grow to 8-9 GB: write tests that
assert an exit status instead of waiting forever.

## Milestones

- [x] Freeze Ruby at `v1-ruby`
- [x] Differential harness and first conformance cases
- [x] CLI, `-c`, exit status, external commands
- [x] Quotes, escapes, comments, `$VAR`, field splitting
- [x] Lists (`;`, `&&`, `||`), `exit`, `cd`, `:`, `true`, `false`
- [x] Pipelines, redirects (`>`, `>>`, `<`, `N>`, `2>&1`)
- [x] Assignments, prefix assignments, `export`, `unset`
- [x] `$(...)`, `${...}` operators, `$((...))`, globbing, tilde
- [x] Brace expansion, here-strings, `&>`, `${var/pat/repl}`
- [x] Extended parameter expansion: substrings, case, indirect, `$'...'`
- [x] Associative arrays, array slices and arithmetic indexes
- [x] C-style `for`, `;;&`/`;&`, extglob and arithmetic assignment
- [x] Process substitution with reaped children and closed fds
- [x] Interactive history expansion with `set -H`
- [x] `exec` (with `-a` and persistent redirections) and `time`/`time -p`
- [x] Bracketed paste and multiline editing with per-line motion
- [x] History autosuggest with ghost text and Right/End accept
- [x] Indexed arrays: literals, `[@]`, index, `#`, `+=` and `a[i]=`
- [x] `set -e/-u/-x`, `pipefail`, `set --`, `trap` EXIT, `read` and `shift`
- [x] `[[ =~ ]]`, `-a`, `-L/-h`, integer coercion
- [x] `type`, `alias`/`unalias`, `history`, `pwd`
- [x] Starship prompt integration (`tide starship on`)
- [x] Abbreviations (`shoal`, `abbr`/`unabbr`) with space and Enter expansion
- [x] Ctrl-R incremental history search
- [x] Parser and AST: `if`, `for`, `while`, `until`, `case`, functions,
      groups, subshells, `!`
- [x] `[[ ]]` conditionals and heredocs (`<<`, `<<-`, quoted delimiters)
- [x] `((...))` as a command
- [x] `source`, script files, positional parameters in scripts
- [x] Interactive REPL: raw-mode line editor, prompt, history, completion,
      process groups and terminal handoff
- [x] Job control: `&`, `jobs`, `fg`, `bg`, Ctrl-Z, process groups and
      terminal handoff (child signal dispositions are reset before exec)
- [ ] Job control, signals, `trap`
- [x] Configuration parser: `config.sharky` and `theme.sharky`
- [x] Koral shell integration marks (OSC 133 and OSC 7)
- [x] `sharky-config` crate shared with Koral
- [ ] Configuration applied by the interactive shell (prompt, keybinds)
- [x] AUR packaging

## Decisions

- **Configuration is a small declarative language, not command syntax.**
  `src/config.rs` owns a parser that never sees shell commands, so a config
  typo can never execute anything. One statement per line:
  `instruction property value`, `//` comments, `"quoted values"`, and
  `@colour` references validated against `coral` definitions.

  | Instruction | Purpose |
  | --- | --- |
  | `tide` | shell preferences (`greeting`, `autosuggest`, `highlight`, `history`) |
  | `pearl` | environment variables |
  | `current` | prepend a directory to `PATH` |
  | `reef` | aliases |
  | `shoal` | abbreviations |
  | `anchor` | key bindings, validated against a fixed action list |
  | `coral` | named colours |
  | `wave` | prompt template and highlight styles |
  | `wake` | commands run once before the first interactive prompt |

  Unknown instructions, wrong arity, bad colours, unknown keybind actions and
  dangling `@colour` references are errors with file and line, reported without
  stopping the shell.
- **The configuration parser is a crate.** `rust/config` is `sharky-config`,
  versioned separately and pinned by Koral as a git dependency. It has no
  dependencies and no knowledge of the shell, so `src/config.rs` never returns.
  The tag `sharky-config-v0.1.0` is created only when the maintainer asks.
- **No async runtime.** A shell is fork/exec plus poll; tokio would fight the
  fork model.
- **Single-threaded at fork time.** Between `fork` and `exec` only
  async-signal-safe work happens; redirect targets and argv/env are prepared
  before forking.
- **The differential suite grows with every feature.** A new feature lands with
  at least one `conformance/cases` file so the Ruby oracle keeps checking it.

## Known shortcut

All shell variables are exported to children. Bash only exports those marked
with `export`. Revisit when `env`-visible behaviour starts to matter.
