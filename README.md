# sharky

[![Ruby 3.4](https://img.shields.io/badge/ruby-3.4-CC342D?logo=ruby&logoColor=white)](https://www.ruby-lang.org/)
[![Linux](https://img.shields.io/badge/platform-Linux-FCC624?logo=linux&logoColor=black)](https://www.kernel.org/)
[![License: GPL--3.0](https://img.shields.io/badge/license-GPL--3.0-blue)](LICENSE)
[![ci](https://github.com/mafuzyk/sharky-shell/actions/workflows/ci.yml/badge.svg)](https://github.com/mafuzyk/sharky-shell/actions/workflows/ci.yml)

sharky is a Unix-first interactive shell written in Ruby. Commands run as real Unix processes; the parser, executor, job control, prompt, editor and completion are owned by the shell.

Internal design notes and implementation plans are kept outside the published repository.

## Requirements

- Ruby 3.4 on Linux
- Standard library and default gems only; no third-party runtime gems

sharky is not a login shell in v1.0. Start it from Bash or another shell and test it there first.

## Install

Install into a prefix (default `/usr/local`):

~~~sh
make install
# user-local:
make install PREFIX="$HOME/.local"
# packaging:
make install DESTDIR="$pkgdir" PREFIX=/usr
~~~

This installs `bin/sharky`, `lib/sharky.rb`, `lib/sharky/*.rb` and `man/man1/sharky.1`; the installed binary no longer depends on the source checkout. Read the manual with `man sharky` (or `man -l man/sharky.1` from a checkout). Remove with:

~~~sh
make uninstall PREFIX="$HOME/.local"
~~~

For a throwaway checkout without installing, symlink `bin/sharky` and keep the source tree in place:

~~~sh
mkdir -p ~/.local/bin
ln -s "$PWD/bin/sharky" ~/.local/bin/sharky
~~~

Build a source tarball with `make dist`; it includes tests and benchmarks so `make test` also works from the extracted source. The Arch recipe is `packaging/PKGBUILD`.

## First run

~~~sh
sharky --version
sharky -c 'printf "a b\n" | wc -w'
sharky
~~~

`-c` runs one command string and exits with its status. With no arguments, sharky starts interactively and requires a TTY. `--no-config` skips the configuration file.

## Machine-readable output

`sharky --json -c COMMAND` writes exactly one JSON object to stdout and exits with the command status:

~~~json
{"schema_version":1,"status":0,"stdout":"hi\n","stderr":"","duration_ms":3,"cwd":"/tmp","env_delta":{"FOO":"bar"}}
~~~

`sharky state --json` writes the current shell state without the environment:

~~~json
{"schema_version":1,"cwd":"/tmp","last_status":0,"pid":1234}
~~~

Pass `--env` to include the full environment explicitly:

~~~json
{"schema_version":1,"cwd":"/tmp","env":{"PATH":"/usr/bin"},"last_status":0,"pid":1234}
~~~

The default omits environment variables so secrets are not written to logs or agent transcripts. With `--env`, `state.env` is the full environment. `env_delta` maps each changed environment name to its new value; a removed variable is `null`. `pid` is the sharky process. JSON mode keeps stdout machine-readable: configuration warnings and other diagnostics go to stderr, while command stderr is captured in the envelope.

## Configuration

The configuration directory is `$SHARKY_CONFIG`, or `${XDG_CONFIG_HOME:-~/.config}/sharky`:

- `sharky.rb` is loaded at startup and by the `reload` builtin.
- `themes/<name>.toml` is loaded on demand by `Sharky.theme :name`; a `.rb` theme is the fallback for computed themes.
- `functions/<name>.rb` becomes the command `<name>`, autoloaded on first use.

`init.rb` is still read as a legacy fallback with a warning; use `sharky.rb` for new configurations.

~~~ruby
# ~/.config/sharky/sharky.rb
Sharky.prompt = ->(ctx) { "#{ctx.cwd} #{ctx.git_branch} ❯ " }
Sharky.alias "ll", "eza -lah"
Sharky.abbr "gco", "git checkout"
Sharky.bind "ctrl-r", :history_search

Sharky.command :hello do |argv, _stdin|
  puts "hello #{argv.first || 'world'}"
  0
end

Sharky.complete "deploy" do |word, _line, _cursor|
  %w[staging production].grep(/\A#{Regexp.escape(word)}/)
end
~~~

Config errors print `sharky: config error in <file>: <message>` and the shell continues with defaults. Configuration and function files are user-owned Ruby and run with your privileges.

`reload` re-evaluates `sharky.rb` and clears aliases, abbreviations, bindings, completions, prompt and theme state. Function files that are already loaded stay in memory; start a new shell after editing one. New function files load on first use.

## Supported language

sharky implements a native Bash-like subset at the prompt, in `sharky -c`, and in `.sh` scripts:

- single and double quotes, escapes, comments and backslash-newline continuation;
- variables, positional parameters, parameter and tilde expansion, arithmetic expansion `$((...))` and arithmetic commands `((...))`;
- indexed arrays, brace expansion, globbing and nested command substitution `$(...)`;
- pipes, redirects, here-documents, here-strings, lists with `;`/`&&`/`||`, background jobs, subshells and brace groups;
- `[[ ]]`, `if`/`elif`/`else`, `for`, `while`, `until`, `case`, shell functions, `break`, `continue`, `return`, `source`/`.`, `set -e/-u/-x`, `pipefail` and `trap` (EXIT actions execute).

This is not full Bash/POSIX compatibility. Unsupported features include associative arrays, array slices, process substitution, extglob, `coproc`, custom `IFS` and full `[[ =~ ]]` parity. Bash `complete`/`compgen` are not emulated; Ruby configs can register a completion block with `Sharky.complete`, but context detection and quote/escape insertion there are still limited. Signal traps are stored, but only EXIT actions execute in v1.

Unsupported reserved words and operators at command position produce:

~~~text
sharky: unsupported syntax: 'coproc'. Use bash -c '...' or a Ruby function.
~~~

The status is 2 and nothing runs. sharky never falls back to Bash silently.

## Shell scripts

`.sh` files and scripts with a `#!/usr/bin/env sharky` shebang run through sharky's native parser:

~~~sh
sharky script.sh arg1 arg2
~~~

The script path is `$0`; arguments are available as `$1`, `$2`, `$@` and `$#`. Use `source FILE [ARG...]` (or `. FILE`) to run a file in the current shell.

For scripts that need unsupported Bash features, run `bash script.sh` explicitly. sharky never silently falls back to Bash or `/bin/sh`. Launching an executable script directly without a shebang fails with status 126 and a hint to select an interpreter or add a shebang.

## Ruby

- `ruby` starts IRB in-process with the `Sharky` API and shell helpers.
- `sharky script.rb` runs a Ruby script with the API.
- `Sharky.sh("line")` and `Sharky.capture("line")` use the parser and executor.
- `Sharky.cwd`, `Sharky.env`, `Sharky.last_status` and `Sharky.pid` expose shell state.

## Performance

Startup is dominated by the Ruby VM, not by shell work. On the reference
machine (Arch Linux, NTFS checkout):

| Command | Median |
| --- | ---: |
| `bash -c true` | ~1 ms |
| `fish --no-config -c true` | ~2 ms |
| `ruby --disable-gems -e ''` | ~7 ms |
| `Sharky::Prompt.render` (renderer only, prebuilt context) | ~0.003 ms median / p95 |
| `sharky --no-config -c true` | ~42 ms |

Fish's interactive startup is commonly in the 20–30 ms range; sharky is in that
range, not Bash's. sharky does **not** claim to be the fastest shell. The v1
target is startup under 50 ms and interactive prompt p95 under 20 ms. Measured
on the reference machine: startup median 41.9 ms (`--no-config -c true`) and
renderer p95 0.003 ms with prebuilt context. The interactive prompt also runs
`git status` for branch and dirty state, with a short cache and a timeout; that
work is not included in the renderer measurement. `ruby bench/prompt.rb` measures
rendering only, not the complete interactive prompt. See also `ruby bench/startup.rb`.

## Tests and benchmarks

~~~sh
ruby test/run.rb
ruby bench/startup.rb
ruby bench/parser.rb
ruby bench/prompt.rb
ruby bench/soak.rb
~~~

The suite includes a Bash conformance matrix and PTY tests. Benchmarks print
their results; the CI soak job runs repeated command and PTY sessions.

## Roadmap

v1.0 is the stable daily-driver release: installable, job-control hardened,
CI-tested, documented, and built around the session contract. Login-shell
installation remains post-v1.

## License

sharky is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only.

See [LICENSE](LICENSE). Copyright (C) 2026 mafuzyk.
