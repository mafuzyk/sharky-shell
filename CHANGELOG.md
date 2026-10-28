# Changelog

All notable changes to sharky are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses semantic
versioning.

## [Unreleased]

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
