# Changelog

All notable changes to sharky are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/) and the project uses semantic
versioning. sharky is in alpha: version numbers mark releases, while alpha is
the project's ongoing maturity, delivered through the normal release channel
rather than a separate preview track.

## [Unreleased]

### Changed

- Interactive sessions refresh the shared history file before every primary
  prompt, so a command another shell ran appears in the editor, the autosuggest
  and the `history` listing without `history merge`, which stays as the forced,
  idempotent form of the same path. One `stat` decides whether the file has to be
  read, an unchanged file is neither read nor locked, and a continuation line
  never refreshes. History expansion is guarded by origin: `!!`, `!n`, `!prefix`,
  `!?text?` and `!-n` resolve only the entries a session loaded at startup or ran
  itself, never a command a refresh picked up from another terminal, and after a
  refresh `!n` numbers that expansion list. A `history -c` in one session clears
  what the others had loaded while their own commands survive, and a failed
  append stays expandable. Sessions no longer keep their own view until
  `history merge`.

### Added

- The `history` builtin searches, filters and deletes: `history [search]
  [--max N] [--reverse] [--show-time] [--null] [--case-sensitive]
  [--contains|--prefix|--exact] [PATTERN]` lists matching entries newest first
  with a case-insensitive substring match by default, `--max 0` prints nothing,
  `--show-time` prefixes each timed entry with a UTC timestamp line and
  `history delete --exact PATTERN` removes exact matches by rewriting the file
  under the history lock, preserving legacy bytes and the v2 record fields.
- Auto-clean bounds the shared history file itself: a vacuum caps it at 262144
  records by file position, independently of any session's in-memory
  `tide history` limit (default 1000). A small in-memory limit never truncates a
  file other sessions share; duplicate commands collapse to their newest record
  and legacy lines keep their original bytes when rewritten.
- History records use a one-line v2 format,
  `h2 \t <when-us> \t <cwd> \t <npaths> \t <paths...> \t <cmd> \t <hits>`
  (TAB-separated, with escapes inside each field), carrying the command's
  timestamp, working directory, path hints and hit count. Legacy v1 lines
  coexist and migrate lazily: loading rewrites nothing, and a legacy record
  keeps `when = None` and its original bytes. Downgrading to a 3.6.0 binary is
  non-destructive: that binary reads v2 lines as opaque commands and its vacuum
  keeps them opaque, so the 3.6.0 history file is never lost or corrupted.
- The ghost autosuggestion completes the token under the cursor instead of
  requiring a whole-line prefix: it scans history newest first and prefers an
  exact case-sensitive token match over a case-insensitive one. A record whose
  referenced paths no longer resolve relative to where the command ran is
  skipped; a command whose output path did not exist at record time, and legacy
  records without paths, stay eligible. Ctrl-R ranks a non-empty query by
  substring and conservative frecency (hits times an age weight of 4 within a
  day, 2 within 30 days and 1 beyond, with legacy records counted as old),
  breaking ties on the newer timestamp and then the file position, and pins the
  newest exact case-sensitive prefix match first so repeated Ctrl-R finds the
  command just typed; an empty query and ordinary Up/Down recall stay
  chronological.
- The flake now exposes `devShells` with the Rust toolchain, `nixosModules` and
  `homeManagerModules` for declarative setup, and `passthru.shellPath` on the
  package so it can be used as `users.users.<name>.shell` on NixOS.

## [3.6.0] - 2026-11-19

### Added

- `sharky status` prints plain-text facts before the shell starts: the version,
  the config and theme paths resolved through `SHARKY_CONFIG`, `XDG_CONFIG_HOME`
  and `HOME` with existence markers, the history path resolved through
  `SHARKY_HISTORY`, `XDG_DATA_HOME` and `HOME` with an existence marker, and the
  active compile-time feature flags (`none` in this release). Both subcommands
  exit 2 on malformed arguments; `status` exits 0 otherwise, and `doctor` exits
  0 when all checks pass and 1 when any fails.
