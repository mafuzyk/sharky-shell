//! Focused integration tests for the builtins added by the roadmap.
//!
//! Each group runs the real binary through `-c` and, where the behavior is
//! stable outside a terminal, compares it with bash 5.x.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-builtins-extra-test";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-builtins-extra-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn run(binary: &Path, args: &[&str], script: &str, cwd: &Path, extra: &[(&str, &str)]) -> Output {
    let mut command = Command::new(binary);
    command
        .args(args)
        .arg(script)
        .current_dir(cwd)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", NO_CONFIG)
        .env("PATH", common::tool_path())
        .envs(extra.iter().copied());
    command.output().unwrap_or_else(|err| panic!("run {}: {err}", binary.display()))
}

fn sharky(script: &str, cwd: &Path) -> Output {
    run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        script,
        cwd,
        &[],
    )
}

fn bash(script: &str, cwd: &Path) -> Output {
    // NixOS has no /bin/bash; the same helper the other tests use finds it.
    run(
        &common::resolve("bash"),
        &["--noprofile", "--norc", "-c"],
        script,
        cwd,
        &[],
    )
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

fn assert_same_as_bash(script: &str, cwd: &Path) {
    let expected = bash(script, cwd);
    let actual = sharky(script, cwd);
    assert_eq!(
        stdout(&actual),
        stdout(&expected),
        "stdout differs for {script:?}\nsharky stderr: {}\nbash stderr: {}",
        stderr(&actual),
        stderr(&expected)
    );
    assert_eq!(
        status(&actual),
        status(&expected),
        "status differs for {script:?}\nsharky stderr: {}\nbash stderr: {}",
        stderr(&actual),
        stderr(&expected)
    );
}

// --- pushd / popd / dirs ---

fn dirstack_root(name: &str) -> PathBuf {
    let root = scratch(name);
    for dir in ["a", "b", "c"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    root
}

#[test]
fn dirstack_popd_removes_top_and_follows_it_like_bash() {
    let root = dirstack_root("dirstack-popd-fix");
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");
    let cwd = |path: &Path| format!("cd {};", path.display());
    assert_same_as_bash(
        &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; dirs; popd >/dev/null; dirs; pwd", cwd(&a), b.display(), c.display()),
        &root,
    );
    assert_same_as_bash(
        &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; pushd >/dev/null; dirs; pwd", cwd(&a), b.display(), c.display()),
        &root,
    );
    for spec in ["+0", "+1", "+2", "+3", "-0", "-1", "-2", "-3"] {
        assert_same_as_bash(
            &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; popd {spec} >/dev/null; dirs -p; pwd", cwd(&a), b.display(), c.display()),
            &root,
        );
    }
    for spec in ["+0", "+1", "+2", "+3", "-0", "-1", "-2", "-3"] {
        assert_same_as_bash(
            &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; pushd {spec} >/dev/null; dirs -p; pwd", cwd(&a), b.display(), c.display()),
            &root,
        );
    }
    for spec in ["+0", "+1", "+2", "-0", "-1", "-2", "-3"] {
        assert_same_as_bash(
            &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; pushd -n {spec} >/dev/null; dirs -p; pwd", cwd(&a), b.display(), c.display()),
            &root,
        );
    }
    assert_same_as_bash(
        &format!("{} pushd -n {}; dirs -p; pwd", cwd(&a), b.display()),
        &root,
    );
    for spec in ["", "+1", "+2", "+3", "-1", "-2", "-3"] {
        assert_same_as_bash(
            &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; popd -n {spec} >/dev/null; dirs -p; pwd", cwd(&a), b.display(), c.display()),
            &root,
        );
    }
    assert_same_as_bash(
        &format!("{} pushd {} >/dev/null; pushd {} >/dev/null; dirs -p; dirs -l; dirs -v", cwd(&a), b.display(), c.display()),
        &root,
    );
}

#[test]
fn dirs_home_abbreviation_requires_a_path_boundary() {
    let root = scratch("dirs-home");
    let home = root.join("home");
    let sibling = root.join("homex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&sibling).unwrap();
    let script = format!("cd {}; dirs", sibling.display());
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        &script,
        &root,
        &[("HOME", home.to_str().unwrap())],
    );
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains(&sibling.to_string_lossy().into_owned()), "unexpected abbreviation: {}", stdout(&out));
    assert!(!stdout(&out).contains('~'), "unexpected abbreviation: {}", stdout(&out));
}

#[test]
fn dirstack_pushd_popd_and_dirs_match_bash() {
    let root = dirstack_root("dirstack");
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");
    let setup = format!(
        "cd {}; pushd {} >/dev/null; pushd {} >/dev/null; ",
        a.display(), b.display(), c.display(),
    );
    assert_same_as_bash(&format!("{setup}dirs -v"), &root);
    assert_same_as_bash(&format!("{setup}pushd +1"), &root);
    assert_same_as_bash(&format!("{setup}pushd -1"), &root);
    assert_same_as_bash(&format!("{setup}popd +0; dirs -v"), &root);
    assert_same_as_bash(&format!("{setup}dirs -p; dirs -l; dirs -v"), &root);
}

#[test]
fn dirstack_rotation_and_cd_minus_match_bash() {
    let root = dirstack_root("rotation");
    let a = root.join("a");
    let b = root.join("b");
    let setup = format!("cd {}; pushd {} >/dev/null; ", a.display(), b.display());
    assert_same_as_bash(&format!("{setup}pushd; echo PWD=$PWD OLDPWD=$OLDPWD"), &root);
    assert_same_as_bash(
        &format!("{setup}cd - >/dev/null; echo PWD=$PWD OLDPWD=$OLDPWD; dirs -v"),
        &root,
    );
}

#[test]
fn dirstack_errors_are_loud() {
    let root = dirstack_root("errors");
    let a = root.join("a");
    let b = root.join("b");
    let setup = format!("cd {}; pushd {} >/dev/null; ", a.display(), b.display());
    for script in [
        format!("{setup}pushd +9; echo s=$?"),
        format!("{setup}pushd -9; echo s=$?"),
        format!("{setup}popd +9; echo s=$?"),
        format!("{setup}popd; popd; echo s=$?"),
        format!("{setup}dirs -x; echo s=$?"),
    ] {
        let out = sharky(&script, &root);
        assert!(!stderr(&out).is_empty(), "no diagnostic for {script:?}");
        assert!(
            stdout(&out).contains("s=1") || stdout(&out).contains("s=2"),
            "unexpected status for {script:?}: {}",
            stdout(&out)
        );
    }
}

// --- ulimit ---

#[test]
fn ulimit_reports_and_sets_limits_like_bash() {
    let root = scratch("ulimit");
    let out = sharky("ulimit -n; ulimit -c; ulimit -Sn; ulimit -Hn", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 4, "{}", stdout(&out));
    assert!(lines[0].parse::<u64>().is_ok(), "{}", stdout(&out));
    let expected = bash("ulimit -n; ulimit -c; ulimit -Sn; ulimit -Hn", &root);
    assert_eq!(stdout(&expected), stdout(&out), "bash vs sharky");

    let both = sharky("ulimit -H -S -n", &root);
    assert_eq!(status(&both), 0, "{}", stderr(&both));
    let both_bash = bash("ulimit -H -S -n", &root);
    assert_eq!(stdout(&both_bash), stdout(&both), "bash vs sharky (-H -S)");

    // Only lowering a soft limit is guaranteed to succeed for an unprivileged
    // process; raising it back up to the original hard value would need root.
    let lowered = sharky("ulimit -Sn 64; ulimit -n; echo s=$?", &root);
    assert_eq!(stdout(&lowered).trim_end(), "64\ns=0", "{}", stdout(&lowered));
    let hard_raise = sharky("ulimit -Hn 999999999; echo s=$?", &root);
    assert!(stdout(&hard_raise).contains("s=1"), "{}", stdout(&hard_raise));
}

#[test]
fn ulimit_both_flags_set_soft_and_hard() {
    let root = scratch("ulimit-both");
    let out = sharky("ulimit -H -S -c 0; ulimit -Sc; ulimit -Hc", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out).trim_end(), "0\n0", "{} {}", stdout(&out), stderr(&out));
}

