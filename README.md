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

sharky is not a login shell in v0.1. Start it from Bash or another shell and test it there first.

## Install

Put `bin/sharky` on `PATH` with a symlink or a small wrapper:

~~~sh
mkdir -p ~/.local/bin
ln -s "$PWD/bin/sharky" ~/.local/bin/sharky
~~~

Make sure `~/.local/bin` is on `PATH`. The source checkout must stay in place because `bin/sharky` loads `lib/` from it.

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
~~~

Config errors print `sharky: config error in <file>: <message>` and the shell continues with defaults. Configuration and function files are user-owned Ruby and run with your privileges.

`reload` re-evaluates `sharky.rb` and clears aliases, abbreviations, bindings, completions, prompt and theme state. Function files that are already loaded stay in memory; start a new shell after editing one. New function files load on first use.

## Supported language

sharky implements a small Bash-like subset at the prompt and in `sharky -c`:

- single and double quotes, backslash escapes;
- `$VAR`, `${VAR}`, `$?`, `$$`, `$!`, and tilde expansion;
- globbing with `*`, `?` and `[...]`;
- command substitution `$(...)`, including nesting;
- pipes, redirects (`>`, `>>`, `<`, `2>`, `2>>`, `2>&1`, `&>`);
- lists with `;`, `&&`, `||`, background `&`, and subshells `( ... )`;
- comments and backslash-newline continuation.

The following are not supported: arrays, `[[ ]]`, arithmetic expansion, brace expansion, here-documents, process substitution, extglob, shell functions, `if`/`for`/`while`/`case`, traps, `source` of Bash files, `set -e`, and custom `IFS`.

Unsupported reserved words and operators at command position produce:

~~~text
sharky: unsupported syntax: 'for'. Use bash -c '...' or a Ruby function.
~~~

The status is 2 and nothing runs. sharky never falls back to Bash silently.

## Bash scripts

Bash remains the scripting language for `.sh` files:

~~~sh
bash script.sh
~~~

sharky does not intercept or emulate Bash scripts. An executable script without a shebang fails with status 126 and a hint to run `bash file` or add a shebang; it is not run by `/bin/sh`.

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
| `sharky --no-config -c true` | ~36 ms |

Fish's interactive startup is commonly in the 20–30 ms range; sharky is in that
range, not Bash's. sharky does **not** claim to be the fastest shell. The v1
budget is startup under 50 ms and prompt p95 under 20 ms, with no external
process in the default path.

## Tests and benchmarks

~~~sh
ruby test/run.rb
ruby bench/startup.rb
ruby bench/parser.rb
~~~

Benchmarks print their results and do not assert in CI.

## Roadmap

v0.1 is complete. v1.0 is the stable daily-driver release: installable,
job-control hardened, CI-tested, documented, and built around the session
contract.

## License

sharky is free software: you can redistribute it and/or modify it under the
terms of the GNU General Public License as published by the Free Software
Foundation, version 3 only.

See [LICENSE](LICENSE). Copyright (C) 2026 mafuzyk.
