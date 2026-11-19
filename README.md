# sharky

An interactive Unix shell written in Rust.

sharky runs commands as real Unix processes. The parser, executor, line editor,
prompt, history, completion and job control belong to the shell itself; it never
wraps Bash or `/bin/sh`.

- one static binary, no runtime dependency, startup around half a millisecond;
- a Bash-like language subset, from pipes and heredocs to `[[ ]]` and functions;
- an interactive editor with history and completion, plus real job control;
- a small declarative configuration language for settings and themes.

## Status

sharky is in alpha. Alpha names an ongoing stage of maturity, not a separate
release track: the packages below are the normal release channel and follow the
project's regular cadence. Expect behaviour to change between releases; the
language subset and configuration format are not yet frozen.

The Ruby v1 implementation is frozen at tag `v1-ruby`. It is not published in
this tree: the source of truth is the Rust code under `rust/`. Keep a local
checkout at `.ruby-oracle/` (gitignored) only when you need the differential
oracle.

## Install

### Arch Linux (AUR)

```sh
paru -S sharky-shell-bin       # prebuilt static binary (x86_64)
paru -S sharky-shell-git       # builds the latest main locally
```

`sharky-shell-bin` is the prebuilt release package: it installs the same binary
published on the GitHub release. `sharky-shell-git` tracks `main` and needs a
Rust toolchain.

### Nix (flake)

```sh
nix profile install github:mafuzyk/sharky-shell
nix run github:mafuzyk/sharky-shell#sharky-bin -- --version
```

The flake installs the official prebuilt static binary for `x86_64-linux`,
pinned to the latest verified release tarball and checked against its published
SHA256. It also installs the man page and the example `config.sharky` and
`theme.sharky` under `share/sharky`. There is no source build or wrapper script.

`default` and `sharky-bin` serve the same derivation. When a newer release
artifact is verified, the version and hash in the root `flake.nix` are updated
in place, so both names pick up the new version.

### Prebuilt binary (GitHub)

```sh
curl -fsSLO https://github.com/mafuzyk/sharky-shell/releases/latest/download/sharky-x86_64-unknown-linux-musl.tar.gz
tar -xzf sharky-x86_64-unknown-linux-musl.tar.gz
install -Dm755 sharky ~/.local/bin/sharky
install -Dm644 sharky.1 ~/.local/share/man/man1/sharky.1
```

The tarball is statically linked and also carries `config.sharky`,
`theme.sharky` and the man page.

### From source

```sh
git clone https://github.com/mafuzyk/sharky-shell.git
cd sharky-shell/rust
cargo build --release --locked --jobs 6
install -Dm755 target/release/sharky ~/.local/bin/sharky
install -Dm644 man/sharky.1 ~/.local/share/man/man1/sharky.1
```

Requires Rust 1.85 or newer and Linux.

## First run

```sh
sharky --version
sharky -c 'printf "a b\n" | wc -w'
sharky                      # interactive, needs a TTY
sharky script.sh argument   # runs a script through sharky's own parser
echo 'printf hi' | sharky   # reads stdin when there is no terminal
sharky config explain              # plain-text index of configuration topics
sharky config explain tide history # plain-text guide for one topic
```

## Agent interface

```sh
sharky --json -c 'printf hi'   # one JSON object: status, stdout, stderr, duration, cwd, env_delta
sharky state --json            # schema, cwd, last_status and pid
sharky state --json --env      # adds the environment
sharky -i                      # force the REPL; fails on a non-TTY
```

`env_delta` contains only variables changed by the command; unset variables are
`null`. Unlike the Ruby reference it never includes the `?` status pseudo-variable.

## Diagnostics

```sh
sharky status   # version, resolved config/theme/history paths and feature flags
sharky doctor   # read-only local checks, exit 1 if any check fails
```

Both subcommands run before the shell starts, so neither builds a shell, applies
`pearl`, evaluates profiles, writes files or prints environment or configuration
values. `status` reads no configuration files; `doctor` parses `config.sharky`
and `theme.sharky` read-only through the configuration parser and never applies
what it parses. `status` exits 0 when its arguments are valid
and prints one plain-text fact per line; the paths honour `SHARKY_CONFIG`,
`XDG_CONFIG_HOME`, `HOME`, `SHARKY_HISTORY` and `XDG_DATA_HOME`. `doctor` prints
an `OK` or `FAIL` line for stdin/stdout descriptor validity, config and theme
parsing, history permissions and writability, and the presence of `sh`, `ls` and
`cat` on `PATH`. A missing TTY is reported but does not fail the check; history is
inspected through metadata only, without opening it for append or taking a lock.
There are no compile-time feature flags in this release, so `status` reports
`features: none`.

## Help

