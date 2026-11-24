//! Differential regression tests for shell redirection forms (P1-10).
//!
//! Each case runs the same script under a reference bash and under sharky and
//! compares stdout bytes, exit status and resulting file contents. A missing
//! bash oracle skips the suite instead of reporting a false regression.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const CONTROLLED_ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/bin:/bin:/run/current-system/sw/bin"),
    ("HOME", "/tmp"),
    ("LC_ALL", "C.UTF-8"),
];

/// (case name, script). Scripts create their own files in the sandbox so the
/// two interpreters start from identical empty directories.
const CASES: &[(&str, &str)] = &[
    // |& is stderr redirection into the next stage of the pipeline.
    ("pipe_and_explicit_stderr_keeps_original", "{ echo hi >&2 |& cat; } 2>err; echo \"err=$(cat err)\""),
    // Implicit 2>&1 of |& is appended after a simple command's redirections.
    ("pipe_and_simple_explicit_stderr_keeps_original", "echo hi >&2 |& cat; echo \"st=$?\""),
    ("pipe_and_simple_explicit_stdout_file", "echo hi >out |& cat; echo \"st=$? out=$(cat out)\""),
    ("pipe_and_external_stderr_is_merged", "/bin/sh -c 'echo e >&2' |& cat; echo \"st=$?\""),
    ("pipe_and_group_internal_stderr_is_merged", "{ echo hi >&2; } |& cat; echo \"st=$?\""),
    ("pipe_and_group_stderr_to_null", "{ echo e >&2; } 2>/dev/null |& cat; echo \"st=$?\""),
    ("pipe_and_group_no_explicit_redirection", "{ printf out\\n; printf err\\n >&2; } |& cat; echo \"st=$?\""),
    ("pipe_and_merges_stderr", "{ printf out\\n; printf err\\n >&2; } |& cat"),
    ("pipe_and_captures_both_streams", "{ printf out\\n; printf err\\n >&2; } |& cat"),
    ("pipe_and_pipestatus", "{ printf x\\n >&2; } |& cat; echo \"${PIPESTATUS[@]}\""),
    ("pipe_and_with_pipefail", "set -o pipefail; { printf x\\n >&2; } |& cat; echo \"st=$? ps=${PIPESTATUS[@]}\""),
    ("plain_pipe_keeps_stderr_separate", "printf x\\n >&2 | cat 2>/dev/null; echo \"st=$?\""),
    // Closing descriptors.
    ("close_output_fd_with_exec", "exec 3>f; printf hi\\n >&3; exec 3>&-; printf done\\n; cat f"),
    ("close_input_fd_with_exec", "printf data\\n > f; exec 3<f; exec 3<&-; printf ok\\n; cat f"),
    ("close_stdout_on_command", "printf hidden\\n >&2; /bin/echo shown 1>&- 2>/dev/null; printf 'rc=%s\\n' \"$?\""),
    ("close_stdin_causes_read_error", "printf abc\\n | { read x 0<&-; echo \"rc=$?\"; }"),
    // Duplicating descriptors.
    ("dup_output_fd", "printf data\\n > f; cat 3<f <&3"),
    ("dup_output_fd_after_open", "printf data\\n > f; exec 3<f; cat <&3; echo \"rc=$?\""),
    ("dup_output_fd_to_file", "exec 3>f; printf hi\\n >&3; exec 3>&-; cat f"),
    ("dup_stderr_to_stdout", "{ printf out\\n; printf err\\n >&2; } 2>&1 | cat"),
    ("dup_input_fd_missing_is_error", "cat <&9 2>f; echo \"st=$? non-empty=$(test -s f && echo yes || echo no)\""),
    // Ordering of redirections on the same command.
    ("redirect_order_duplicate_then_write", "printf data\\n > f; cat <f 0<&- 2>e; echo \"st=$? err=$(wc -c < e)\""),
    ("redirect_order_write_then_duplicate", "printf data\\n > f; cat 0<&- <f 2>e; echo \"st=$? out=$(cat 2>/dev/null)\""),
    ("group_redirections", "{ printf first\\n; printf second\\n >&2; } > f 2>&1; cat f"),
    ("noclobber_force_still_works", "printf a > f; set -C; printf b >| f; echo \"st=$? content=$(cat f)\""),
];

fn reference_bash() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("BASH_BIN") {
        let path = PathBuf::from(path);
        return path.is_file().then_some(path);
    }
    let output = Command::new("which").arg("bash").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = PathBuf::from(String::from_utf8_lossy(&output.stdout).lines().next()?.trim());
    path.is_file().then_some(path)
}

fn sandbox(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sharky-redirect-parity-{tag}-{}-{n}",
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

fn files(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return snapshot;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && let Ok(bytes) = std::fs::read(&path)
        {
            snapshot.insert(entry.file_name().to_string_lossy().into_owned(), bytes);
        }
    }
    snapshot
}

#[test]
fn redirection_forms_match_bash() {
    let Some(bash) = reference_bash() else {
        eprintln!("skipping redirection differential suite: no reference bash (set BASH_BIN)");
        return;
    };
    let sharky = Path::new(env!("CARGO_BIN_EXE_sharky"));
    let mut failures = Vec::new();
    let mut compared = 0;

    for (name, script) in CASES {
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

    eprintln!("redirection parity: {compared} cases compared");
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n\n{}",
        failures.len(),
        compared,
        failures.join("\n\n")
    );
}
