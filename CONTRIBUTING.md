# Contributing to sharky

sharky is a Unix-first interactive shell written in Rust. Please keep changes
inside the compatibility boundary documented in the README.

## Setup

- Linux
- Rust 1.85 or newer

Run from a checkout:

```sh
cd rust
cargo test -j 6
cargo clippy -j 6 --all-targets
```

Do not run sharky as your login shell yet. It is a session shell until the
login stability gate passes.

## Differential oracle

The frozen Ruby v1 implementation is not part of this tree. The differential
suite compares against a local checkout and is skipped when it is absent:

```sh
git archive v1-ruby | tar -x -C .ruby-oracle
SHARKY_REF=.ruby-oracle/bin/sharky cargo test -j 6 --test differential
```

`.ruby-oracle/` is gitignored. Do not commit it.

## Before opening a pull request

1. Add a failing test for the behavior, then make it pass.
2. Run `cargo test -j 6` and `cargo clippy -j 6 --all-targets`.
3. For editor, prompt, job-control or terminal changes, exercise a real PTY.
4. Read the diff and keep unrelated formatting out of the commit.

## Style

- Code, identifiers, file names, comments, docs, commits and user-facing
  messages are English.
- Prefer small, explicit modules over abstractions added for a single caller.
- No async runtime: the shell is fork/exec plus poll.
- No silent Bash fallback. Unsupported syntax must keep returning the
  documented status-2 error.
- Configuration is data, never code. Do not add an instruction that executes a
  command or loads a script.
- Keep behavior changes and documentation in the same change when possible.

## Commits

Use Conventional Commits, one logical change per commit. Examples:

```text
feat(rust): add indexed arrays
fix(rust): restore the terminal after a stopped job
docs: explain the sharky configuration language
```

Do not add `Co-authored-by` trailers or AI attribution.

## Tests

- Black-box tests live in `rust/tests/`.
- Differential cases live in `rust/conformance/cases/`, one command per file.
- PTY tests must restore state and must not leave child processes behind.
- A change that cannot be covered by a test needs an explanation in the pull
  request.

## License

By contributing, you agree that your contribution is licensed under the GNU
General Public License, version 3 only. See [LICENSE](LICENSE).
