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

## What is compared

Only three observable channels are part of the contract:

1. **stdout** — exact bytes.
2. **exit status** — integer exit code.
3. **controlled side effects** — every regular file left in the isolated cwd,
   by relative path and exact contents.

stderr is explicitly **not** compared. Shells differ in diagnostic wording, and
raw stderr parity is not a compatibility goal here; the harness discards it.

## Isolation

Every invocation runs in a fresh `mktemp -d` cwd with a cleared environment:

```
env -i PATH=/usr/bin:/bin LC_ALL=C.UTF-8 DIFF_X='a  b' HOME=<isolated-cwd> \
    <reference-shell> [-c | --noprofile --norc -c] <source>
```

The candidate is invoked the same way but with `CANDIDATE_ARGS` (default
`--no-config -c`) appended before the source, matching the Sharky CLI used by
`rust/tests/differential.rs`. No fixture writes outside its isolated cwd.

## Baseline layout

`baseline/<name>.stdout` holds expected stdout bytes, `baseline/<name>.status`
the expected exit code, and `baseline/<name>.files/` a tree of expected
side-effect files (an empty directory means "no files created").

## Commands

```sh
# Regenerate the baseline from references (dash path auto-discovered in /nix/store).
cd compatibility && ./run.sh record

# Sanity-check that references still reproduce the frozen baseline (no sharky build needed).
cd compatibility && ./run.sh verify-refs

# Compare a built Sharky binary; requires a binary produced by someone else.
cd compatibility && ./run.sh check /path/to/sharky
```

`DASH_BIN` and `BASH_BIN` override reference paths; `CANDIDATE_ARGS` overrides
the candidate invocation. `check` requires a Sharky binary that this unit did
not build and did not verify.