```sh
sharky help                 # same text as --help
sharky help read            # help for one builtin, flag or subcommand
sharky help tide history    # one configuration topic
```

`sharky help [TOPIC...]` and the shell `help` builtin share one registry. With
no topic, both print exactly the text of `--help`. With a topic, they join every
word after `help` with a single space and print one entry. Topics cover every
builtin, the `state`, `config explain`, `status` and `doctor` subcommands and the
`-c`, `--check`, `--json` and `-i` flags, plus the configuration-language topics
listed under `config explain`. An unknown topic exits 1 with a message on stderr.
The subcommand runs before the shell starts: it never loads or creates
configuration and has no side effects.

## Login shell

`sharky -l` (or an `argv[0]` beginning with `-`) reads
`/etc/sharky/profile.sharky` and `~/.config/sharky/profile.sharky` as sharky
code, then imports only literal `NAME=value` / `export NAME=value` lines from
`/etc/profile`, `~/.profile`, `~/.bash_profile` and `~/.zshprofile`. Command
substitution and arbitrary shell code in those files never run.

Using sharky as a login shell is not recommended while it is in alpha: profile
import is deliberately limited and the login-shell stability gate has not
passed. Prefer starting sharky from an existing login shell.

## History

After each command is evaluated, sharky appends it to one shared history file
(`$SHARKY_HISTORY`, or `${XDG_DATA_HOME:-~/.local/share}/sharky/history`). The
write happens under a lock; a failed write is reported and the entry stays only
in memory, and a torn tail from a crash is dropped and cleared on the next
append. Each session keeps its own view until it runs `history merge`, which
never rewrites entries from another session. Use `-P`/`--private` to keep history
in memory and leave the file untouched. A history file that cannot be read warns
at startup and makes `history -c` fail rather than claim it cleared the file.

## Shell language

The prompt, `-c` and script files share one grammar:

| Feature | Example |
| --- | --- |
| quotes, escapes, comments | `printf '%s\n' "a b" # comment` |
| variables | `name=value`, `"$name"`, `${name}` |
| parameter operators | `${NAME:-default}`, `${NAME-default}`, `${NAME:=default}`, `${NAME=default}`, `${NAME:+word}`, `${NAME+word}`, `${NAME:?msg}`, `${NAME?msg}`, `${#NAME}`, `${NAME#pre}`, `${NAME/pat/repl}`, `${NAME//pat/repl}`, `${NAME:1:2}`, `${NAME^^}`, `${!NAME}` |
| ANSI-C quoting | `$'a\tb'`, `$'\x41\u00e9'` |
| brace expansion | `file{1..3}.txt`, `a{b,c}d` |
| indexed arrays | `a=(one two)`, `"${a[1]}"`, `"${a[@]}"`, `"${#a[@]}"`, `"${a[@]:1:2}"` |
| associative arrays | `declare -A a`, `a[key]=value`, `"${a[key]}"`, `"${!a[@]}"` |
| command substitution | `files=$(ls \| wc -l)` |
| arithmetic | `$((2 + 3 * 4))`, `((total > 10))` |
| globbing and tilde | `src/*.rs`, `~/bin`, `@(a|b).txt`, `!(tmp)`, `shopt -s globstar; **/*.rs` |
| pipelines and redirects | `a \| b > out 2>&1`, `cmd &> out`, `cmd &>> out` |
| heredocs and here-strings | `cat <<EOF` ... `EOF`, `<<-EOF`, `<<'EOF'`, `cat <<< "text"` |
| process substitution | `diff <(a) <(b)`, `tee >(cat > out)` |
| lists | `a; b`, `a && b`, `a \|\| b` |
| options | `set -e`, `set -u`, `set -x`, `set -o pipefail`, `set -H`/`+H`, `set -o vi`/`+o vi`, `set -- a b`, `shopt -s/-u globstar`, `shopt globstar` |
| traps | `trap 'cleanup' EXIT`, `trap -l` |
| read and shift | `read -r name`, `shift 2` |
| conditionals | `if`, `elif`, `else`, `fi`, `[[ -f x && "$y" =~ ^f ]]` |
| loops | `for x in a b; do ...; done`, `for ((i=0; i<3; i++))`, `while`, `until` |
| case | `case $x in f*) ...;; *) ...;; esac`, `;;&`, `;&` |
| functions | `greet() { printf 'hi %s\n' "$1"; }` |
| groups and subshells | `{ a; b; }`, `( a; b )` |
| builtins | `type`, `command`, `help`, `alias`, `unalias`, `abbr`, `unabbr`, `history`, `pwd`, `read`, `mapfile`, `readarray`, `shift`, `local`, `exec`, `time` |
| history search | Ctrl-R, type to filter, Enter runs the match, Ctrl-G cancels |
| paste and multiline | bracketed paste inserts text verbatim; Enter submits the whole buffer; Up/Down, Home/End work per line |
| autosuggest | history suffix ghost text; Right/End accepts, `tide autosuggest off` disables |
| completion | Tab completes commands, variables, files and git branches; several matches open a menu (Tab/Shift-Tab, Enter, Esc) |
| vi mode | `set -o vi`; Esc for normal mode, `h l 0 $ w b e j k`, `x dd dw d$ D cw C p P u r`, counts; `set +o vi` returns to emacs |
| fuzzy files | Ctrl-T opens a ranked file list; type to filter, arrows to move, Enter inserts, Esc cancels |
| syntax highlighting | `wave command`, `builtin`, `string`, `variable`, `operator`, `path`, `comment`, `error`, `overwrite`; `tide highlight off` disables |
| history expansion | `!!`, `!n`, `!-n`, `!prefix`, `!?text?`, `!$`, `!*`, `!^`, `^old^new^`; `set +H` disables |
| job control | `cmd &`, `jobs`, `fg`, `bg`, Ctrl-Z |
| scripts | `source lib.sh`, `sharky file.sh args` |

