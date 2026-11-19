# Compatibility baseline contract

Small, benign shell snippets with a frozen expected baseline, used to compare a
candidate shell (Sharky) against real reference shells. This unit only prepares
fixtures and the contract; it does not build or modify the Rust implementation,
the Ruby oracle, or any Rust tests.

## Classification

Each fixture is listed once in `manifest.tsv` as `name<TAB>class<TAB>reference`:

- `portable` → reference is **dash** (POSIX-oriented baseline).
- `bash` → reference is **bash** (Bash-only extensions; dash is not a valid
  oracle for these). In particular `set -euo pipefail` lives here because
  `pipefail` is a Bash extension (the dash build used here, 0.5.13.5, happens
  to accept it, but the contract still assigns this case to Bash).

Portable cases: `p-printf`, `p-status`, `p-unset-default`, `p-unquoted-ifs`,
`p-quoted-join`, `p-cmdsub`, `p-heredoc`, `p-if-then`, `p-side-effect-file`.

Bash-only cases: `b-set-euo-pipefail`, `b-array`, `b-double-bracket`,
`b-parameter-case`, `b-brace-expansion`, `b-local`, `b-side-effect-append`.

`p-unquoted-ifs` pins the *correct* unquoted-IFS behavior: with `IFS=,` an
unquoted `$v` splits into three fields (`a`, `b`, `c`), `count=3`.

`p-quoted-join` pins quoted `"$*"` joining: with `IFS=,` the fields join as
`aa,bb,cc`, and with `IFS=` they concatenate with no separator (`aabbcc`); a
literal-space join fails both. It stays in the portable class because dash
follows the same first-IFS-character rule.

`b-set-euo-pipefail` first observes the pipefail status (`false | true` reports
`rc=1`), then trips nounset on an unset variable, so a candidate that ignores
either extension diverges.

## What is compared

Only three observable channels are part of the contract:

1. **stdout** — exact bytes.
2. **exit status** — integer exit code.
3. **controlled side effects** — every regular file left in the isolated cwd,
   by relative path and exact contents.

stderr is explicitly **not** compared. Shells differ in diagnostic wording, and
raw stderr parity is not a compatibility goal here; the harness discards it.

## Isolation and bounds

Every invocation runs in a fresh `mktemp -d` cwd with a cleared environment,
under a timeout that kills the whole process group (fixture children included):

```
env -i PATH=<controlled-tool-path> LC_ALL=C.UTF-8 DIFF_X='a  b' HOME=<isolated-cwd> \
    <shell> [-c | --noprofile --norc -c] <source>
```

The PATH is auto-resolved from the current locations of `printf`, `timeout` and
`bash` (plus existing `/usr/bin`, `/bin`) and is the same for references and the
candidate; `COMPAT_PATH` overrides it. `timeout --kill-after=2s <TIMEOUT_SECS>s`
(default 5s) bounds every run. The candidate is invoked the same way but with
`CANDIDATE_ARGS` (default `--no-config -c`) appended before the source, matching
the Sharky CLI used by `rust/tests/differential.rs`. No fixture writes outside
its isolated cwd. `check` refuses to run unless a preflight proves the candidate
can execute `printf` (status 0, stdout `ok`), so a missing tool cannot masquerade
as a compatibility result.

## Baseline layout

`baseline/<name>.stdout` holds expected stdout bytes, `baseline/<name>.status`
the expected exit code, and `baseline/<name>.files/` a tree of expected
side-effect files (an empty directory means "no files created").

## Commands

There is no implicit Nix-store dash discovery. If `dash` is not in PATH, set
`DASH_BIN` explicitly before any command that runs portable cases.

```sh
# Regenerate the baseline from references.
cd compatibility && DASH_BIN=/path/to/dash ./run.sh record

# Sanity-check that references still reproduce the frozen baseline (no sharky build needed).
cd compatibility && DASH_BIN=/path/to/dash ./run.sh verify-refs

# Compare an already-built Sharky binary; this unit never compiles one.
cd compatibility && ./run.sh check /path/to/sharky
```

`DASH_BIN` and `BASH_BIN` select reference paths; `COMPAT_PATH`, `TIMEOUT_SECS`,
`TIMEOUT_BIN` and `CANDIDATE_ARGS` control the isolated run. `check` PASS/FAIL
lines name the candidate, not the reference. `check` requires a Sharky binary
that this unit did not build and did not verify.
