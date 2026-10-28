# Changelog

All notable changes to sharky are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses semantic
versioning.

## [Unreleased]

### Added

- Built-in prompt presets and optional Starship provider.
- Design docs for the v1 session contract.

### Changed

- Documented the v1.0 scope: session shell, no login-shell installation until
  the stability gate passes.

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