`shopt` supports exactly one option, `globstar`. It is off by default:
`shopt -s globstar` makes `**` match across zero or more directories,
`shopt -u globstar` restores ordinary single-level globbing, and
`shopt globstar` prints `globstar` followed by a tab and `on` or `off`, with a
matching exit status. Other option names, combined flags such as `-su`, and
interleaved `-s`/`-u` pairs are rejected; the setting is per shell and is
neither saved in the config files nor inherited by a new process. Quoted `**`
stays literal, hidden entries are skipped unless the component starts with `.`,
repeated matches are deduplicated, and `./` and `../` prefixes are preserved. A
directory
reached implicitly through a symlink is listed as an entry but not descended by
a leading `**`; a `**` that follows a path prefix descends symlinked directories,
and naming a symlink directly keeps normal directory behavior. Only leading
`./` and `../` components are normalized: a `.` or `..` component later in the
path (for example `dir/./**/*.txt`) is not expanded and the pattern stays
literal.

Unquoted expansions split on `IFS` (space, tab and newline by default):
repeated delimiters keep empty fields, leading and trailing ones are dropped,
and an empty `IFS` disables splitting. A quoted `"$*"`, `"${a[*]}"` or
`"${!a[*]}"` joins fields with the first character of `IFS`, or nothing when
`IFS` is empty.

This is a subset of Bash, not a drop-in replacement. Unsupported syntax fails
with status 2 instead of silently falling back to another shell.

Known limits: ANSI-C escapes above 0x7f are UTF-8 characters, not raw bytes; a
process substitution created inside a pipeline is reaped by its parent process,
so the top-level `wait` does not observe it. `TIMEFORMAT` is out of scope.

## Configuration

Configuration lives in `$SHARKY_CONFIG`, or
`${XDG_CONFIG_HOME:-~/.config}/sharky`. Two files are read at startup:
`config.sharky` and `theme.sharky`.

The format is deliberately not shell code. It is a declarative language with
one statement per line, `//` comments, optional double quotes, and no `=`,
braces or semicolons. A configuration file cannot execute anything.

```
instruction property value
```

Values are bare words or `"quoted strings"`. Inside quotes, `\"`, `\\`, `\n`
and `\t` are escapes. Anything after `//` outside quotes is a comment.

### `config.sharky`

| Instruction | Meaning | Example |
| --- | --- | --- |
| `tide` | shell preference | `tide history 10000` |
| `pearl` | environment variable | `pearl EDITOR "nvim"` |
| `current` | prepend a directory to `PATH` | `current "~/.cargo/bin"` |
| `reef` | alias | `reef ll "ls -lah"` |
| `shoal` | abbreviation, expanded on space or Enter | `shoal gco "git checkout"` |
| `wake` | run a command at interactive startup | `wake "fastfetch"` |
| `anchor` | key binding | `anchor ctrl+r history_search` |

`tide` keys:

| Key | Values |
| --- | --- |
| `greeting` | `on` / `off` |
| `autosuggest` | `on` / `off` |
| `highlight` | `on` / `off` |
| `history` | number of history entries to keep |
| `starship` | `on` / `off`; use `starship prompt` for the prompt |
| `profile_paths` | `on` / `off`; import `PATH` additions from shell profiles |