#[test]
fn ulimit_all_lists_the_supported_resources() {
    let root = scratch("ulimit-all");
    let out = sharky("ulimit -a", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    for fragment in [
        "core file size              (blocks, -c)",
        "file size                   (blocks, -f)",
        "open files                          (-n)",
        "stack size                  (kbytes, -s)",
        "cpu time                   (seconds, -t)",
        "max user processes                  (-u)",
        "virtual memory              (kbytes, -v)",
    ] {
        assert!(stdout(&out).contains(fragment), "missing {fragment:?}: {}", stdout(&out));
    }
}

#[test]
fn ulimit_rejects_unknown_options_and_values() {
    let root = scratch("ulimit-bad");
    for (script, expected_status, expected_stderr) in [
        ("ulimit -Z; echo s=$?", "s=2", true),
        ("ulimit -n abc; echo s=$?", "s=1", true),
        ("ulimit -n 999999999999999999999999; echo s=$?", "s=1", true),
        ("ulimit -H -S -n; echo s=$?", "s=0", false),
        ("ulimit -H -S -n 999999999999999999999999; echo s=$?", "s=1", true),
    ] {
        let out = sharky(script, &root);
        assert_eq!(expected_stderr, !stderr(&out).is_empty(), "diagnostic mismatch: {script:?}: {}", stderr(&out));
        assert!(stdout(&out).contains(expected_status), "unexpected status: {script:?} => {}", stdout(&out));
    }
}

// --- hash ---

#[test]
fn hash_tracks_external_commands_and_reports_them() {
    let root = scratch("hash");
    let out = sharky("hash; echo s=$?", &root);
    assert_eq!(status(&out), 0);
    assert!(stdout(&out).contains("s=1"), "empty table status: {}", stdout(&out));
    assert!(!stderr(&out).is_empty(), "empty table must be loud");

    // A name with a slash is never hashed by bash; use PATH lookup instead.
    let out = sharky("env >/dev/null; hash -t env", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    let path = stdout(&out).trim().to_string();
    assert!(path.contains("/env"), "unexpected cached path: {path}");

    let out = sharky("env >/dev/null; hash; hash -l", &root);
    assert!(stdout(&out).contains("env"), "table missing command: {}", stdout(&out));
    assert!(stdout(&out).contains("builtin hash -p"), "reusable listing missing: {}", stdout(&out));

    let out = sharky("env >/dev/null; hash -r; hash -t env; echo s=$?", &root);
    assert!(stdout(&out).contains("s=1"), "reset failed: {}", stdout(&out));
}

#[test]
fn disown_removes_jobs_from_the_job_table() {
    let root = scratch("disown");
    let out = sharky("sleep 5 & jobs -p; disown; jobs -p; echo s=$?", &root);
    let first_output = stdout(&out);
    let first = first_output.lines().next().unwrap_or_default();
    assert!(first.chars().all(|c| c.is_ascii_digit()), "missing first jobs -p: {first_output:?}");
    assert_eq!(
        first_output.lines().skip(1).collect::<Vec<_>>(),
        vec!["s=0"],
        "job still listed after disown: {first_output:?}"
    );

    let out = sharky("sleep 5 & jobs -p; disown -h; jobs -p; echo s=$?", &root);
    assert_eq!(
        stdout(&out).lines().filter(|line| line.chars().all(|c| c.is_ascii_digit())).count(),
        2,
        "disown -h removed the job: {:?}", stdout(&out)
    );

    let out = sharky("disown -x; echo s=$?", &root);
    assert!(stderr(&out).contains("invalid option"));
    assert!(stdout(&out).contains("s=2"));
}

#[test]
fn let_evaluates_arithmetic_and_reports_status() {
    let root = scratch("let");
    let out = sharky("let x=3+4; echo $x; let x+=1 y=x*2; echo $x,$y", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "7\n8,16\n");
    assert_same_as_bash("let x=3+4; echo $x; let x+=1 y=x*2; echo $x,$y", &root);

    let out = sharky("let 0; echo s=$?", &root);
    assert!(stdout(&out).contains("s=1"));
    assert_same_as_bash("let 0; echo s=$?", &root);
    let out = sharky("let 'x = 1 / 0'; echo s=$?", &root);
    assert!(stdout(&out).contains("s=1"), "{}", stdout(&out));
    assert!(stderr(&out).contains("arithmetic"));
    assert_same_as_bash("let 'x = 1 / 0'; echo s=$?", &root);
}

#[test]
fn times_reports_shell_and_child_cpu_time() {
    let root = scratch("times");
    let out = sharky("times; echo s=$?", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains('m'), "unexpected times output: {text}");
    assert!(text.lines().count() >= 2, "unexpected times output: {text}");
    // CPU values vary, but bash uses the same two-line `MmSS.mmmS` shape.
    let bash_out = bash("times", &root);
    let bash_text = stdout(&bash_out);
    for (actual, expected) in text.lines().zip(bash_text.lines()) {
        assert_eq!(actual.split_whitespace().count(), expected.split_whitespace().count());
        assert!(actual.ends_with('s'), "unexpected times line: {actual}");
    }
}

#[test]
fn heredoc_falls_back_when_tmpdir_is_unusable() {
    let root = scratch("heredoc-tmpdir");
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "cat <<EOF\nhi\nEOF",
        &root,
        &[("TMPDIR", "/nonexistent/sharky-builtins-test")],
    );
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "hi\n");
}

