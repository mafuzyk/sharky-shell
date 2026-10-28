# Contributing to sharky

sharky is a Unix-first interactive shell written in Ruby. Please keep changes
inside the compatibility boundary documented in the README and the v1 scope
summarized there.

## Setup

- Linux
- Ruby 3.4
- No Bundler and no third-party runtime gems

Run from a checkout:

```sh
bin/sharky --version
ruby test/run.rb
```

Do not run `bin/sharky` as your login shell yet. It is a session shell until the
login stability gate in the roadmap passes.

## Before opening a pull request

1. Add a failing test for the behavior, then make it pass.
2. Run `ruby test/run.rb` and `ruby -w test/run.rb`.
3. For editor, prompt, job-control or terminal changes, exercise a real PTY.
4. Read the diff and keep unrelated formatting out of the commit.

## Style

- Code, identifiers, file names, comments, docs, commits and user-facing
  messages are English.
- Prefer small, explicit Ruby modules over frameworks.
- Runtime code uses the Ruby standard library and default gems only.
- Do not add a silent Bash fallback. Unsupported syntax must keep returning the
  documented status-2 error.
- Keep behavior changes and documentation in the same change when possible.

## Commits

Use Conventional Commits, one logical change per commit. Examples:

```text
feat: add session result envelope
fix: restore terminal after a stopped job
docs: explain the compatibility boundary
```

Do not add `Co-authored-by` trailers or AI attribution.

## Tests

- Unit and acceptance tests live in `test/`.
- PTY tests must restore state and must not leave child processes behind.
- A change that cannot be covered by a test needs an explanation in the pull
  request.

## License

By contributing, you agree that your contribution is licensed under the GNU
General Public License, version 3 only. See [LICENSE](LICENSE).