- `sharky doctor` runs read-only local checks: stdin/stdout descriptor validity,
  config parsing through `config::load` with a separate check for files that
  load would silently skip, theme parsing, history permissions and writability
  by metadata only, and resolution of `sh`, `ls` and `cat` through
  `exec::resolve`. It prints one `OK`/`FAIL` line per check, exits 0 when all
  pass and 1 when any fails, and performs no fstab checks. Both subcommands are
  dispatched from `cli::run` before the normal shell startup path, so neither
  applies `pearl`, evaluates profiles, writes files or prints environment or
  configuration values; `doctor` builds a bare `Shell` only to resolve tool
  names. `status` reads no configuration files; `doctor` parses `config.sharky`
  and `theme.sharky` read-only through the configuration parser.
- Durable command history: each evaluated command is appended to one shared file
  under a lock, with failed writes kept only in memory and torn tails dropped on
  the next append. Sessions keep their own view until `history merge`, and
  `-P`/`--private` keeps history in memory. The file defaults to
  `${XDG_DATA_HOME:-~/.local/share}/sharky/history` (`SHARKY_HISTORY` overrides).
- Prebuilt Nix flake: `nix profile install github:mafuzyk/sharky-shell` installs
  the verified static binary, man page and example config on `x86_64-linux`.
- Opt-in `wave overwrite` styling colours the literal target of `>`, `2>` and
  `&>` when it names an existing file.
- `shopt` supports exactly one option, `globstar`: `shopt -s globstar` enables
  recursive `**` pathname matching (off by default), `shopt -u globstar`
  restores ordinary single-level globbing and `shopt globstar` reports the
  state. Quoted `**` stays literal, hidden entries are skipped unless the
  component starts with `.`, matches are deduplicated and `./`/`../` prefixes
  are preserved. Other option names and combined or interleaved `-s`/`-u`
  flags are rejected; the setting is per shell and is not persisted.
- `mapfile`/`readarray` reads stdin lines into an indexed array, defaulting to
  `MAPFILE`. The optional `-t` strips the trailing newline while preserving
  carriage returns and backslashes. Invalid options return 2, while invalid or
  associative targets return 1, matching Bash's status codes.
- A portable and Bash-only compatibility corpus under `compatibility/` freezes
  expected stdout, exit status and controlled side effects for small shell
  fixtures and compares a candidate against dash and Bash. The `verify-refs`
  command checks the references against the frozen baseline; `check <binary>`
  measures a built binary's stdout/status/side-effect divergence. stderr is
  not compared and no conformance is claimed.
- `sharky config explain` prints the configuration guide as plain text before the
  shell starts, reading no configuration, profiles or history and leaving no side
  effects. With no topic it lists every topic id; the words after `explain` are
  joined with spaces to select one topic. Each parser directive has its own topic
  (`tide`, `pearl`, `current`, `reef`, `shoal`, `anchor`, `coral`, `wave`, `wake`)
  alongside `tide history`, `files`, `theme` and `overwrite`. Unknown topics and
  mixed options exit 2, and there is no top-level `sharky explain` alias.
- Unset-only parameter operators `${name-word}`, `${name=word}`, `${name+word}`
  and `${name?word}` act only on unset parameters, leaving the colon forms
  (`${name:-word}`, `${name:=word}`, `${name:+word}`, `${name:?word}`) to treat
  unset and null alike. `${name?word}` expands its word before reporting it,
  and an empty `$@`/`$*` counts as unset for `+` and `-`.
- Unquoted expansions are split on `IFS` (space, tab and newline by default):
  repeated delimiters keep empty fields, leading and trailing ones are dropped,
  and an empty `IFS` disables splitting. Literals and quoted expansions stay
  unsplit.
- The `local` builtin declares variables in the current function frame with
  dynamic scope: earlier bindings (scalar, indexed or associative) are saved
  and restored on return, and locals are visible to called functions. `local`
  outside a function, invalid names and unsupported options report failure.
- `sharky help [TOPIC...]` and the `help` builtin share one static registry with
  `--help`. With no topic they print the same index as `--help`; with a topic
  they join every word with spaces and print one entry. Topics are builtin
  names, command-line flags and subcommands, plus the configuration topics of
  `config explain`. An unknown topic exits 1. The subcommand runs before the
  shell starts: no configuration is loaded or created and there are no side
  effects.