`profile_paths` is on by default. Sharky reads simple `PATH` additions from
`~/.profile`, `~/.bash_profile`, `~/.bash_login`, `~/.bashrc`, `~/.zshrc`,
`~/.zprofile` and `~/.config/fish/config.fish` without executing them: only
`export PATH=...` values built from literals, `~`, `$HOME` and `$PATH`, plus
`fish_add_path` and `set -gx PATH`. Anything else is ignored, so command
substitution and arbitrary shell code never run. Use `current` for anything
more complex, and `tide profile_paths off` to disable the scan.

`anchor` actions: `history_search`, `complete`, `clear_screen`, `accept_line`,
`abort`, `backward_char`, `forward_char`, `backward_word`, `forward_word`,
`beginning_of_line`, `end_of_line`, `delete_char`, `backward_delete_char`,
`kill_line`, `yank`, `transpose_chars`, `upcase_word`, `downcase_word`,
`capitalize_word`, `undo`, `accept_suggestion`. Keys are named `ctrl+x`,
`c+x`, `alt+x`, `m+x`, `tab`, `shift-tab`, `enter`, `escape`, `up`, `down`,
`left`, `right`, `home`, `end`, `delete`, `backspace` or a single character.

```text
// Sharky ~ settings
tide greeting off
tide autosuggest on
tide history 10000

pearl EDITOR "nvim"
pearl BROWSER "firefox"
wake "fastfetch"

current "~/.cargo/bin"
current "~/.local/bin"

reef ll "ls -lah"
reef gs "git status"
shoal gco "git checkout"
reef cls "clear"

anchor ctrl+r history_search
anchor tab complete
```

### `theme.sharky`

| Instruction | Meaning | Example |
| --- | --- | --- |
| `coral` | define a colour | `coral kelp #8CCFA8` |
| `wave` | prompt template or style | `wave prompt.cwd @kelp` |

Colours are `#RRGGBB`. Styles reference a colour with `@name`; a reference to a
colour that was never defined is an error.

Prompt placeholders: `{cwd}`, `{symbol}`, `{status}`, `{user}`, `{host}`.

```text
// Sharky ~ under the sea
coral abyss #071923
coral foam #E4F5F4
coral sea #35B8C5
coral kelp #8CCFA8
coral danger #FF8F80

wave prompt "{cwd} {symbol} "
wave prompt.cwd @kelp
wave prompt.symbol @sea

wave command @foam
wave builtin @sea
wave string @kelp
wave variable @sea
wave error @danger
wave suggestion @kelp
wave selection @sea
```

`wave suggestion` colours the autosuggest ghost text. Leave it out and the ghost
falls back to dim, so it never looks like text you typed. Set `NO_COLOR=1` to keep
the prompt plain.

`wave overwrite` colours the target of `>`, `2>` and `&>` when it is an existing
literal path. Only unquoted static paths are matched: quotes, variables, tilde
and other expansions are left plain, and no filesystem check runs when
highlighting or colour is off.

### Errors

Invalid instructions, wrong arity, bad colours, unknown key actions and
undefined `@colour` references are reported as `file:line: message`. The shell
continues with defaults, so a typo never locks you out of your own shell.

### `config explain`

`sharky config explain` prints the configuration guide as plain text. With no
topic it prints an index of topic ids; `sharky config explain tide history`
prints the text for one topic, joining every word after `explain` with a single
space. Each parser directive has its own topic — `tide`, `pearl`, `current`,
`reef`, `shoal`, `anchor`, `coral`, `wave` and `wake` — alongside `tide history`,
`files`, `theme` and `overwrite`. An unknown topic or an option mixed into the
request exits 2 with a message on stderr. `config explain` runs before the shell
starts: it reads no configuration, profiles or history and has no side effects.
There is no top-level `sharky explain` alias. `help TOPIC` and the `help` builtin
reuse these topics.

## Files

| Path | Purpose |
| --- | --- |
| `~/.config/sharky/config.sharky` | settings, environment, PATH, aliases, keys |
| `~/.config/sharky/theme.sharky` | palette and prompt styles |
| `~/.local/share/sharky/history` | command history |

Environment: `SHARKY_CONFIG`, `SHARKY_HISTORY`, `XDG_CONFIG_HOME`,
`XDG_DATA_HOME`, `NO_COLOR`, `PATH`.

## Development

```sh
cd rust
cargo test -j 6
cargo clippy -j 6 --all-targets
```

`rust/PORTING.md` documents the architecture and the porting milestones;
`rust/RELEASING.md` has the release checklist. `rust/conformance/cases/` holds
one command per file, compared against a local Ruby v1 checkout at
`.ruby-oracle/bin/sharky` (override with `SHARKY_REF`). Recreate that checkout
with:

```sh
git archive v1-ruby | tar -x -C .ruby-oracle
```

The differential suite is skipped when the oracle is absent.

## License

GPL-3.0-only. Copyright (C) 2026 mafuzyk.
