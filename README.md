# sharky

An interactive Unix shell written in Rust.

sharky runs commands as real Unix processes. The parser, executor, line editor,
prompt, history, completion and job control belong to the shell itself; it never
wraps Bash or `/bin/sh`.

- one static binary, no runtime dependency, startup around half a millisecond;
- a Bash-like language subset, from pipes and heredocs to `[[ ]]` and functions;
- an interactive editor with history and completion, plus real job control;
- a small declarative configuration language for settings and themes.

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

`sharky-shell-bin` is the stable package: it installs the same binary published
on the GitHub release. `sharky-shell-git` tracks `main` and needs a Rust
toolchain.

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

## Login shell

`sharky -l` (or an `argv[0]` beginning with `-`) reads
`/etc/sharky/profile.sharky` and `~/.config/sharky/profile.sharky` as sharky
code, then imports only literal `NAME=value` / `export NAME=value` lines from
`/etc/profile`, `~/.profile`, `~/.bash_profile` and `~/.zshprofile`. Command
substitution and arbitrary shell code in those files never run.

## Shell language

The prompt, `-c` and script files share one grammar:

| Feature | Example |
| --- | --- |
| quotes, escapes, comments | `printf '%s\n' "a b" # comment` |
| variables | `name=value`, `"$name"`, `${name}` |
| parameter operators | `${NAME:-default}`, `${#NAME}`, `${NAME#pre}`, `${NAME/pat/repl}`, `${NAME//pat/repl}`, `${NAME:1:2}`, `${NAME^^}`, `${!NAME}` |
| ANSI-C quoting | `$'a\tb'`, `$'\x41\u00e9'` |
| brace expansion | `file{1..3}.txt`, `a{b,c}d` |
| indexed arrays | `a=(one two)`, `"${a[1]}"`, `"${a[@]}"`, `"${#a[@]}"`, `"${a[@]:1:2}"` |
| associative arrays | `declare -A a`, `a[key]=value`, `"${a[key]}"`, `"${!a[@]}"` |
| command substitution | `files=$(ls \| wc -l)` |
| arithmetic | `$((2 + 3 * 4))`, `((total > 10))` |
| globbing and tilde | `src/*.rs`, `~/bin`, `@(a|b).txt`, `!(tmp)` |
| pipelines and redirects | `a \| b > out 2>&1`, `cmd &> out`, `cmd &>> out` |
| heredocs and here-strings | `cat <<EOF` ... `EOF`, `<<-EOF`, `<<'EOF'`, `cat <<< "text"` |
| process substitution | `diff <(a) <(b)`, `tee >(cat > out)` |
| lists | `a; b`, `a && b`, `a \|\| b` |
| options | `set -e`, `set -u`, `set -x`, `set -o pipefail`, `set -H`/`+H`, `set -o vi`/`+o vi`, `set -- a b` |
| traps | `trap 'cleanup' EXIT`, `trap -l` |
| read and shift | `read -r name`, `shift 2` |
| conditionals | `if`, `elif`, `else`, `fi`, `[[ -f x && "$y" =~ ^f ]]` |
| loops | `for x in a b; do ...; done`, `for ((i=0; i<3; i++))`, `while`, `until` |
| case | `case $x in f*) ...;; *) ...;; esac`, `;;&`, `;&` |
| functions | `greet() { printf 'hi %s\n' "$1"; }` |
| groups and subshells | `{ a; b; }`, `( a; b )` |
| builtins | `type`, `alias`, `unalias`, `abbr`, `unabbr`, `history`, `pwd`, `read`, `shift`, `exec`, `time` |
| history search | Ctrl-R, type to filter, Enter runs the match, Ctrl-G cancels |
| paste and multiline | bracketed paste inserts text verbatim; Enter submits the whole buffer; Up/Down, Home/End work per line |
| autosuggest | history suffix ghost text; Right/End accepts, `tide autosuggest off` disables |
| completion | Tab completes commands, variables, files and git branches; several matches open a menu (Tab/Shift-Tab, Enter, Esc) |
| vi mode | `set -o vi`; Esc for normal mode, `h l 0 $ w b e j k`, `x dd dw d$ D cw C p P u r`, counts; `set +o vi` returns to emacs |
| fuzzy files | Ctrl-T opens a ranked file list; type to filter, arrows to move, Enter inserts, Esc cancels |
| syntax highlighting | `wave command`, `builtin`, `string`, `variable`, `operator`, `path`, `comment`, `error`; `tide highlight off` disables |
| history expansion | `!!`, `!n`, `!-n`, `!prefix`, `!?text?`, `!$`, `!*`, `!^`, `^old^new^`; `set +H` disables |
| job control | `cmd &`, `jobs`, `fg`, `bg`, Ctrl-Z |
| scripts | `source lib.sh`, `sharky file.sh args` |

This is a subset of Bash, not a drop-in replacement. Unsupported syntax fails
with status 2 instead of silently falling back to another shell.

Known limits: word splitting uses space, tab and newline (IFS is not
supported); ANSI-C escapes above 0x7f are UTF-8 characters, not raw bytes; a
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

Set `NO_COLOR=1` to keep the prompt plain.

### Errors

Invalid instructions, wrong arity, bad colours, unknown key actions and
undefined `@colour` references are reported as `file:line: message`. The shell
continues with defaults, so a typo never locks you out of your own shell.

## Files

| Path | Purpose |
| --- | --- |
| `~/.config/sharky/config.sharky` | settings, environment, PATH, aliases, keys |
| `~/.config/sharky/theme.sharky` | palette and prompt styles |
| `~/.local/share/sharky/history` | command history |

Environment: `SHARKY_CONFIG`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `NO_COLOR`,
`PATH`.

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