#[test]
fn temp_dir_fallback_filters_empty_paths() {
    let root = scratch("tmpdir-empty");
    let cwd = root.join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();

    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "cat <<EOF\nhi\nEOF",
        &cwd,
        &[("TMPDIR", "")],
    );
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out), "hi\n");
    let leftovers: Vec<_> = std::fs::read_dir(&cwd)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("sharky-here-"))
        .collect();
    assert!(leftovers.is_empty(), "relative heredoc file left behind: {leftovers:?}");

    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "--json", "-c"],
        "printf HI",
        &cwd,
        &[("TMPDIR", "")],
    );
    assert_eq!(status(&out), 0, "{} {}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("HI"), "{}", stdout(&out));
}

#[test]
fn capture_and_fifo_substitution_fall_back_from_missing_tmpdir() {
    let root = scratch("tmpdir-missing");
    let missing = root.join("missing");
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "--json", "-c"],
        "printf HI",
        &root,
        &[("TMPDIR", missing.to_str().unwrap())],
    );
    assert_eq!(status(&out), 0, "{} {}", stdout(&out), stderr(&out));
    assert!(stdout(&out).contains("HI"), "{}", stdout(&out));

    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "cat <(printf hi)",
        &root,
        &[
            ("TMPDIR", missing.to_str().unwrap()),
            ("SHARKY_PROCESS_SUBST_FIFO", "1"),
        ],
    );
    assert_eq!(status(&out), 0, "{} {}", stdout(&out), stderr(&out));
    assert_eq!(stdout(&out), "hi");
}

