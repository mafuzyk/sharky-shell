//! `test` and `[` evaluate conditions inside the shell, so a login profile can
//! use them before `PATH` provides an external `test`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-test-builtin-{}-{name}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs one snippet with a `PATH` that holds no `test`, `[` or coreutils.
fn run_without_path(home: &Path, source: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", source])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", home)
        .env("PATH", "/nonexistent")
        .output()
        .expect("failed to run sharky")
}

fn status(home: &Path, source: &str) -> i32 {
    let out = run_without_path(home, source);
    out.status.code().unwrap_or(-1)
}

fn evaluate(home: &Path, condition: &str) -> i32 {
    status(home, condition)
}

#[test]
fn conditions_work_with_no_usable_path() {
    let home = scratch("no-path");
    std::fs::write(home.join("file"), "data\n").unwrap();

    // The exact guard shapes the NixOS /etc/profile relies on.
    assert_eq!(evaluate(&home, r#"[ -n "x" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -z "" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -n "${MISSING:-}" ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ -f "$HOME/file" ]"#), 0);
    assert_eq!(evaluate(&home, r#"test -f "$HOME/file""#), 0);
    assert_eq!(evaluate(&home, r#"test -e "$HOME/missing""#), 1);

    // A profile branch that depends on the guard still runs. Only builtins are
    // used here, because this shell has no `PATH` to find anything else.
    let out = run_without_path(
        &home,
        r#"if [ -f "$HOME/file" ]; then true; else false; fi"#,
    );
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let out = run_without_path(
        &home,
        r#"if [ -f "$HOME/missing" ]; then true; else false; fi"#,
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn fixed_argument_counts_match_bash() {
    let home = scratch("argc");

    // Zero and one argument.
    assert_eq!(evaluate(&home, "[ ]"), 1);
    assert_eq!(evaluate(&home, r#"[ "" ]"#), 1);
    assert_eq!(evaluate(&home, "[ x ]"), 0);
    assert_eq!(evaluate(&home, "[ -n ]"), 0);
    assert_eq!(evaluate(&home, "[ ! ]"), 0);
    assert_eq!(evaluate(&home, "test"), 1);

    // Two arguments: `!` negates, otherwise the first word must be unary.
    assert_eq!(evaluate(&home, "[ ! x ]"), 1);
    assert_eq!(evaluate(&home, "[ ! -n ]"), 1);
    let out = run_without_path(&home, "[ = x ]");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("=: unary operator expected"),
        "{out:?}"
    );

    // Three arguments without a binary operator in the middle.
    let out = run_without_path(&home, "[ a b c ]");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("b: binary operator expected"),
        "{out:?}"
    );

    // A binary operator with no right operand is a syntax error, not a value.
    let out = run_without_path(&home, "[ x = ]");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("x: unary operator expected"),
        "{out:?}"
    );

    // `[` requires its closing bracket; `test` does not take one.
    let out = run_without_path(&home, "[ -n x");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("missing `]'"),
        "{out:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn string_comparisons_are_literal() {
    let home = scratch("strings");

    assert_eq!(evaluate(&home, "[ x = x ]"), 0);
    assert_eq!(evaluate(&home, "[ x = y ]"), 1);
    assert_eq!(evaluate(&home, "[ x != y ]"), 0);
    assert_eq!(evaluate(&home, r#"[ "a b" = "a b" ]"#), 0);
    // Globs belong to `[[ ]]`; `[` compares the bytes it received.
    assert_eq!(evaluate(&home, "[ a = a* ]"), 1);
    assert_eq!(evaluate(&home, r#"[ b \< a ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ y \> x ]"#), 0);

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn integer_operators_and_their_errors() {
    let home = scratch("integers");

    assert_eq!(evaluate(&home, "[ 3 -eq 3 ]"), 0);
    assert_eq!(evaluate(&home, "[ 03 -eq 3 ]"), 0);
    assert_eq!(evaluate(&home, r#"[ " 3 " -eq 3 ]"#), 0);
    assert_eq!(evaluate(&home, "[ +3 -lt 4 ]"), 0);
    assert_eq!(evaluate(&home, "[ -3 -lt 0 ]"), 0);
    assert_eq!(evaluate(&home, "[ 4 -ge 5 ]"), 1);
    assert_eq!(evaluate(&home, "[ 1 -ne 2 ]"), 0);

    let out = run_without_path(&home, "[ 3.0 -eq 3 ]");
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("3.0: integer expected"),
        "{out:?}"
    );
    assert_eq!(run_without_path(&home, "[ a -eq b ]").status.code(), Some(2));

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn unary_file_tests_follow_links_and_links_themselves() {
    let home = scratch("files");
    std::fs::write(home.join("full"), "data\n").unwrap();
    std::fs::write(home.join("empty"), "").unwrap();
    std::fs::create_dir(home.join("dir")).unwrap();
    std::os::unix::fs::symlink(home.join("full"), home.join("link")).unwrap();
    std::os::unix::fs::symlink(home.join("missing"), home.join("dangling")).unwrap();

    assert_eq!(evaluate(&home, r#"[ -f "$HOME/full" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -f "$HOME/dir" ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ -d "$HOME/dir" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -s "$HOME/full" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -s "$HOME/empty" ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ -e "$HOME/link" ]"#), 0);
    // `-e` follows the link, so a dangling one does not exist; `-L` sees it.
    assert_eq!(evaluate(&home, r#"[ -e "$HOME/dangling" ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ -L "$HOME/dangling" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -h "$HOME/full" ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ "$HOME/full" -ef "$HOME/link" ]"#), 0);
    assert_eq!(
        evaluate(&home, r#"[ "$HOME/full" -nt "$HOME/missing" ]"#),
        0
    );
    assert_eq!(
        evaluate(&home, r#"[ "$HOME/full" -ot "$HOME/missing" ]"#),
        1
    );
    assert_eq!(evaluate(&home, "[ -t 99 ]"), 1);

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn logical_operators_and_grouping() {
    let home = scratch("logical");

    assert_eq!(evaluate(&home, "[ -n x -a -n y ]"), 0);
    assert_eq!(evaluate(&home, r#"[ -n x -a -z y ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ -n "" -o -n x ]"#), 0);
    assert_eq!(evaluate(&home, "[ ! x = y ]"), 0);
    assert_eq!(evaluate(&home, r#"[ ! \( x = x \) ]"#), 1);
    assert_eq!(evaluate(&home, r#"[ \( x = x -a y = y \) ]"#), 0);
    // `-a` binds tighter than `-o`.
    assert_eq!(evaluate(&home, r#"[ -n x -o -n y -a -n "" ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ -n "" -a -n "" -o -n x ]"#), 0);
    assert_eq!(evaluate(&home, r#"[ ! ! x ]"#), 0);
    assert_eq!(evaluate(&home, "[ a = a -a b = b -a c = c ]"), 0);

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn both_names_report_themselves_as_builtins() {
    let home = scratch("type");
    let out = run_without_path(&home, "type test; type [");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "test is a shell builtin\n[ is a shell builtin\n"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn bracket_and_test_agree() {
    let home = scratch("agree");
    for (bracket, plain) in [
        ("[ -n x ]", "test -n x"),
        ("[ x = x ]", "test x = x"),
        ("[ 2 -gt 1 ]", "test 2 -gt 1"),
        ("[ ! -f /nonexistent ]", "test ! -f /nonexistent"),
    ] {
        assert_eq!(evaluate(&home, bracket), 0, "{bracket}");
        assert_eq!(evaluate(&home, plain), 0, "{plain}");
    }
    std::fs::remove_dir_all(&home).ok();
}
