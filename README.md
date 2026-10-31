# sharky

An interactive Unix shell written in Rust.

sharky runs commands as real Unix processes. The parser, executor, line editor,
prompt, history, completion and job control belong to the shell itself; it never
wraps Bash or `/bin/sh`.

- one static binary, no runtime dependency, startup around half a millisecond;
- a Bash-like language subset, from pipes and heredocs to `[[ ]]` and functions;
- an interactive editor with history and completion, plus real job control;
- a small declarative configuration language for settings and themes.

The Ruby v1 implementation is frozen at tag `v1-ruby` and stays in the tree as
the behavioural oracle for the differential test suite.

## Install

### Arch Linux (AUR)

```sh
paru -S sharky-shell-git       # or: yay -S sharky-shell-git
```

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

## Shell language

The prompt, `-c` and script files share one grammar:

| Feature | Example |
| --- | --- |
| quotes, escapes, comments | `printf '%s\n' "a b" # comment` |
| variables | `name=value`, `"$name"`, `${name}` |
| parameter operators | `${NAME:-default}`, `${#NAME}`, `${NAME#pre}`, `${NAME%%.*}` |
| command substitution | `files=$(ls \| wc -l)` |
| arithmetic | `$((2 + 3 * 4))`, `((total > 10))` |
| globbing and tilde | `src/*.rs`, `~/bin` |
| pipelines and redirects | `a \| b > out 2>&1` |
| heredocs | `cat <<EOF` ... `EOF`, `<<-EOF`, `<<'EOF'` |
| lists | `a; b`, `a && b`, `a \|\| b` |
| conditionals | `if`, `elif`, `else`, `fi`, `[[ -f x && -n "$y" ]]` |
| loops | `for x in a b; do ...; done`, `while`, `until` |
| case | `case $x in f*) ...;; *) ...;; esac` |
| functions | `greet() { printf 'hi %s\n' "$1"; }` |
| groups and subshells | `{ a; b; }`, `( a; b )` |
| job control | `cmd &`, `jobs`, `fg`, `bg`, Ctrl-Z |
| scripts | `source lib.sh`, `sharky file.sh args` |

This is a subset of Bash, not a drop-in replacement. Unsupported syntax fails
with status 2 instead of silently falling back to another shell.

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
| `anchor` | key binding | `anchor ctrl+r history_search` |

`tide` keys:

| Key | Values |
| --- | --- |
| `greeting` | `on` / `off` |
| `autosuggest` | `on` / `off` |
| `highlight` | `on` / `off` |
| `history` | number of history entries to keep |

`anchor` actions: `history_search`, `complete`, `clear_screen`, `accept_line`,
`abort`, `backward_char`, `forward_char`, `backward_word`, `forward_word`,
`beginning_of_line`, `end_of_line`, `delete_char`, `backward_delete_char`,
`kill_line`, `yank`, `transpose_chars`, `upcase_word`, `downcase_word`,
`capitalize_word`, `undo`.

```text
// Sharky ~ settings
tide greeting off
tide autosuggest on
tide history 10000

pearl EDITOR "nvim"
pearl BROWSER "firefox"

current "~/.cargo/bin"
current "~/.local/bin"

reef ll "ls -lah"
reef gs "git status"
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
one command per file, compared against the frozen Ruby implementation when
`bin/sharky` is present.

## License

GPL-3.0-only. Copyright (C) 2026 mafuzyk.
