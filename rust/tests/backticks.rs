//! Old-style command substitution with backticks: `` `cmd` ``.
//!
//! Every expectation in this file was measured against GNU bash 5.3.20(1)
//! invoked as `env -i PATH=... LC_ALL=C.UTF-8 bash --noprofile --norc -c`.
//!
//! Backticks become the same `Part::CmdSub` that the `$(...)` form produces, so
//! newline stripping, word splitting, globbing and the substitution status are
//! shared with `$(...)` instead of being reimplemented.
//!
//! The one deliberate difference from `$(...)` is the backslash pre-pass: inside
//! backticks bash removes one level of backslash before `$`, `` ` `` and `\`
//! *before* the inner command is parsed, which is why `` `echo \$HOME` `` expands
//! HOME while `$(echo \$HOME)` prints a literal `$HOME`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";
const PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sharky-backticks-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_in(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", PATH)
        .current_dir(dir)
        .output()
        .expect("failed to run sharky")
}

fn sharky(script: &str) -> Output {
    let dir = temp_dir();
    let out = sharky_in(&dir, script);
    std::fs::remove_dir_all(&dir).ok();
    out
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

#[test]
fn basic_substitution_runs_the_command() {
    let out = sharky("echo `echo hi`");
    assert_eq!(stdout(&out), "hi\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn trailing_newlines_are_stripped() {
    let out = sharky("v=`printf 'a\\nb\\n\\n\\n'`; printf '[%s]' \"$v\"; echo");
    assert_eq!(stdout(&out), "[a\nb]\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn substitution_inside_double_quotes_expands() {
    let out = sharky("echo \"`echo hi`\"");
    assert_eq!(stdout(&out), "hi\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn substitution_inside_double_quotes_keeps_surrounding_text() {
    let out = sharky("echo \"x`echo hi`y\"");
    assert_eq!(stdout(&out), "xhiy\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn single_quotes_keep_backticks_literal() {
    let out = sharky("echo '`echo hi`'");
    assert_eq!(stdout(&out), "`echo hi`\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn escaped_backticks_inside_double_quotes_are_literal() {
    let out = sharky("echo \"\\`echo hi\\`\"");
    assert_eq!(stdout(&out), "`echo hi`\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Bash removes one backslash from `\$` inside backticks before parsing the inner
// command, so the inner shell still sees an expansion. This is the documented
// difference from `$(...)`.
#[test]
fn one_backslash_before_dollar_is_removed() {
    let out = sharky("HOME=/h; echo `echo \\$HOME`");
    assert_eq!(stdout(&out), "/h\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn two_backslashes_before_dollar_leave_a_literal_dollar() {
    let out = sharky("HOME=/h; echo `echo \\\\$HOME`");
    assert_eq!(stdout(&out), "$HOME\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Four backslashes lose one level at scan time (`` \\ `` -> `\`), so the inner
// command is `echo \\` and it prints a single backslash.
#[test]
fn four_backslashes_become_two_in_the_inner_command() {
    let out = sharky("echo `echo \\\\\\\\`");
    assert_eq!(stdout(&out), "\\\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Only `$`, `` ` `` and `\` lose a backslash at scan time; every other backslash
// reaches the inner parser untouched, which is why `\t` below becomes the two
// characters `t`.
#[test]
fn backslash_before_other_characters_reaches_the_inner_parser() {
    let out = sharky("echo `echo a\\tb`");
    assert_eq!(stdout(&out), "atb\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn nested_backticks_need_the_inner_pair_escaped() {
    let out = sharky("echo `echo \\`echo deep\\``");
    assert_eq!(stdout(&out), "deep\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Without the escapes the first unescaped backtick ends the substitution, so the
// inner command is `echo ` and the trailing `echo deep` is a second word of the
// outer command. Bash yields "echo deep".
#[test]
fn unescaped_nested_backticks_end_the_substitution_early() {
    let out = sharky("echo `echo `echo deep``");
    assert_eq!(stdout(&out), "echo deep\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn substitutions_concatenate_with_adjacent_text() {
    let out = sharky("echo a`echo b`c`echo d`e");
    assert_eq!(stdout(&out), "abcde\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_backticks_expand_to_nothing() {
    let out = sharky("printf '[%s]\\n' `printf ''`; echo \"A``B\"");
    assert_eq!(stdout(&out), "[]\nAB\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn a_newline_inside_the_substitution_separates_inner_commands() {
    let out = sharky("echo `echo one\ntwo`");
    assert_eq!(stdout(&out), "one\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("two"),
        "expected a diagnostic about `two`, got {:?}",
        stderr(&out)
    );
}

// An unterminated backtick must be a syntax error, never a word that silently
// keeps the backtick in its output.
#[test]
fn an_unterminated_backtick_is_a_syntax_error() {
    let out = sharky("echo `echo hi");
    assert_eq!(stdout(&out), "", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 2);
    assert!(!stderr(&out).is_empty(), "expected a diagnostic");
}

#[test]
fn heredoc_bodies_substitute() {
    let out = sharky("cat <<EOF\na `echo hi` b\nEOF");
    assert_eq!(stdout(&out), "a hi b\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn quoted_heredoc_delimiters_keep_backticks_literal() {
    let out = sharky("cat <<'EOF'\na `echo hi` b\nEOF");
    assert_eq!(stdout(&out), "a `echo hi` b\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_stores_the_result() {
    let out = sharky("v=`printf 'x y'`; printf '[%s]\\n' \"$v\"");
    assert_eq!(stdout(&out), "[x y]\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn for_word_list_splits_the_result() {
    let out = sharky("for x in `echo a b`; do printf '[%s]' \"$x\"; done; echo");
    assert_eq!(stdout(&out), "[a][b]\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn redirect_target_uses_the_result() {
    let dir = temp_dir();
    let out = sharky_in(&dir, "echo hi > `echo out.txt`; cat out.txt");
    assert_eq!(stdout(&out), "hi\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "hi\n"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn test_words_substitute() {
    let out = sharky("[[ `echo hi` == hi ]] && echo yes");
    assert_eq!(stdout(&out), "yes\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_status_is_the_substitution_status() {
    let out = sharky("v=`false`; echo $?; v=`exit 3`; echo $?");
    assert_eq!(stdout(&out), "1\n3\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn an_unquoted_result_is_split() {
    let out = sharky("printf '[%s]\\n' `printf 'a  b'`");
    assert_eq!(stdout(&out), "[a]\n[b]\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn a_quoted_result_is_not_split() {
    let out = sharky("printf '[%s]\\n' \"`printf 'a  b'`\"");
    assert_eq!(stdout(&out), "[a  b]\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn an_unquoted_result_is_globbed() {
    let dir = temp_dir();
    std::fs::write(dir.join("f1.txt"), "").unwrap();
    std::fs::write(dir.join("f2.txt"), "").unwrap();
    let out = sharky_in(&dir, "echo `echo '*.txt'`");
    assert_eq!(
        stdout(&out),
        "f1.txt f2.txt\n",
        "stderr: {:?}",
        stderr(&out)
    );
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_quoted_result_is_not_globbed() {
    let dir = temp_dir();
    std::fs::write(dir.join("f1.txt"), "").unwrap();
    let out = sharky_in(&dir, "echo \"`echo '*.txt'`\"");
    assert_eq!(stdout(&out), "*.txt\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

// The downstream pipeline is shared with `$(...)`, so quoted-ness, splitting and
// newline handling must agree for the same inner command.
#[test]
fn backticks_and_dollar_form_expand_identically() {
    let pairs = [
        ("echo `printf 'a  b'`", "echo $(printf 'a  b')"),
        (
            "printf '[%s]\\n' `printf 'a b'`",
            "printf '[%s]\\n' $(printf 'a b')",
        ),
        ("echo \"`echo hi`\"", "echo \"$(echo hi)\""),
        ("echo `echo one; echo two`", "echo $(echo one; echo two)"),
        (
            "echo \"`echo one; echo two`\"",
            "echo \"$(echo one; echo two)\"",
        ),
        (
            "v=`printf 'x\\ny\\n'`; printf '[%s]' \"$v\"",
            "v=$(printf 'x\\ny\\n'); printf '[%s]' \"$v\"",
        ),
        (
            "for x in `echo a b`; do printf '[%s]' \"$x\"; done; echo",
            "for x in $(echo a b); do printf '[%s]' \"$x\"; done; echo",
        ),
    ];
    for (backtick, dollar) in pairs {
        let left = sharky(backtick);
        let right = sharky(dollar);
        assert_eq!(
            (stdout(&left), status(&left)),
            (stdout(&right), status(&right)),
            "backticks {backtick:?} diverged from {dollar:?} (stderr {:?} / {:?})",
            stderr(&left),
            stderr(&right)
        );
        assert_eq!(status(&left), 0, "stderr: {:?}", stderr(&left));
    }
}

// A backslash before a double quote keeps that quote out of the scan-time quote
// state, and the pair then reaches the inner parser, which is what makes a
// literal quote inside a substitution possible.
#[test]
fn an_escaped_double_quote_reaches_the_inner_parser() {
    let out = sharky("echo `echo \\\"hi\\\"`");
    assert_eq!(stdout(&out), "\"hi\"\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Inside a double-quoted word bash scans the substitution with the
// double-quote escaping set, so `\"` loses its backslash and the inner command
// sees a real quote pair. Outside double quotes the backslash stays.
#[test]
fn an_escaped_double_quote_inside_double_quotes_is_a_real_quote() {
    let out = sharky("echo \"`echo \\\"quoted\\\"`\"");
    assert_eq!(stdout(&out), "quoted\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

#[test]
fn an_escaped_double_quote_outside_double_quotes_stays_escaped() {
    let out = sharky("echo `echo \\\"quoted\\\"`");
    assert_eq!(stdout(&out), "\"quoted\"\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
}

// Known divergence, pinned so it cannot regress into silent output. Bash lets a
// lone trailing backslash survive at end of input, so `` `echo \\` `` runs
// `echo \` and prints one backslash. Sharky's lexer reports a trailing backslash
// as incomplete, so the substitution child emits a diagnostic and the outer
// command keeps an empty result instead of a wrong word.
#[test]
fn a_trailing_backslash_in_the_inner_command_is_a_diagnostic() {
    let out = sharky("echo `echo \\\\`");
    assert_eq!(stdout(&out), "\n", "stderr: {:?}", stderr(&out));
    assert!(
        stderr(&out).contains("trailing backslash"),
        "expected a trailing-backslash diagnostic, got {:?}",
        stderr(&out)
    );
}