#[test]
fn hash_cache_does_not_leak_across_shells_or_paths() {
    let root = scratch("hash-diag");
    let tool = common::resolve("env");
    let tool_dir = tool.parent().unwrap().to_str().unwrap();

    // A lookup through the shell's PATH is cached for that shell only.
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "env >/dev/null; hash -t env",
        &root,
        &[("PATH", tool_dir)],
    );
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), tool.to_str().unwrap());

    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "hash -t env; echo s=$?",
        &root,
        &[("PATH", tool_dir)],
    );
    assert!(stdout(&out).contains("s=1"), "cache leaked between shells: {}", stdout(&out));

    // Changing PATH invalidates a cached hit: bash hashes the new location.
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "env >/dev/null; PATH=/nonexistent env >/dev/null 2>/dev/null; hash -t env; echo s=$?",
        &root,
        &[("PATH", tool_dir)],
    );
    assert!(
        stdout(&out).contains(&format!("{}/env", tool_dir)),
        "stale cache after PATH change: {}",
        stdout(&out),
    );

    // `command -p` uses the default PATH and must not poison the normal
    // hash table with its private result.
    let out = run(
        Path::new(env!("CARGO_BIN_EXE_sharky")),
        &["--no-config", "-c"],
        "PATH=/nonexistent; command -p env >/dev/null; hash -t env; echo s=$?",
        &root,
        &[("PATH", "/nonexistent")],
    );
    assert!(stdout(&out).contains("s=1"), "command -p polluted cache: {}", stdout(&out));
}

#[test]
fn hash_reset_is_applied_when_p_combines_with_earlier_flags() {
    let root = scratch("hash-flags");
    let out = sharky("hash -p /bin/true known; hash -r -p /bin/false fake; hash", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert!(stdout(&out).contains("fake"), "missing new entry: {}", stdout(&out));
    assert!(!stdout(&out).contains("known"), "-r was ignored: {}", stdout(&out));
}

#[test]
fn hash_accepts_explicit_paths_and_rejects_bad_options() {
    let root = scratch("hash-bad");
    let out = sharky("hash -p /bin/false fake; hash -t fake", &root);
    assert_eq!(status(&out), 0, "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "/bin/false");
    let out = sharky("hash -x; echo s=$?", &root);
    assert!(stderr(&out).contains("invalid option"), "{}", stderr(&out));
    assert!(stdout(&out).contains("s=2"), "{}", stdout(&out));
}