- `--check` parses `-c` or a file for syntax only: it exits 0 silently on
  success, or 2 with a line reference on a top-level syntax error. It requires
  `-c` or a file (not stdin), checks only top-level syntax, executes nothing and
  neither reads nor writes configuration, profiles or history. It cannot be
  combined with `-i`, `-l`, `--json` or `state`.
- `command` runs a name without function lookup, so it reaches the builtin or
  external command hidden by a same-named function. `-v` prints how each name
  resolves (bare name for builtins and functions, `alias name='value'` for
  aliases, a path for externals) and `-V` prints a verbose description; missing
  names are skipped silently by `-v` and reported on stderr by `-V`. `-p`
  searches the default PATH instead of the environment PATH for lookup and for
  execution, without changing the environment. The status follows Bash: with
  `-v`/`-V` it is 0 when any name resolves (and with no names), and 1 only when
  every name is missing; otherwise it is the status of the command run (127
  when the name is not found), and an invalid option returns 2.
- `command -V NAME` now prints the function definition after the
  `NAME is a function` line, in Bash's `declare -f` layout: the name and an
  empty parameter list, the body in braces with one statement per line at
  four-space indentation and semicolons between statements. Nested blocks and
  `elif` are formatted the way Bash prints them.

### Fixed

- `"$*"` and `"${a[*]}"` join their elements with the first character of `IFS`
  (nothing when `IFS` is empty) instead of a literal space.
- `${!a[*]}` and `${!m[*]}` join indexed and associative array keys with the
  first character of `IFS` (nothing when `IFS` is empty) instead of a literal
  space.
- `${!x[@]}` and `${!x[*]}` treat a defined scalar as a one-element key list
  holding `0`, so they expand to `0` instead of nothing; an undefined name
  still expands to no keys, and empty or sparse arrays keep listing only their
  real indices.
- `set` accepts clustered short options such as `-euo`, long names under
  `-o`/`+o` (`errexit`, `nounset`, `xtrace`, `histexpand`, `pipefail`, `vi`),
  and keeps reading letters after `o` so `set -oe pipefail` enables both.
  `set +o` without a name lists the active options as reusable `set` commands.
- Array literals parse after `local`, `readonly` and `typeset`, so declarations
  such as `local a=(one two)` no longer fail.
- An assignment-only simple command takes the status of its last command
  substitution, so a failing substitution now trips `errexit` instead of
  reporting success.
- `set -e` and `set -u` no longer exit the interactive shell: errexit failures,
  unbound variables and failing `if` conditions report a status and return to
  the prompt.
- Expansion errors now follow Bash's per-context status: a null or unset
  parameter error exits 127 in a `-c` command string without errexit, while
  scripts, standard input, subshells, errexit shells and interactive lines keep
  status 1 (arithmetic errors stay at 1). An expansion error in an `if`/`elif`
  condition discards the whole compound command: no branch runs, not even
  `else`. Non-interactive shells abort on the error; interactive shells report
  its status and keep running.
- `read` splits fields on `IFS` with Bash's last-name remainder rule: leading
  and delimiter-adjacent IFS whitespace is skipped, a non-whitespace delimiter
  ends a field even when empty, and the last name receives the rest of the line
  with its delimiters. A trailing IFS character ends the remainder, a null IFS
  leaves the line unsplit, and bare `read` stores the untrimmed line in `REPLY`.
- A name no longer holds both a scalar and an array binding: a later assignment
  of either kind wins, scalar writes go into element 0 of an existing array, and
  a stale scalar is no longer exported after the name becomes an array.
- Declaring an indexed array associative, or an associative array indexed, no
  longer discards its elements: the conversion is rejected with status 1, the
  original array is kept and the error is reported for that name while the
  remaining arguments still apply.
- `command -v`, `command -V` and `type` report a name containing a slash only
  when it names an executable file, without searching PATH: non-executable
  files, directories and symlinks to non-executable files are no longer
  reported as found. Running such a path still reports permission denied.
- When both `-v` and `-V` are given to `command`, the last one wins, whether in
  one cluster or across separate words: `command -Vv cd` prints the bare name
  and `command -vV cd` prints the description. Previously any `-V` forced the
  verbose form.

## [3.5.4] - 2026-11-02

### Fixed

