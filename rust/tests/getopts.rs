//! `getopts`: parse option arguments with POSIX semantics.
//!
//! Expected bytes, statuses and OPTIND/OPTARG states were measured against
//! bash 5.3.20. Each test pins one observable behaviour of the state machine,
//! including the `shift $((OPTIND-1))` idiom scripts rely on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-getopts-test";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-getopts-builtin-{}-{name}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky(home: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", home)
        .env("PATH", "/run/current-system/sw/bin:/usr/bin:/bin")
        .output()
        .expect("failed to run sharky")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn iterates_over_explicit_arguments() {
    let home = scratch("iterate");
    let out = sharky(&home, r#"while getopts "ab" o -a -b; do echo "$o"; done"#);
    assert_eq!(stdout(&out), "a\nb\n", "{out:?}");
}

#[test]
fn required_argument_lands_in_optarg() {
    let home = scratch("optarg");
    let out = sharky(&home, r#"getopts "b:" o -b val; echo "$o [$OPTARG] $OPTIND""#);
    assert_eq!(stdout(&out), "b [val] 3\n", "{out:?}");
}

#[test]
fn attached_argument_is_taken_from_the_same_word() {
    let home = scratch("attached");
    let out = sharky(
        &home,
        r#"getopts "ab:" o -abval; echo "$o $OPTIND"; getopts "ab:" o -abval; echo "$o [$OPTARG] $OPTIND""#,
    );
    assert_eq!(stdout(&out), "a 1\nb [val] 2\n", "{out:?}");
}

#[test]
fn clustered_options_keep_optind_on_the_same_word() {
    let home = scratch("cluster");
    let out = sharky(
        &home,
        r#"set -- -abc; getopts "abc" o; echo "$o $OPTIND"; getopts "abc" o; echo "$o $OPTIND"; getopts "abc" o; echo "$o $OPTIND""#,
    );
    assert_eq!(stdout(&out), "a 1\nb 1\nc 2\n", "{out:?}");
}

#[test]
fn invalid_option_reports_question_mark_and_diagnostic() {
    let home = scratch("invalid");
    let out = sharky(
        &home,
        r#"getopts "a" o -x; echo "s=$? o=$o arg=${OPTARG-UNSET} ind=$OPTIND""#,
    );
    assert_eq!(stdout(&out), "s=0 o=? arg=UNSET ind=2\n", "{out:?}");
    assert!(
        stderr(&out).contains("illegal option -- x"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn silent_mode_puts_the_bad_option_in_optarg() {
    let home = scratch("silent-invalid");
    let out = sharky(&home, r#"getopts ":a" o -x; echo "$o [${OPTARG-UNSET}]""#);
    assert_eq!(stdout(&out), "? [x]\n", "{out:?}");
    assert_eq!(stderr(&out), "", "{out:?}");
}

#[test]
fn silent_mode_reports_a_missing_argument_with_colon() {
    let home = scratch("silent-missing");
    let out = sharky(&home, r#"getopts ":a:" o -a; echo "$o [${OPTARG-UNSET}]""#);
    assert_eq!(stdout(&out), ": [a]\n", "{out:?}");
    assert_eq!(stderr(&out), "", "{out:?}");
}

#[test]
fn missing_argument_reports_question_mark_and_diagnostic() {
    let home = scratch("missing");
    let out = sharky(
        &home,
        r#"getopts "a:" o -a; echo "$o [${OPTARG-UNSET}] ind=$OPTIND""#,
    );
    assert_eq!(stdout(&out), "? [UNSET] ind=2\n", "{out:?}");
    assert!(
        stderr(&out).contains("option requires an argument -- a"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn end_of_options_fails_and_sets_question_mark() {
    let home = scratch("eof");
    let out = sharky(&home, r#"getopts "a" o -a; getopts "a" o -a; echo "s=$? o=$o""#);
    assert_eq!(stdout(&out), "s=1 o=?\n", "{out:?}");
}

#[test]
fn eof_unsets_optarg() {
    let home = scratch("eof-optarg");
    let out = sharky(
        &home,
        r#"getopts "a:" o -a val; getopts "a:" o -a val; echo "[${OPTARG-UNSET}] ind=$OPTIND""#,
    );
    assert_eq!(stdout(&out), "[UNSET] ind=3\n", "{out:?}");
}

#[test]
fn parses_positional_parameters_by_default() {
    let home = scratch("positional");
    let out = sharky(
        &home,
        r#"set -- -a -b; while getopts "ab" o; do :; done; shift $((OPTIND-1)); echo "$#""#,
    );
    assert_eq!(stdout(&out), "0\n", "{out:?}");
}

#[test]
fn double_dash_ends_options_and_is_counted() {
    let home = scratch("dashdash");
    let out = sharky(&home, r#"getopts "a" o -- -a; echo "s=$? ind=$OPTIND""#);
    assert_eq!(stdout(&out), "s=1 ind=2\n", "{out:?}");
    let out = sharky(
        &home,
        r#"set -- -a -b -- rest; while getopts "ab" o; do echo "$o $OPTIND"; done; shift $((OPTIND-1)); echo "n=$# [$1]""#,
    );
    assert_eq!(stdout(&out), "a 2\nb 3\nn=1 [rest]\n", "{out:?}");
}

#[test]
fn a_lone_dash_is_not_an_option() {
    let home = scratch("lone-dash");
    let out = sharky(&home, r#"getopts "a" o - --; echo "s=$? ind=$OPTIND""#);
    assert_eq!(stdout(&out), "s=1 ind=1\n", "{out:?}");
}

#[test]
fn assigning_optind_one_resets_the_scan() {
    let home = scratch("reset");
    let out = sharky(&home, r#"set -- -a x; getopts "a" o; OPTIND=1; getopts "a" o; echo "o=$o s=$?""#);
    assert_eq!(stdout(&out), "o=a s=0\n", "{out:?}");
}

#[test]
fn assigning_optind_one_restarts_a_clustered_word() {
    let home = scratch("reset-cluster");
    // The assignment resets the scan even though OPTIND already reads 1.
    let out = sharky(
        &home,
        r#"OPTIND=1; getopts "abc" o -abc; OPTIND=1; getopts "abc" o -abc; echo "$o $OPTIND""#,
    );
    assert_eq!(stdout(&out), "a 1\n", "{out:?}");
}

#[test]
fn opterr_zero_suppresses_the_diagnostic() {
    let home = scratch("opterr");
    let out = sharky(&home, r#"OPTERR=0; getopts "a" o -x; echo "$o""#);
    assert_eq!(stdout(&out), "?\n", "{out:?}");
    assert_eq!(stderr(&out), "", "{out:?}");
}

#[test]
fn explicit_arguments_take_precedence_over_positional_parameters() {
    let home = scratch("precedence");
    let out = sharky(&home, r#"set -- -b; getopts "a" o -a; echo "$o""#);
    assert_eq!(stdout(&out), "a\n", "{out:?}");
}

#[test]
fn missing_operands_print_usage_and_fail() {
    let home = scratch("usage");
    let out = sharky(&home, r#"getopts; echo "s=$?""#);
    assert_eq!(stdout(&out), "s=2\n", "{out:?}");
    assert!(
        stderr(&out).contains("usage: getopts"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn invalid_identifier_is_rejected() {
    let home = scratch("identifier");
    let out = sharky(&home, r#"getopts "a" 1bad -a; echo "s=$?""#);
    assert_eq!(stdout(&out), "s=1\n", "{out:?}");
    assert!(
        stderr(&out).contains("not a valid identifier"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn empty_argument_is_a_present_option_argument() {
    let home = scratch("empty-arg");
    let out = sharky(&home, r#"set -- -a ""; getopts "a:" o; echo "[$OPTARG]""#);
    assert_eq!(stdout(&out), "[]\n", "{out:?}");
}

#[test]
fn success_returns_zero() {
    let home = scratch("success");
    let out = sharky(&home, r#"getopts "a" o -a; echo "s=$?""#);
    assert_eq!(stdout(&out), "s=0\n", "{out:?}");
}

#[test]
fn a_non_option_word_ends_the_scan_without_being_skipped() {
    let home = scratch("non-option");
    let out = sharky(&home, r#"getopts "a" o x -a; echo "s=$? ind=$OPTIND""#);
    assert_eq!(stdout(&out), "s=1 ind=1\n", "{out:?}");
}

#[test]
fn a_colon_only_optstring_defines_no_options() {
    let home = scratch("colon-only");
    let out = sharky(&home, r#"getopts ":" o -x; echo "s=$? o=$o [${OPTARG-UNSET}]""#);
    assert_eq!(stdout(&out), "s=0 o=? [x]\n", "{out:?}");
    assert_eq!(stderr(&out), "", "{out:?}");
}
