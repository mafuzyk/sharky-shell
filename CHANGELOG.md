# Changelog

All notable changes to sharky are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses semantic
versioning.

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