- The autosuggest ghost never paints itself in the same style as typed text: a
  theme without `wave suggestion` now gets dim ghost text instead of the
  terminal's normal foreground.

## [3.5.3] - 2026-11-01

> _Added more bugs to fix it later._

### Added

- Named-FIFO fallback for process substitution on hosts without `/dev/fd`.
- `wake` is a builtin, so login profiles can register startup commands.

### Fixed

- `declare -A a=([k]=v ...)` parses and fills the map.
- Associative keys accept quoting and expansion; `"${a[*]}"` joins, while
  `"${a[@]}"` keeps one field per value, including prefixes, suffixes and case
  operators.
- `unset 'a[i]'` leaves a hole instead of renumbering later elements.
- Process-substitution descriptors survive function bodies, and substitution
  children are reaped at prompt idle without taking the terminal foreground.
- Completion escapes file names and never opens a menu in a narrow terminal;
  the fuzzy pager uses the alternate screen.
- Multiline redraw returns from the row it drew, wide characters count two
  columns, and pasted control sequences are inserted verbatim.
- SIGTERM and SIGHUP restore the terminal and disable bracketed paste.
- JSON capture uses private exclusive files, survives `exec`, aborts before
  running when capture cannot be set up, and keeps profiles inside the
  envelope. Non-UTF-8 environment values are replaced, not dropped.
- Arithmetic and substring offsets no longer abort on overflow; arithmetic
  offsets are evaluated, `&&`/`||` short-circuit without side effects, and
  malformed C-style `for` is a status-2 error.
- History selectors respect quoting, and an apostrophe inside double quotes no
  longer suppresses expansion.
- Profiles run as whole units, so `if`/loops are not executed line by line.
- Vi counts work for `e`, `p`, `P` and `dd`; Esc cancels an operator without
  moving the cursor.
- Ctrl-_ reaches undo, transpose follows readline, and `set -o` without a name
  reports active options.
- Extglob handles quoted parentheses, nullable `+()` alternatives, quoted
  metacharacters and globs in directory components.
- Empty quoted arguments keep their field (`set -- ""`, `pre"$@"post`,
  `"${@}"`, `"${*}"`) and zero-element `[@]` expands to zero fields.
- `set -e` ignores the status of a negated pipeline, as Bash does.
- JSON `--json` reports the cwd and environment delta reached before `exec`,
  including when `exec` comes from a login profile.
- A login profile with a parse error stops at that file instead of running its
  lines outside the broken structure; diagnostics carry the real line.
- The FIFO fallback wakes and removes an unconsumed substitution when its
  command ends, waits for slow consumers, reaps substitutions while the prompt
  waits, and never deletes unrelated files from the temp directory.
- Command completion escapes PATH executables, and pasted content ending in
  ESC no longer loses the end marker.
- Sparse array slices select by index, negative offsets follow the last
  defined index, `unset 'a["key"]'` removes associative keys, and `${@:n}`
  keeps one field per argument.
- The process-substitution scanner accepts `case` lists and extglob patterns,
  including cases nested in `if`/`for` and an empty `case ... in esac`.
- Malformed arithmetic is a status-2 error instead of a lexer panic, and an
  arithmetic expansion error fails the command before it applies redirections.
- Quoted glob characters stay literal (`x"*"`), including inside quoted array
  expansions.
- Vertical movement preserves the display column with wide characters and
  tabs, the menu closes on resize, SIGTERM leaves the fuzzy alternate screen,
  and vi `r` leaves the cursor on the last replaced character.

## [3.5.2] - 2026-11-01

### Fixed

- The editor no longer clears the prompt on every redraw. `ESC[J` now starts
  at the end of the prompt, so prompt icons and text stay visible while typing.

## [3.5.1] - 2026-11-01

### Fixed

- The editor no longer moves the cursor one row too far when the prompt plus
  the input exactly fills the terminal width. Long commands stopped erasing the
  line above and Koral's command mark stayed intact.

## [3.5.0] - 2026-10-31

### Added

- Extended parameter expansion: substrings, case conversion, indirect
  expansion (`${!x}`, `${!prefix*}`, `${!prefix@}`) and `${!a[@]}` indices.
