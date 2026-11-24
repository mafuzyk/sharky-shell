//! Durable differential regression suite for the Fase A semantic fixes.
//!
//! Every case below runs the same script under a reference bash (`BASH_BIN`, or
//! `which bash`) and under the sharky binary, then compares stdout *bytes*,
//! stderr *presence* and exit status, plus the files left in the working
//! directory. stderr text is deliberately not compared: the two shells prefix
//! diagnostics differently, so only the presence of a diagnostic is a stable
//! contract.
//!
//! When no reference bash is available the whole suite skips with a note; a
//! missing oracle must never be reported as a regression.
//!
//! History expansion is intentionally absent here: it is interactive-only and
//! is exercised through a PTY in `tests/interactive.rs`, not through `-c`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The same reduced environment for both shells; `env_clear` keeps host
/// dotted-variables and shell-specific exports out of the comparison.
const CONTROLLED_ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/bin:/bin:/run/current-system/sw/bin"),
    ("HOME", "/tmp"),
    ("LC_ALL", "C.UTF-8"),
];

/// (case name, script). File side effects are produced by the scripts
/// themselves so both shells start from the same empty sandbox and the final
/// directory contents can be compared as part of each case.
const CASES: &[(&str, &str)] = &[
    // noclobber: `set -C` refuses to truncate an existing file (status 1).
    ("noclobber_refuses_truncate", r#"printf a > f; set -C; echo b > f; echo "st=$?""#),
    // noclobber: `>|` overrides the refusal and writes the new content.
    ("noclobber_force_override", r#"printf a > f; set -C; echo b >| f; echo "st=$?""#),
    // noclobber: appending to an existing file is not blocked.
    ("noclobber_append_unaffected", "printf a > f; set -C; printf b >> f"),
    // readonly: a failed assignment aborts its line and the next line still runs.
    ("readonly_assignment_aborts_line", "readonly q=1\nq=2\nprintf 'AFTER\\n'"),
    // readonly: unset/declare/export are all refused with status 1, value kept.
    (
        "readonly_unset_declare_export_refused",
        r#"readonly r=1; unset r; printf "%s " "$?"; declare r=2; printf "%s " "$?"; export r=3; printf "%s " "$?"; printf "<%s>" "$r""#,
    ),
    // readonly: `readonly -p` lists the variable (filtered: bash also lists its own internals).
    ("readonly_p_lists_variable", r#"readonly r=1; readonly -p | grep -F 'declare -r r='"#),
    // readonly: re-assigning the same value is still a refused write.
    ("readonly_repeat_value_refused", "readonly r=1; r=1; printf after"),
    // pipestatus: false|true records both stage statuses.
    ("pipestatus_two_stages", r#"false | true; echo "${PIPESTATUS[@]}""#),
    // pipestatus: a later builtin replaces the array with its own status.
    ("pipestatus_reset_by_builtin", r#"false | true; :; echo "${PIPESTATUS[@]}""#),
    // pipefail: changes `$?` but leaves PIPESTATUS intact.
    ("pipefail_changes_status", r#"set -o pipefail; false | true; echo "${PIPESTATUS[@]} $?""#),
    // source/return: `return N` in a sourced file keeps the caller alive.
    (
        "sourced_return_keeps_caller",
        r#"printf 'echo in-lib\nreturn 5\necho never\n' > lib.sh; source ./lib.sh; echo "st=$? survived""#,
    ),
    // source/return: top-level `return` is an error with status 2, script continues.
    ("top_level_return_is_error", r#"return 9; echo "st=$? survived""#),
    // errexit: inside $() a failing command does not end the substitution.
    (
        "errexit_disabled_in_cmdsub",
        r#"set -e; x=$(false; echo inner); echo "st=$? x=$x survived""#,
    ),
    // errexit: a substitution whose final status fails still fails the assignment.
    ("failed_cmdsub_exits_under_errexit", "set -e; x=$(false); echo survived"),
    // read: `-n 1` consumes exactly one character from a printf pipe.
    ("read_n_one_char", r#"printf 'abc\n' | { read -n 1 x; printf "<%s|%s>" "$x" "$?"; }"#),
    // read: `-a` splits the input into an array.
    (
        "read_a_array",
        r#"printf 'a b c\n' | { read -a arr; printf "<%s|%s|%s>" "${#arr[@]}" "${arr[0]}" "${arr[2]}"; }"#,
    ),
    // jobs: `jobs -p` prints a numeric pid (normalised to YES so pids can differ).
    (
        "jobs_p_prints_pid",
        r#"sleep 0.1 >/dev/null 2>&1 & p=$(jobs -p); case $p in ""|*[!0-9]*) echo NO;; *) echo YES;; esac"#,
    ),
    // declare: a function-local `declare` does not leak to the global scope.
    (
        "declare_local_in_function",
        r#"z=outer; f() { declare z=inner; printf 'in=%s\n' "$z"; }; f; printf 'out=%s\n' "$z""#,
    ),
    // declare: `declare -F` lists the defined functions, sorted.
    ("declare_F_lists_functions", r#"zfn() { :; }; afn() { :; }; declare -F; echo "st=$?""#),
    // $(<file): reads the file and strips trailing newlines.
    (
        "cmdsub_file_content",
        r#"printf 'alpha\nbeta\n\n' > f; x=$(<f); printf '[%s] status=%s\n' "$x" "$?""#,
    ),
    // $(<file): a missing file yields empty output, status 1 and a diagnostic.
    ("cmdsub_file_missing", r#"x=$(<missing); printf '[%s] status=%s\n' "$x" "$?""#),
    // arith: hex, shift, bitwise, ternary and exponent all evaluate.
    (
        "arith_hex_shift_bitwise_ternary",
        r#"printf '%s %s %s %s %s' "$((0x10))" "$((1 << 3))" "$((5 & 3))" "$((1 ? 2 : 3))" "$((2 ** 8))""#,
    ),
    // arith: an invalid octal literal aborts the expansion with a diagnostic.
    ("arith_invalid_octal_error", r#"printf '%s' "$((08))""#),
    // tilde: `~root` resolves through the passwd database (guarded at runtime).
    ("tilde_root_user", "echo ~root"),
    // wait: `wait -1` is an invalid option (status 2), not a pid lookup.
    ("wait_negative_is_invalid_option", "wait -1; echo st=$?"),
    // wait: `wait $!` reports the background job's exit status.
    ("wait_background_status", r#"(exit 7) & wait $!; echo "st=$?""#),
    // glob: quoted directory names with parens/brackets expand `mv "$d"/*.mkv`.
    (
        "glob_quoted_dir_metachars",
        r#"mkdir -p "Show (2025)" "[SubsPlease] [1080p]" dest && : > "Show (2025)/ep01.mkv" && : > "Show (2025)/keep.txt" && : > "[SubsPlease] [1080p]/ep02.mkv" && for d in "Show (2025)" "[SubsPlease] [1080p]"; do mv "$d"/*.mkv dest/; done && printf '<%s>\n' dest/*.mkv "Show (2025)"/* "[SubsPlease] [1080p]"/*"#,
    ),
    // glob: trailing `/` matches directories and dir symlinks only.
    (
        "glob_trailing_slash_dirs_only",
        r#"mkdir -p alpha/sub beta .hid && : > gamma.mkv && ln -s alpha linkdir && ln -s gamma.mkv linkfile && printf '<%s>\n' */ */*/"#,
    ),
    // brace: brace expansion runs before variable expansion and globbing.
    (
        "brace_with_vars_and_glob",
        r#"mkdir -p "Show (2025)" && : > "Show (2025)/a.mkv" && : > "Show (2025)/b.txt" && d="Show (2025)" && f="ep 1.mkv" && printf '<%s>\n' "$d"/*.{mkv,txt} "$f"{,.bak} "$d"/{01..03} "$d/{a,b}""#,
    ),
];

/// Find the reference bash without ever failing the suite over a missing one.
fn reference_bash() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BASH_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let output = Command::new("which").arg("bash").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path = stdout.lines().next()?.trim();
    if path.is_empty() {
        return None;
    }
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

/// `~root` is host-dependent: only compare it when that user really has a home.
fn tilde_root_resolves() -> bool {
    if !Path::new("/root").is_dir() {
        return false;
    }
    nix::unistd::User::from_name("root")
        .ok()
        .flatten()
        .is_some()
}

fn sandbox(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sharky-bash-parity-{tag}-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create case sandbox");
    dir
}

fn run(bin: &Path, is_bash: bool, script: &str, dir: &Path) -> Output {
    let mut cmd = Command::new(bin);
    if is_bash {
        cmd.args(["--noprofile", "--norc", "-c", script]);
    } else {
        cmd.args(["--no-config", "-c", script]);
    }
    cmd.current_dir(dir)
        .env_clear()
        .envs(CONTROLLED_ENV.iter().copied())
        .output()
        .unwrap_or_else(|err| panic!("failed to run {}: {err}", bin.display()))
}

/// All regular files left in the sandbox, keyed by name, for side-effect checks.
fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return snapshot;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Ok(bytes) = std::fs::read(&path) {
                snapshot.insert(entry.file_name().to_string_lossy().into_owned(), bytes);
            }
        }
    }
    snapshot
}

#[test]
fn matches_bash_on_every_fase_a_semantic_fix() {
    let Some(bash) = reference_bash() else {
        eprintln!("skipping bash differential suite: no reference bash (set BASH_BIN)");
        return;
    };
    let sharky = Path::new(env!("CARGO_BIN_EXE_sharky"));
    let mut failures = Vec::new();
    let mut compared = 0;
    let mut skipped = 0;

    for (name, script) in CASES {
        if *name == "tilde_root_user" && !tilde_root_resolves() {
            eprintln!("skipping {name}: ~root does not resolve on this host");
            skipped += 1;
            continue;
        }

        let bash_dir = sandbox("bash");
        let sharky_dir = sandbox("sharky");
        let expected = run(&bash, true, script, &bash_dir);
        let actual = run(sharky, false, script, &sharky_dir);
        let expected_files = files(&bash_dir);
        let actual_files = files(&sharky_dir);
        std::fs::remove_dir_all(&bash_dir).ok();
        std::fs::remove_dir_all(&sharky_dir).ok();
        compared += 1;

        let mut diffs = Vec::new();
        if expected.stdout != actual.stdout {
            diffs.push(format!(
                "  stdout\n    bash:   {:?}\n    sharky: {:?}",
                String::from_utf8_lossy(&expected.stdout),
                String::from_utf8_lossy(&actual.stdout)
            ));
        }
        if expected.stderr.is_empty() != actual.stderr.is_empty() {
            diffs.push(format!(
                "  stderr presence\n    bash:   {} ({:?})\n    sharky: {} ({:?})",
                !expected.stderr.is_empty(),
                String::from_utf8_lossy(&expected.stderr),
                !actual.stderr.is_empty(),
                String::from_utf8_lossy(&actual.stderr)
            ));
        }
        if expected.status.code() != actual.status.code() {
            diffs.push(format!(
                "  status\n    bash:   {:?}\n    sharky: {:?}",
                expected.status.code(),
                actual.status.code()
            ));
        }
        if expected_files != actual_files {
            diffs.push(format!(
                "  files\n    bash:   {expected_files:?}\n    sharky: {actual_files:?}"
            ));
        }
        if !diffs.is_empty() {
            failures.push(format!("{name} (script: {script:?})\n{}", diffs.join("\n")));
        }
    }

    eprintln!("bash parity: {compared} cases compared, {skipped} skipped");
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n\n{}",
        failures.len(),
        compared,
        failures.join("\n\n")
    );
}
