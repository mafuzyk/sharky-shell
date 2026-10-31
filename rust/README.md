# sharky (Rust)

An interactive Unix shell written in Rust. This directory is the v2
implementation; the Ruby v1 tree at the repository root is frozen at tag
`v1-ruby` and only serves as the behavioural oracle for the differential tests.

Status: the shell runs commands, scripts and an interactive session, with job
control (`&`, `jobs`, `fg`, `bg`, Ctrl-Z).

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

The AUR recipe is in `packaging/PKGBUILD` (`sharky-shell-git`). Generate
`.SRCINFO` with `makepkg --printsrcinfo > .SRCINFO` before pushing to the AUR.

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
declarative files. They are parsed by `src/config.rs`, never executed by the
command parser:

```text
// config.sharky
tide greeting off
tide history 10000
pearl EDITOR "nvim"
current "~/.local/bin"
reef ll "ls -lah"
anchor ctrl+r history_search
```

```text
// theme.sharky
coral kelp #8CCFA8
coral sea #35B8C5
wave prompt "{cwd} ❯ "
wave prompt.cwd @kelp
wave prompt.symbol @sea
```

Copies of both files ship in `examples/`. Invalid instructions, wrong arity,
bad colours and undefined `@colour` references are reported with file and line,
and the shell continues with defaults.

## History

Stored in `$XDG_DATA_HOME/sharky/history` (default
`~/.local/share/sharky/history`), limited by `tide history`.

## Tests

The suite drives the built binary, plus a differential run against the frozen
Ruby implementation when it is present:

```sh
cargo test -j 6                      # unit, CLI, PTY and differential tests
SHARKY_REF=/path/to/ruby/bin/sharky cargo test -j 6 --test differential
```

Conformance cases live in `conformance/cases/`, one command per file. Add one
for every new feature: it is what keeps the port honest against the oracle.

Porting notes and design decisions are in `PORTING.md`.