- ANSI-C quoting (`$'...'`) with `\n`, `\t`, `\xHH`, `\uHHHH`, octal and
  the other Bash escapes.
- Associative arrays (`declare -A`), array slices and arithmetic indexes.
- C-style `for ((...))`, `case` fallthrough with `;;&` and `;&`, and extglob
  patterns (`?(a|b)`, `*(a|b)`, `+(a|b)`, `@(a|b)`, `!(a|b)`).
- Process substitution (`<(list)`, `>(list)`) with reaped children and closed
  descriptors.
- Interactive history expansion: `!!`, `!n`, `!-n`, `!prefix`, `!?text?`,
  `!$`, `!*`, `!^`, `^old^new^`, controlled by `set -H`/`set +H`.
- `exec` with `-a`, persistent redirections and interactive failure handling;
  `time` and `time -p`.
- Bracketed paste and multiline editing with per-line Up/Down and Home/End.
- History autosuggest with ghost text and Right/End acceptance.
- `wave`-driven syntax highlighting for commands, builtins, strings,
  variables, operators, paths, comments and lexer errors.
- `anchor` keybindings for the full action set, including
  `accept_suggestion` and `fuzzy_files`; `ctrl+x`, `c+x`, `alt+x` and `m+x`
  spellings.
- Completion for commands, variables, files, `~` paths and git branches, with
  an inline menu; Ctrl-T fuzzy file search.
- Agent interface: `--json -c` with captured stdout/stderr, status, duration,
  cwd and environment delta; `state --json [--env]`; `-i` to force the REPL.
- Login shell (`-l`/`--login` or a leading `-` in `argv[0]`) reading sharky
  profiles and importing only literal assignments from standard profiles.
- Vi mode (`set -o vi`): insert/normal modes, `h l 0 $ w b e j k`, `x dd dw
  d$ D cw C p P u r<char>` and count prefixes; `set +o vi` restores emacs.

### Changed

- Navigation keys are bindable through `anchor`; defaults match the previous
  behaviour.
- `set` reports `histexpand` and `vi` when they are enabled.

## [3.0.0] - 2026-10-31

### Added

- Koral terminal integration: OSC 133 prompt and command marks with a
  URL-encoded command line, OSC 7 working-directory marks, and the exit status
  on every command end.
- `sharky-config` crate shared with Koral: parser, `tokenize`, `INSTRUCTIONS`
  and `ACTIONS`, with no dependencies and a stable public API.
- Abbreviations: `shoal` in `config.sharky`, `abbr`/`unabbr` builtins, and
  expansion on space or Enter at command position.
- Ctrl-R incremental reverse history search; Enter runs the match and Ctrl-G
  cancels.
- Multi-line input in the REPL: unfinished compound commands keep buffering
  instead of failing at the end of the line.

### Changed

- The configuration parser moved to `rust/config`; the shell depends on it by
  path. The release binary stays static, with `nix` as the only external
  dependency.

## [2.1.0] - 2026-10-31

### Added

- Starship prompt integration (`tide starship on`), including status, command
  duration and running jobs.
- Shell options: `set -e`, `set -u`, `set -x`, `set -o pipefail`, `set --` and
  bare `set`; `trap` EXIT with listing and removal; `read` and `shift`.
- `[[ =~ ]]`, `-a`, `-L`/`-h` and Ruby-style integer coercion.
- Indexed arrays, here-strings, `&>`/`&>>`, brace expansion and
  `${x/pat/repl}`/`${x//pat/repl}`.
- Builtins `type`, `alias`, `unalias`, `history`, `pwd`, `wait`, `kill`,
  `z`, `zi` and `reload`.
- Startup commands (`wake "fastfetch"`) and automatic `PATH` discovery from
  shell profiles (`tide profile_paths`).
- First run seeds `~/.config/sharky/config.sharky` and `theme.sharky`.

### Fixed

- Malformed input no longer panics or hangs the parser (`;;&`, `for ;`,
  `for ((...))`, unclosed `$(`, `$((` and `${`).
- `&` separates list items on the same line, and background jobs work outside
  the REPL with process groups.
- Multi-line prompts such as Starship are drawn once per input line instead of
  being reprinted on every keystroke.
