# sharky (Rust)

An interactive Unix shell written in Rust. This is the v3 implementation and
the source of truth. The Ruby v1 tree is frozen at tag `v1-ruby` and is not
published here; keep a local checkout at `../.ruby-oracle/` only when you need
the behavioural oracle for the differential tests.

Status: `0.1.2`, an ongoing stage of maturity shipped through the normal
release channel.
Interactive features include path-checked ghost autosuggestions with word-by-word
acceptance (`Alt+Right`/`Ctrl+Right`), prefix-filtered `Up`/`Down` history recall,
live syntax highlighting (`@error`, `@overwrite`), multi-position abbreviations
(`abbr`), `autocd`, interactive `"did you mean"` hints, `z`/`zi`, fuzzy
file search (`Ctrl+T`), completion menus (`cd`/`pushd` directories-only,
case-insensitive fallback), vi mode, and `{cwd_short}`/`{git}`/`{duration}` prompt
tokens. Non-interactive scripting and automation include `[[ ]]`, `((...))`,
indexed/associative arrays, extended parameter expansion, heredocs, here-strings,
process substitution, extglob, `shopt -s/-u globstar|autocd`, traps, job control,
`sharky agent` (`--stdio`, `--socket`, `--mcp`), `workflow`, and Code Mode.

## Build

```sh
cargo build --release -j 6
./target/release/sharky --version
```

Install manually:

```sh
install -Dm755 target/release/sharky ~/.local/bin/sharky
install -Dm644 man/sharky.1 ~/.local/share/man/man1/sharky.1
```

The AUR recipes are in `packaging/` (`sharky-shell-git`) and
`packaging/bin/` (`sharky-shell-bin`). Generate `.SRCINFO` with
`makepkg --printsrcinfo > .SRCINFO` before pushing either one to the AUR.

## Use

```sh
sharky                      # interactive
sharky -c 'printf "a b\n" | wc -w'
sharky script.sh argument
echo 'printf hi' | sharky   # reads stdin when there is no terminal
```

Read `man sharky` for the supported language subset.

## Configuration

`$SHARKY_CONFIG`, or `${XDG_CONFIG_HOME:-~/.config}/sharky`, holds two small
declarative files. They are parsed by the `config` crate (`config/`), never
executed by the command parser:

```text
// config.sharky
set greeting off
set history 10000
env EDITOR "nvim"
path "~/.local/bin"
alias ll "ls -lah"
bind ctrl+r history_search
```

```text
// theme.sharky
color ok #8CCFA8
color accent #35B8C5
style prompt "{cwd} {symbol} "
style prompt.cwd @ok
style prompt.symbol @accent
```

Copies of both files ship in `examples/`. Invalid instructions, wrong arity,
bad colours and undefined `@colour` references are reported with file and line,
and the shell continues with defaults.

## History

Stored in `$SHARKY_HISTORY`, or `${XDG_DATA_HOME:-~/.local/share}/sharky/history`,
and limited by `set history`. Commands are appended after evaluation under a
lock; a failed write keeps the entry only in memory. `-P`/`--private` keeps
history in memory and leaves the file untouched. Ctrl-R opens an incremental
reverse search; Enter runs the selected entry and Ctrl-G cancels.

## Tests

The suite drives the built binary, plus a differential run against a local
Ruby v1 checkout when `../.ruby-oracle/bin/sharky` is present:

```sh
cargo test -j 6                               # unit, CLI, PTY and differential tests
SHARKY_REF=/path/to/ruby/bin/sharky cargo test -j 6 --test differential
```

Conformance cases live in `conformance/cases/`, one command per file. Add one
for every new feature: it is what keeps the port honest against the oracle.

Porting notes and design decisions are in `PORTING.md`.
