use std::process::{Command, Output};

fn sharky(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn version_prints_name_and_version() {
    let out = sharky(&["--version"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "sharky 2.0.0");
}

#[test]
fn command_success_exits_zero_without_output() {
    let out = sharky(&["-c", "true"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
    assert!(out.stderr.is_empty());
}

#[test]
fn command_failure_propagates_exit_status() {
    let out = sharky(&["-c", "false"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn command_runs_external_program_with_arguments() {
    let out = sharky(&["-c", "printf hi"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
}

#[test]
fn double_quotes_keep_spaces_in_one_word() {
    let out = sharky(&["-c", r#"printf '%s\n' "a b""#]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n");
}

#[test]
fn single_quotes_keep_spaces_in_one_word() {
    let out = sharky(&["-c", r#"printf '%s\n' 'a b'"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n");
}

#[test]
fn backslash_escapes_a_space() {
    let out = sharky(&["-c", r"printf '%s\n' a\ b"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n");
}

#[test]
fn empty_quoted_word_survives_as_an_argument() {
    let out = sharky(&["-c", r#"printf '[%s]\n' """#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[]\n");
}

#[test]
fn comment_ends_the_command() {
    let out = sharky(&["-c", "printf '[%s]\\n' a # b"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a]\n");
}

#[test]
fn unclosed_quote_is_a_syntax_error() {
    let out = sharky(&["-c", r#"printf "x"#]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unclosed double quote"));
}

#[test]
fn unsupported_operator_fails_instead_of_looping() {
    let out = sharky(&["-c", r"printf '%s' $(printf x)"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unsupported syntax"));
}

fn sharky_with(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharky"));
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output().expect("failed to run sharky")
}

#[test]
fn semicolon_runs_commands_in_sequence() {
    let out = sharky(&["-c", "false; true"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn last_command_status_is_the_status() {
    let out = sharky(&["-c", "true; false"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn and_if_skips_after_failure() {
    let out = sharky(&["-c", "false && printf no"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
}

#[test]
fn and_if_runs_after_success() {
    let out = sharky(&["-c", "true && printf yes"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn or_if_runs_after_failure() {
    let out = sharky(&["-c", "false || printf yes"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn exit_builtin_stops_the_list() {
    let out = sharky(&["-c", "printf no; exit 3; printf no"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no");
}

#[test]
fn dollar_question_reports_last_status() {
    let out = sharky(&["-c", "false; printf '%s' $?"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}

#[test]
fn quoted_variable_expansion_keeps_spaces() {
    let out = sharky_with(&["-c", r#"printf '%s\n' "$X""#], &[("X", "a b")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n");
}

#[test]
fn unquoted_variable_expansion_splits_fields() {
    let out = sharky_with(&["-c", r"printf '%s\n' $X"], &[("X", "a b")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\nb\n");
}

#[test]
fn unset_variable_expands_to_nothing() {
    let out = sharky_with(&["-c", r#"sh -c 'printf "%s" "$#"' x $X"#], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");
}

#[test]
fn quoted_unset_variable_is_an_empty_argument() {
    let out = sharky_with(&["-c", r#"sh -c 'printf "%s" "$#"' x "$X""#], &[]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}

fn temp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sharky-cli-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_in(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn pipeline_connects_stdout_to_stdin() {
    let out = sharky(&["-c", r"printf 'a\nb\n' | wc -l"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");
}

#[test]
fn pipeline_status_is_the_last_command() {
    let out = sharky(&["-c", "printf x | cat"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn redirect_writes_a_file() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "printf hi > out; cat out"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn redirect_append_keeps_previous_content() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "printf a > out; printf b >> out; cat out"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ab");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn redirect_reads_a_file() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "printf hi > out; cat < out"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stderr_redirect_captures_diagnostics() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "sh -c 'printf e >&2' 2> err; cat err"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "e");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stdout_to_stderr_dup_merges_both_streams() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "sh -c 'printf o; printf e >&2' > f 2>&1; cat f"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "oe");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_redirect_target_is_a_syntax_error() {
    let out = sharky(&["-c", "printf hi >"]);
    assert_eq!(out.status.code(), Some(2));
}
