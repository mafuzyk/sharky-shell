# Porting sharky to Rust

The Ruby implementation is frozen at tag `v1-ruby`. It is the behaviour oracle:
read it, run it, but do not edit it.

## Layout

- `src/` — the Rust shell. `lexer.rs` (tokens and word parts), `shell.rs`
  (lists, pipelines, expansion, builtins), `exec.rs` (PATH lookup, fork/exec,
  wait), `cli.rs` (arguments).
- `conformance/cases/*.sh` — one command per file.
- `tests/differential.rs` — runs every case through `../bin/sharky` and the
  Rust binary, comparing stdout, stderr and exit status. Skipped when no
  reference binary is present; override with `SHARKY_REF=/path/to/sharky`.
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
- [ ] `$(...)`, `${...}` operators, `$((...))`, globbing
- [ ] Parser and AST: `if`, `for`, `while`, `case`, functions, `[[ ]]`
- [ ] `source`, scripts, shebang, positional parameters
- [ ] Job control, signals, `trap`
- [ ] Interactive: line editor, history, completion, prompt
- [ ] Configuration (see below)
- [ ] AUR packaging

## Decisions

- **Configuration is the shell's own language, fish-style.** `config.sh` is
  executed by the shell at startup; there is no embedded interpreter and no
  separate config DSL. Builtins such as `set`, `alias`, `abbr`, `bind` and
  `complete` configure the session, and the prompt is a function
  (`function prompt; ...; end`). This keeps one grammar and one parser.
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