- `cd` handles no argument and `-`, updates `PWD`/`OLDPWD`, and records visits
  for `z`; `$!` reports the last background pid.

## [2.0.0] - 2026-10-28

### Added

- Rust implementation of sharky, replacing the Ruby shell, with static musl
  releases and AUR packages.

## [1.0.1] - 2026-10-28

### Fixed

- Redirections now expand arguments, assignments and every target before opening
  files, so a failed expansion cannot truncate an existing destination.
- Parser fixes for whole-subshell redirection, escaped globs, multiple prefixed
  assignments, here-document literal bodies and `$X` delimiters, comments inside
  `${...}`, contextual `))`, multiline subshells and newlines after operators.
- Arithmetic fixes for short-circuit operations, negative division/remainder and
  relational/equality precedence; compound commands work inside `$()`.
- Command status is now recorded for pipelines, subshells, conditionals and
  negations; `&&`/`||` mixed lists and suppressed `set -e` contexts behave like
  Bash; EXIT traps run all actions and honor an explicit `exit` status.
- Session, JSON and the Ruby facade now use the compound parser, matching the
  normal `-c` entry point.
- `[[ ]]` rejects unknown operators, supports symlink and lexical predicates,
  and no longer returns true for an unsupported expression.
- `type` recognizes functions defined by the native parser; editor history Down
  moves toward newer entries; docs describe the existing Ruby completion hook.
- Job-control fixes for partially reaped `fg` pipelines, pipefail, HUP cleanup of
  running/stopped jobs, default `fg` selection and done-job pruning.
- Compound stdin reaches external commands; command-substitution stderr reaches
  JSON envelopes; `[[ ]]` expands variables/patterns; `read` handles leading
  whitespace, escapes and line continuations.
- Starship accepts an explicit shell environment, including removed variables,
  and sets the working directory consistently. Its deadline now also covers
  output readers when descendants inherit the pipes.
- Git prompt queries drain stdout and stderr while waiting, preserving dirty
  state for repositories with large status output under one deadline.
- Result environment deltas copy and freeze names and values rather than
  sharing mutable strings with a session.
- Arch packaging uses the cloned source directory and declares its Git build
  dependency. Source tarballs include tests, benchmarks and CI metadata;
  installation checks preserve existing artifacts by building in temporary trees.
- CI fails on Ruby warnings, and the PTY soak requires an actual output line and
  a new prompt rather than accepting editor echo. Documentation now describes
  the supported native language, renderer-only measurements and 1.x security policy.

## [1.0.0] - 2026-10-28

### Added

- First-class `Sharky::Session` and structured `Sharky::Result`; `--json -c` and
  `state --json` use a versioned envelope with environment redaction by default.
- Native Bash-like language subset: conditionals, loops, `case`, functions,
  groups, `!`, arithmetic commands and expansion, indexed arrays, brace
  expansion, parameter expansion, here-documents, here-strings and compound
  redirections.
- Native `.sh` scripts and shebangs, `$0`/`$#`/`$@` positional parameters,
  function arguments, `source`/`.`, `set -e/-u/-x`, `pipefail`, `read`,
  `shift`, `break`/`continue`/`return` and `trap` (EXIT action executes).
- Interactive continuation for compound commands and here-documents; syntax
  highlighting and completion understand the new grammar.
- `Makefile` prefix install/uninstall, source tarball, Arch `PKGBUILD`, man page
  and an install integration test.

### Changed

- CLI `-c` and the interactive loop use the compound parser; the legacy
  `Shell#run` default remains for compatibility.
- Unsupported syntax still exits 2 with a precise message; no Bash fallback.
- Job listings reconstruct compound command text instead of showing blanks.

### Fixed

- Command substitution now runs in a forked subshell, captures stdout, strips
  trailing newlines and reaps the child.
- Compound trailing redirections, `pipefail`, nounset defaults and brace
  expansion regressions found by the Bash oracle matrix.

### Known limitations

- Login-shell installation remains post-v1 until the stability gate passes.
- Signal traps are stored, but only the EXIT trap executes in v1.
- No positional/slice array field splitting parity; no associative arrays,
  `coproc`, process substitution, extglob, programmable completion or full
  `[[ =~ ]]`.
- `pipefail` is not applied to job-control pipelines.

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
