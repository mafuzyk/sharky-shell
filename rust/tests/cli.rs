use std::os::unix::fs::FileTypeExt;
use std::process::{Command, Output};

mod common;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn version_prints_name_and_version() {
    let out = sharky(&["--version"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("sharky {}", env!("CARGO_PKG_VERSION"))
    );
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
fn command_substitution_runs_and_captures() {
    let out = sharky(&["-c", r"printf '%s' $(printf hi)"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
}

fn sharky_with(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharky"));
    cmd.args(args).env("SHARKY_CONFIG", NO_CONFIG);
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
        .env("SHARKY_CONFIG", NO_CONFIG)
        .current_dir(dir)
        .output()
        .expect("failed to run sharky")
}

fn file_text(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|err| panic!("reading {}: {err}", path.display()))
}

fn sharky_in_with_stdin(dir: &std::path::Path, args: &[&str], input: &str) -> Output {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run sharky");
    child
        .stdin
        .take()
        .expect("sharky stdin")
        .write_all(input.as_bytes())
        .expect("failed to write sharky stdin");
    child.wait_with_output().expect("failed to collect output")
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

#[test]
fn assignment_sets_a_shell_variable() {
    let out = sharky(&["-c", "X=hi; printf '%s' $X"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
}

#[test]
fn assignment_value_keeps_quoted_spaces() {
    let out = sharky(&["-c", r#"X="a b"; printf '[%s]' "$X""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a b]");
}

#[test]
fn prefix_assignment_is_visible_to_the_command_only() {
    let out = sharky(&["-c", r#"X=1 sh -c 'printf %s "$X"'; printf '[%s]' "$X""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1[]");
}

#[test]
fn unset_removes_a_variable() {
    let out = sharky_with(&["-c", r#"unset X; printf '[%s]' "$X""#], &[("X", "1")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[]");
}

#[test]
fn export_keeps_a_variable_for_children() {
    let out = sharky(&["-c", r#"X=1; export X; sh -c 'printf %s "$X"'"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}

#[test]
fn if_runs_the_then_branch() {
    let out = sharky(&["-c", "if true; then printf yes; fi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn if_runs_the_else_branch() {
    let out = sharky(&["-c", "if false; then printf no; else printf yes; fi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn if_runs_the_first_true_elif() {
    let out = sharky(&["-c", "if false; then printf a; elif true; then printf b; fi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b");
}

#[test]
fn for_iterates_over_words() {
    let out = sharky(&["-c", r#"for x in a b; do printf %s "$x"; done"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ab");
}

#[test]
fn while_runs_the_body_until_false() {
    let out = sharky(&["-c", "while false; do printf no; done; printf ok"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
}

#[test]
fn until_stops_when_the_condition_succeeds() {
    let out = sharky(&["-c", "until true; do printf no; done; printf ok"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
}

#[test]
fn case_matches_a_glob_pattern() {
    let out = sharky(&["-c", "case foo in f*) printf matched;; *) printf no;; esac"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "matched");
}

#[test]
fn case_falls_through_to_the_default_arm() {
    let out = sharky(&["-c", "case bar in f*) printf no;; *) printf other;; esac"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "other");
}

#[test]
fn function_definition_and_call() {
    let out = sharky(&["-c", "f() { printf fn; }; f"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fn");
}

#[test]
fn function_keyword_form() {
    let out = sharky(&["-c", "function g { printf g; }; g"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "g");
}

#[test]
fn function_sees_positional_parameters() {
    let out = sharky(&["-c", r#"f() { printf '%s|%s' "$1" "$#"; }; f hi there"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi|2");
}

#[test]
fn return_sets_the_function_status() {
    let out = sharky(&["-c", "f() { return 7; }; f; printf '%s' $?"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "7");
}

#[test]
fn brace_group_runs_in_the_current_shell() {
    let out = sharky(&["-c", "{ printf a; printf b; }"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ab");
}

#[test]
fn subshell_runs_its_list() {
    let out = sharky(&["-c", "( printf a; printf b )"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ab");
}

#[test]
fn not_inverts_the_status() {
    let out = sharky(&["-c", "! false"]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn break_leaves_the_loop() {
    let out = sharky(&["-c", "for x in 1 2; do break; printf no; done; printf ok"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
}

#[test]
fn continue_skips_the_rest_of_the_body() {
    let out = sharky(&["-c", "for x in a b; do continue; printf no; done; printf ok"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
}

#[test]
fn function_variables_do_not_leak_after_return() {
    let out = sharky(&["-c", r#"f() { printf '%s' "$1"; }; f a; printf '[%s]' "$1""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a[]");
}

fn run_with_config(config: &str, command: &str) -> Output {
    let dir = temp_dir();
    std::fs::write(dir.join("config.sharky"), config).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", command])
        .env("SHARKY_CONFIG", &dir)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    out
}

#[test]
fn wake_does_not_run_in_a_non_interactive_shell() {
    let out = run_with_config("wake \"printf WAKE\"\n", "printf cmd");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "cmd");
}

#[test]
fn config_sets_environment_and_aliases() {
    let out = run_with_config(
        "pearl GREETING hello\nreef hi \"printf %s world\"\n",
        r#"printf '%s' "$GREETING"; hi"#,
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "helloworld");
}

#[test]
fn config_current_prepends_to_path() {
    let out = run_with_config(
        "current \"/opt/sharky-test-bin\"\n",
        r#"printf '%s' "$PATH""#,
    );
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("/opt/sharky-test-bin:"));
}

#[test]
fn no_config_skips_the_file() {
    let dir = temp_dir();
    std::fs::write(dir.join("config.sharky"), "pearl GREETING hello\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", r#"printf '[%s]' "$GREETING""#])
        .env("SHARKY_CONFIG", &dir)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[]");
}

#[test]
fn config_error_is_reported_and_the_shell_continues() {
    let out = run_with_config("tidee greeting on\n", "printf ok");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("config error"));
    assert!(stderr.contains("unknown instruction 'tidee'"));
}

#[test]
fn theme_defines_colours_and_prompt_styles() {
    let dir = temp_dir();
    std::fs::write(
        dir.join("theme.sharky"),
        "coral kelp #8CCFA8\nwave prompt.cwd @kelp\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "true"])
        .env("SHARKY_CONFIG", &dir)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.stderr.is_empty());
}

#[test]
fn theme_with_undefined_colour_fails_loudly() {
    let dir = temp_dir();
    std::fs::write(dir.join("theme.sharky"), "wave prompt.cwd @kelp\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "true"])
        .env("SHARKY_CONFIG", &dir)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert!(String::from_utf8_lossy(&out.stderr).contains("undefined colour"));
}

#[test]
fn command_substitution_strips_trailing_newlines() {
    let out = sharky(&["-c", r#"X=$(printf 'a\nb\n\n'); printf '[%s]' "$X""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a\nb]");
}

#[test]
fn command_substitution_nests() {
    let out = sharky(&["-c", r"printf '%s' $(printf '%s' $(printf x))"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "x");
}

#[test]
fn command_substitution_splits_when_unquoted() {
    let out = sharky(&["-c", r"printf '[%s]\n' $(printf 'a b')"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[a]\n[b]\n");
}

#[test]
fn arithmetic_respects_precedence() {
    let out = sharky(&["-c", "printf '%s' $((2 + 3 * 4))"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "14");
}

#[test]
fn arithmetic_reads_variables() {
    let out = sharky(&["-c", "i=2; printf '%s' $((i * 3))"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "6");
}

#[test]
fn arithmetic_can_drive_a_loop() {
    let out = sharky(&[
        "-c",
        "i=0; while [ $i -lt 3 ]; do printf '%s' $i; i=$((i+1)); done",
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "012");
}

#[test]
fn parameter_default_uses_fallback_when_unset() {
    let out = sharky(&["-c", r#"printf '%s' "${X:-fallback}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fallback");
}

#[test]
fn parameter_default_keeps_a_set_value() {
    let out = sharky(&["-c", r#"X=v; printf '%s' "${X:-fallback}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "v");
}

#[test]
fn parameter_length_counts_characters() {
    let out = sharky(&["-c", r#"X=abc; printf '%s' "${#X}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3");
}

#[test]
fn parameter_trims_the_shortest_prefix() {
    let out = sharky(&["-c", r#"X=prefix.txt; printf '%s' "${X#*.}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "txt");
}

#[test]
fn parameter_trims_the_longest_suffix() {
    let out = sharky(&["-c", r#"X=a.tar.gz; printf '%s' "${X%%.*}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a");
}

#[test]
fn tilde_expands_to_home() {
    let out = sharky_with(&["-c", "printf '%s' ~/x"], &[("HOME", "/tmp/h")]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/tmp/h/x");
}

#[test]
fn glob_expands_matching_files() {
    let dir = temp_dir();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();
    let out = sharky_in(&dir, &["-c", r"printf '%s\n' *.txt"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a.txt\nb.txt\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn glob_keeps_the_pattern_when_nothing_matches() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", r"printf '%s' *.nope"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "*.nope");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn quoted_glob_is_not_expanded() {
    let dir = temp_dir();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    let out = sharky_in(&dir, &["-c", r#"printf '%s' "*.txt""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "*.txt");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn runs_a_script_file_with_positional_arguments() {
    let dir = temp_dir();
    let script = dir.join("hello.sh");
    std::fs::write(&script, "printf '%s|%s' \"$1\" \"$#\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&script)
        .arg("hi")
        .arg("there")
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi|2");
}

#[test]
fn script_exit_status_propagates() {
    let dir = temp_dir();
    let script = dir.join("fail.sh");
    std::fs::write(&script, "printf a\nexit 4\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&script)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a");
}

#[test]
fn source_runs_in_the_current_shell() {
    let dir = temp_dir();
    std::fs::write(dir.join("lib.sh"), "X=fromlib\n").unwrap();
    let out = sharky_in(&dir, &["-c", r#". ./lib.sh; printf '%s' "$X""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fromlib");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn source_passes_positional_arguments() {
    let dir = temp_dir();
    std::fs::write(dir.join("lib.sh"), "printf '%s' \"$1\"\n").unwrap();
    let out = sharky_in(&dir, &["-c", r#"source ./lib.sh hi"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reads_commands_from_stdin_when_not_a_tty() {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn sharky");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"printf '%s' piped\n")
        .unwrap();

    let out = child.wait_with_output().expect("wait");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "piped");
}

#[test]
fn pipeline_read_loop_ends_at_eof() {
    let out = run_bounded("printf 'a\\nb\\n' | while read line; do printf '%s\\n' \"$line\"; done");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a\nb\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

// Runs `-c command` in its own process group and waits at most five seconds:
// the pipeline under test blocks forever when a write end leaks, and the
// bounded wait kills the whole group instead of hanging the suite or leaking
// the loop child.
fn run_bounded(command: &str) -> Output {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .env("SHARKY_CONFIG", NO_CONFIG)
        .args(["-c", command])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .expect("spawn sharky");

    let group = nix::unistd::Pid::from_raw(child.id() as i32);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("wait for sharky") {
            let out = child.wait_with_output().expect("collect output");
            assert!(status.success(), "status: {status:?}");
            return out;
        }
        if Instant::now() >= deadline {
            let _ = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL);
            let out = child.wait_with_output().expect("collect output");
            panic!(
                "shell did not end at EOF: stdout={:?} stderr={:?}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn double_bracket_tests_strings() {
    assert_eq!(sharky(&["-c", "[[ -n x ]]"]).status.code(), Some(0));
    assert_eq!(sharky(&["-c", "[[ -z '' ]]"]).status.code(), Some(0));
}

#[test]
fn double_bracket_matches_glob_patterns() {
    assert_eq!(sharky(&["-c", "[[ foo == f* ]]"]).status.code(), Some(0));
    assert_eq!(sharky(&["-c", "[[ foo != f* ]]"]).status.code(), Some(1));
}

#[test]
fn double_bracket_compares_integers() {
    assert_eq!(sharky(&["-c", "[[ 3 -gt 2 ]]"]).status.code(), Some(0));
    assert_eq!(sharky(&["-c", "[[ 3 -lt 2 ]]"]).status.code(), Some(1));
}

#[test]
fn double_bracket_keeps_quoted_spaces() {
    assert_eq!(
        sharky(&["-c", r#"X="a b"; [[ "$X" == "a b" ]]"#])
            .status
            .code(),
        Some(0)
    );
}

#[test]
fn double_bracket_supports_file_tests_and_negation() {
    let dir = temp_dir();
    let file = dir.join("thing");
    std::fs::write(&file, "x").unwrap();
    let command = format!("[[ -f '{}' ]] && ! [[ -d '{}' ]]", file.display(), file.display());
    let out = sharky_in(&dir, &["-c", &command]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn double_bracket_combines_conditions_with_precedence() {
    let out = sharky(&["-c", r#"[[ -n x || -n "" ]] && [[ -z "" ]]"#]);
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn heredoc_feeds_stdin() {
    let out = sharky(&["-c", "cat <<EOF\nhello\nworld\nEOF"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\nworld\n");
}

#[test]
fn heredoc_expands_variables_by_default() {
    let out = sharky(&["-c", "X=v; cat <<EOF\nx=$X\nEOF"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "x=v\n");
}

#[test]
fn quoted_heredoc_delimiter_keeps_text_literal() {
    let out = sharky(&["-c", "X=v; cat <<'EOF'\n$X\nEOF"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "$X\n");
}

#[test]
fn heredoc_strip_removes_leading_tabs() {
    let out = sharky(&["-c", "cat <<-EOF\n\tline\n\t\tEOF"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "line\n");
}

#[test]
fn heredoc_works_inside_a_pipeline() {
    let out = sharky(&["-c", "cat <<EOF | wc -l\none\ntwo\nEOF"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "2");
}

#[test]
fn arithmetic_command_status_follows_the_value() {
    assert_eq!(sharky(&["-c", "((1 + 1))"]).status.code(), Some(0));
    assert_eq!(sharky(&["-c", "((0))"]).status.code(), Some(1));
}

#[test]
fn arithmetic_command_works_in_a_condition() {
    let out = sharky(&["-c", "if ((2 > 1)); then printf yes; fi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn background_command_continues_on_the_same_line() {
    assert_eq!(sharky(&["-c", "true & false"]).status.code(), Some(1));
    assert_eq!(sharky(&["-c", "false & true"]).status.code(), Some(0));
}

#[test]
fn background_command_continues_inside_a_case_body() {
    let out = sharky(&["-c", "case x in x) true & false;; esac"]);
    assert_eq!(out.status.code(), Some(1));
}


#[test]
fn for_without_a_variable_is_a_syntax_error() {
    let out = sharky(&["-c", "for ; do :; done"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("syntax error"));
}


#[test]
fn cd_without_arguments_uses_home() {
    let dir = temp_dir();
    let home = dir.join("home");
    std::fs::create_dir(&home).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", "cd; printf '%s' \"$PWD\""])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", &home)
        .current_dir(&dir)
        .output()
        .expect("failed to run sharky");
    let expected = home.canonicalize().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected.display().to_string());
}

#[test]
fn startup_sets_pwd_to_the_working_directory() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "printf '%s' \"$PWD\""]);
    let expected = dir.canonicalize().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(String::from_utf8_lossy(&out.stdout), expected.display().to_string());
}

#[test]
fn cd_updates_pwd_and_oldpwd() {
    let dir = temp_dir();
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = sharky_in(&dir, &["-c", "cd sub; printf '%s|%s' \"$PWD\" \"$OLDPWD\""]);
    let sub = sub.canonicalize().unwrap();
    let parent = dir.canonicalize().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}|{}", sub.display(), parent.display())
    );
}

#[test]
fn cd_dash_returns_to_the_previous_directory() {
    let dir = temp_dir();
    let sub = dir.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let out = sharky_in(&dir, &["-c", "cd sub; cd -; printf '%s' \"$PWD\""]);
    let parent = dir.canonicalize().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), parent.display().to_string());
}

#[test]
fn last_background_pid_is_available() {
    let out = sharky(&["-c", "sleep 0.05 & printf '%s' \"$!\""]);
    let text = String::from_utf8_lossy(&out.stdout);
    let pid = text.lines().last().unwrap_or_default();
    assert!(
        !pid.is_empty() && pid.chars().all(|char| char.is_ascii_digit()),
        "unexpected pid in {text:?}"
    );
}

#[test]
fn background_jobs_are_tracked_in_scripts() {
    let dir = temp_dir();
    let script = dir.join("jobs.sh");
    std::fs::write(&script, "sleep 0.05 &\njobs\n").unwrap();
    let out = sharky_in(&dir, &["--no-config", script.to_str().unwrap()]);
    let text = String::from_utf8_lossy(&out.stdout);
    std::fs::remove_dir_all(&dir).ok();
    assert!(text.contains("[1]"), "unexpected jobs output {text:?}");
    assert!(text.contains("sleep 0.05"), "unexpected jobs output {text:?}");
}

#[test]
fn here_string_feeds_one_line_to_stdin() {
    let out = sharky(&["-c", "cat <<< \"a b\""]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b\n");
}

#[test]
fn here_string_expands_variables() {
    let out = sharky(&["-c", "X=v; cat <<< \"$X\""]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "v\n");
}

#[test]
fn combined_redirect_captures_stdout_and_stderr() {
    let dir = temp_dir();
    let path = dir.join("both");
    let command = format!(
        "sh -c 'echo o; echo e >&2' &> {}; cat {}",
        path.display(),
        path.display()
    );
    let out = sharky_in(&dir, &["-c", &command]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "o\ne\n");
}

#[test]
fn combined_append_redirect_keeps_the_file() {
    let dir = temp_dir();
    let path = dir.join("both");
    let command = format!(
        "printf a &> {}; printf b &>> {}; cat {}",
        path.display(),
        path.display(),
        path.display()
    );
    let out = sharky_in(&dir, &["-c", &command]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ab");
}

#[test]
fn brace_expansion_expands_lists_and_ranges() {
    let out = sharky(&["-c", "printf '%s ' a{b,c}d"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "abd acd ");
    let out = sharky(&["-c", "printf '%s ' {1..3}"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1 2 3 ");
    let out = sharky(&["-c", "printf '%s ' {c..a}"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "c b a ");
}

#[test]
fn parameter_substitution_replaces_literal_text() {
    let out = sharky(&["-c", "x=abcabc; printf '%s' \"${x/b/X}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "aXcabc");
    let out = sharky(&["-c", "x=abcabc; printf '%s' \"${x//b/X}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "aXcaXc");
}

#[test]
fn array_assignment_expands_elements() {
    let out = sharky(&["-c", r#"a=(one two); printf '[%s]' "${a[@]}""#]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[one][two]");
}

#[test]
fn array_unquoted_expansion_splits_fields() {
    let out = sharky(&["-c", r#"a=(one two); printf '[%s]' ${a[@]}"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[one][two]");
}

#[test]
fn array_index_and_length() {
    let out = sharky(&["-c", r#"a=(one two); printf '%s|%s' "${a[1]}" "${#a[@]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "two|2");
}

#[test]
fn array_append_and_element_assignment() {
    let out = sharky(&[
        "-c",
        r#"a=(one two); a+=(three); a[0]=zero; printf '%s|' ${a[@]}"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "zero|two|three|");
}

#[test]
fn sparse_array_assignment_counts_only_defined_elements() {
    let out = sharky(&[
        "-c",
        r#"a=(one); a[3]=x; printf '%s|%s' "${#a[@]}" "${a[2]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2|");
}

#[test]
fn empty_array_has_zero_length() {
    let out = sharky(&["-c", r#"a=(); printf '%s' "${#a[@]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");
}

#[test]
fn set_e_exits_on_the_first_failure() {
    let out = sharky(&["-c", "set -e; false; printf no"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn set_e_is_suppressed_in_and_or_lists() {
    let out = sharky(&["-c", "set -e; false && true; printf yes"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
    let out = sharky(&["-c", "set -e; true && false; printf no"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn set_e_is_suppressed_in_conditions_and_negation() {
    let out = sharky(&["-c", "set -e; if false; then :; fi; printf yes"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
    let out = sharky(&["-c", "set -e; ! true; printf yes"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
}

#[test]
fn pipefail_returns_the_rightmost_failure() {
    let out = sharky(&["-c", "set -o pipefail; false | true; printf '%s' \"$?\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}

#[test]
fn set_e_with_pipefail_exits_on_a_failed_pipeline() {
    let out = sharky(&["-c", "set -e -o pipefail; false | true; printf no"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn set_dash_dash_sets_positional_parameters() {
    let out = sharky(&["-c", r#"set -- a b; printf '%s|%s|%s' "$1" "$2" "$#""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a|b|2");
}

#[test]
fn set_without_arguments_reports_active_options() {
    let out = sharky(&["-c", "set -e; set"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("errexit"));
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn set_x_traces_to_stderr() {
    let out = sharky(&["-c", "set -x; printf hi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    assert!(String::from_utf8_lossy(&out.stderr).contains("+ printf hi"));
}

#[test]
fn trap_runs_the_exit_action() {
    let out = sharky(&["-c", "trap 'printf bye' EXIT; printf hi"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hibye");
}

#[test]
fn trap_lists_and_clears_signal_actions() {
    let out = sharky(&["-c", "trap -l"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("EXIT HUP"));
    let out = sharky(&["-c", "trap 'printf bye' EXIT; trap - EXIT; printf hi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
}

#[test]
fn read_splits_fields_and_gives_the_rest_to_the_last_name() {
    let out = sharky(&["-c", r#"printf 'a b c\n' | { read x y; printf '%s|%s' "$x" "$y"; }"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a|b c");
}

#[test]
fn read_r_keeps_backslashes_literal() {
    let out = sharky(&["-c", r#"printf 'a\\b\n' | { read -r x; printf '%s' "$x"; }"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), r"a\b");
}

#[test]
fn read_honors_escaped_spaces() {
    let out = sharky(&["-c", r#"printf 'a\\ b\n' | { read x; printf '%s' "$x"; }"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "a b");
}

#[test]
fn shift_moves_positional_parameters() {
    let out = sharky(&["-c", r#"f() { shift; printf '%s' "$1"; }; f a b"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "b");
    let out = sharky(&["-c", r#"f() { shift 2; printf '%s' "$1"; }; f a b c"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "c");
}

#[test]
fn nounset_stops_expansion_of_an_unset_variable() {
    let out = sharky(&["-c", r#"set -u; printf '%s' "$UNSET_X"; printf after"#]);
    assert_eq!(out.status.code(), Some(127));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound variable"));
}

#[test]
fn error_expansion_reports_the_command_string_status() {
    let out = sharky(&["-c", r#"unset v; printf '[%s]' "${v?boom}"; printf after"#]);
    assert_eq!(out.status.code(), Some(127));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn nounset_allows_default_operators() {
    let out = sharky(&["-c", r#"set -u; printf '%s' "${UNSET_X:-fallback}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fallback");
}

#[test]
fn set_e_propagates_out_of_functions() {
    let out = sharky(&["-c", "set -e; f() { false; printf no; }; f; printf no2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
}

#[test]
fn type_reports_aliases_builtins_and_paths() {
    let out = sharky(&["-c", "type cd; type printf"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("cd: shell builtin"), "{text:?}");
    assert!(text.contains("printf: /"), "{text:?}");
    let out = sharky(&["-c", "type definitely-not-a-command"]);
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn alias_declares_and_removes_an_alias() {
    let out = sharky(&["-c", r#"alias hi='echo hello'; hi"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    let out = sharky(&["-c", r#"alias hi='echo hello'; unalias hi; hi"#]);
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn pwd_prints_the_tracked_directory() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "pwd"]);
    let expected = dir.canonicalize().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), expected.display().to_string());
}

#[test]
fn history_is_unavailable_without_an_interactive_session() {
    let out = sharky(&["-c", "history"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not available"));
}

#[test]
fn double_bracket_supports_regex_matching() {
    assert_eq!(sharky(&["-c", "[[ foo =~ ^f ]]"]).status.code(), Some(0));
    assert_eq!(sharky(&["-c", "[[ foo =~ ^b ]]"]).status.code(), Some(1));
    assert_eq!(sharky(&["-c", "[[ foo =~ ( ]]"]).status.code(), Some(1));
}

#[test]
fn double_bracket_supports_symlinks_and_exists() {
    let dir = temp_dir();
    let file = dir.join("file");
    let link = dir.join("link");
    std::fs::write(&file, "x").unwrap();
    std::os::unix::fs::symlink(&file, &link).unwrap();
    let command = format!("[[ -a '{}' ]] && [[ -L '{}' ]]", file.display(), link.display());
    let out = sharky_in(&dir, &["-c", &command]);
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn double_bracket_coerces_integers_like_ruby() {
    assert_eq!(sharky(&["-c", "[[ 1 -eq x ]]"]).status.code(), Some(1));
    assert_eq!(sharky(&["-c", "[[ 12abc -eq 12 ]]"]).status.code(), Some(0));
}

#[test]
fn wait_returns_the_job_status() {
    let out = sharky(&["-c", r#"sleep 0.05 & wait; printf 'st=%s' "$?""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().last(), Some("st=0"));
    let out = sharky(&["-c", r#"sleep 5 & kill %1; wait %1; printf 'st=%s' "$?""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().last(), Some("st=143"));
}

#[test]
fn kill_accepts_signals_and_job_specs() {
    let out = sharky(&["-c", r#"sleep 5 & kill -9 %1; wait %1; printf 'st=%s' "$?""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout).lines().last(), Some("st=137"));
}

#[test]
fn wait_and_kill_report_unknown_jobs() {
    assert_eq!(sharky(&["-c", "kill %99"]).status.code(), Some(1));
    assert_eq!(sharky(&["-c", "wait %99"]).status.code(), Some(127));
}

#[test]
fn first_run_seeds_the_config_directory() {
    let dir = temp_dir();
    let config_home = dir.join("config");
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "true"])
        .env_remove("SHARKY_CONFIG")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &dir)
        .output()
        .expect("failed to run sharky");
    assert_eq!(out.status.code(), Some(0));

    let config = config_home.join("sharky/config.sharky");
    let theme = config_home.join("sharky/theme.sharky");
    let text = std::fs::read_to_string(&config).expect("seeded config");
    assert!(text.contains("// Sharky"), "config={text:?}");
    assert!(std::fs::read_to_string(&theme).is_ok(), "theme not seeded");

    std::fs::write(&config, "tide greeting off\n").unwrap();
    let _ = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "true"])
        .env_remove("SHARKY_CONFIG")
        .env("XDG_CONFIG_HOME", &config_home)
        .env("HOME", &dir)
        .output();
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(text, "tide greeting off\n");
}

#[test]
fn discovers_path_additions_from_shell_profiles() {
    use std::os::unix::fs::PermissionsExt;

    let dir = temp_dir();
    let bin = dir.join(".spicetify");
    std::fs::create_dir_all(&bin).unwrap();
    let tool = bin.join("spicetify");
    let sh = common::resolve("sh");
    std::fs::write(&tool, format!("#!{}\nprintf 'spicetify-test'\n", sh.display())).unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        dir.join(".bashrc"),
        "export PATH=\"$HOME/.spicetify:$PATH\"\n",
    )
    .unwrap();

    let cfg = dir.join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    let run = || {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["-c", "spicetify"])
            .env("HOME", &dir)
            .env("PATH", "/usr/bin:/bin")
            .env("SHARKY_CONFIG", &cfg)
            .output()
            .expect("failed to run sharky")
    };

    let out = run();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "spicetify-test");

    std::fs::write(cfg.join("config.sharky"), "tide profile_paths off\n").unwrap();
    let out = run();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn abbr_declares_lists_and_removes_abbreviations() {
    let out = sharky(&["-c", "abbr gco 'git checkout'; abbr"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "abbr gco='git checkout'\n");

    let out = sharky(&["-c", "abbr gco 'git checkout'; abbr gco"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "abbr gco='git checkout'\n");

    let out = sharky(&["-c", "abbr c=d; abbr c"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "abbr c='d'\n");

    let out = sharky(&["-c", "abbr gco 'git checkout'; unabbr gco; abbr gco"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));

    let out = sharky(&["-c", "abbr a b; unabbr -a; abbr"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");

    assert_eq!(sharky(&["-c", "unabbr"]).status.code(), Some(1));
}

#[test]
fn non_interactive_runs_emit_no_terminal_marks() {
    let out = sharky(&["-c", "printf hi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("\x1b]"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("\x1b]"));
}

#[test]
fn parameter_substrings_use_offsets_and_lengths() {
    let cases = [
        ("x=abcdef; printf '%s' \"${x:2}\"", "cdef"),
        ("x=abcdef; printf '%s' \"${x:1:3}\"", "bcd"),
        ("x=abcdef; printf '%s' \"${x: -2}\"", "ef"),
        ("x=abcdef; printf '%s' \"${x:1:-1}\"", "bcde"),
        ("set -- a b c; printf '<%s>' \"${@:2}\"", "<b><c>"),
        ("set -- a b c; printf '<%s>' \"${@:2:1}\"", "<b>"),
        ("set -- a b c; printf '<%s>' \"${*:2}\"", "<b c>"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn parameter_substrings_clamp_and_evaluate_offsets() {
    let cases = [
        ("x=abcdef; n=2; printf '%s' \"${x:n:1+1}\"", "cd"),
        ("x=abcdef; printf '%s' \"${x:1:2+1}\"", "bcd"),
        ("x=abc; printf '<%s>' \"${x: -4}\"", "<>"),
        ("x=abc; printf '%s' \"${x:1:9223372036854775807}\"", "bc"),
        ("a=(one two); printf '<%s>' \"${a[@]: -3}\"", "<>"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }

    let out = sharky(&["-c", r#"x=abc; printf '<%s>' "${x:1/0}"; printf AFTER"#]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("division by zero"));
}

#[test]
fn expansion_error_does_not_apply_redirections() {
    let dir = temp_dir();
    std::fs::write(dir.join("out"), "KEEP").unwrap();
    let out = sharky_in(
        &dir,
        &["-c", r#"printf BAD > "out$((1/0))"; printf AFTER"#],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("out")).unwrap(),
        "KEEP"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn malformed_arithmetic_for_is_a_status_two_error() {
    let out = sharky(&["-c", "for ((i=0)); do printf BAD; break; done"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn arithmetic_short_circuits_without_side_effects() {
    let out = sharky(&["-c", "i=0; ((0 && i++)); printf '%s' \"$i\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");

    let out = sharky(&["-c", "i=0; ((1 || i++)); printf '%s' \"$i\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");

    let out = sharky(&["-c", "i=0; ((1 && i++)); printf '%s' \"$i\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
}

#[test]
fn set_dash_o_without_a_name_reports_the_mode() {
    let out = sharky(&["-c", "set -o vi; set -o"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("vi"), "{out:?}");

    let out = sharky(&["-c", "set +o vi; set -o"]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("vi"), "{out:?}");
}

#[test]
fn script_arguments_are_not_parsed_as_shell_options() {
    let dir = std::env::temp_dir().join(format!("sharky-script-args-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("args.sh");
    std::fs::write(&script, "printf '<%s>' \"$1\"\n").unwrap();
    let out = sharky(&[script.to_str().unwrap(), "--json"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<--json>");
    assert_eq!(out.status.code(), Some(0));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn time_accepts_a_negated_pipeline() {
    let out = sharky(&["-c", "time ! false | cat; printf '%s' \"$?\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("syntax error"));
}

#[test]
fn parameter_case_operators_change_the_value() {
    let cases = [
        ("x=hello; printf '%s' \"${x^}\"", "Hello"),
        ("x=hello; printf '%s' \"${x^^}\"", "HELLO"),
        ("x=Hello; printf '%s' \"${x,}\"", "hello"),
        ("x=HeLLo; printf '%s' \"${x,,}\"", "hello"),
        ("x=1abc; printf '%s' \"${x^}\"", "1abc"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn parameter_indirection_and_name_prefixes() {
    let out = sharky(&["-c", "name=x; x=value; printf '%s' \"${!name}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "value");

    let out = sharky(&[
        "-c",
        "prefix_b=2; prefix_a=1; printf '<%s>' \"${!prefix_@}\"",
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<prefix_a><prefix_b>");

    let out = sharky(&["-c", "a=(x y z); printf '<%s>' \"${!a[@]}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<0><1><2>");
}

#[test]
fn ansi_c_nul_truncates_the_string() {
    let out = sharky(&["-c", r#"printf '<%s>' $'abc\0def'"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<abc>");

    let out = sharky(&[
        "-c",
        r#"x=$'abc\0def'; printf '%s|%s' "${#x}" "$x""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3|abc");

    let out = sharky(&["-c", r#"printf '%s' $'abc\0\'def'"#]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "abc");
}

#[test]
fn ansi_c_quoting_decodes_escapes() {
    let cases = [
        ("printf '%s' $'a\\tb'", "a\tb"),
        ("printf '<%s>' $'a b'", "<a b>"),
        ("printf '%s' $'it\\'s'", "it's"),
        ("printf '%s' $'\\x41\\u00e9'", "Aé"),
        ("printf '%s' $'\\101'", "A"),
        ("printf '<%s>' \"$'a b'\"", "<$'a b'>"),
        ("cat <<EOF\n$'x'\nEOF\n", "$'x'\n"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}


#[test]
fn associative_arrays_store_and_read_keys() {
    let out = sharky(&[
        "-c",
        r#"declare -A a; a[one]=1; a[two]=2; printf '%s|%s' "${a[one]}" "${#a[@]}"; printf '<%s>' "${!a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1|2<one><two>");
}

#[test]
fn associative_array_literals_fill_declared_arrays() {
    let out = sharky(&[
        "-c",
        r#"declare -A a; a=([x]=1 [y]=2); printf '%s|%s' "${a[x]}" "${a[y]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1|2");
}

#[test]
fn declare_accepts_inline_array_literals() {
    let out = sharky(&[
        "-c",
        r#"declare -A a=([x]=1 [y]=2); printf '%s|%s' "${a[x]}" "${a[y]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1|2");

    let out = sharky(&[
        "-c",
        r#"declare -a a=(one two); printf '%s|%s' "${a[0]}" "${a[1]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "one|two");
}

#[test]
fn quoted_star_joins_elements_into_one_field() {
    let out = sharky(&["-c", r#"a=(one two); printf '<%s>' "${a[*]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<one two>");

    let out = sharky(&["-c", r#"a=(one two three); printf '<%s>' "${a[*]:1}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<two three>");

    let out = sharky(&[
        "-c",
        r#"declare -A a; a[b]=2; a[a]=1; printf '<%s>' "${a[*]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<1 2>");
}

#[test]
fn associative_arrays_accept_quoted_and_expanded_keys() {
    let cases = [
        (
            r#"declare -A a; a["two words"]="v"; printf '<%s>' "${a["two words"]}""#,
            "<v>",
        ),
        (
            r#"k="two words"; declare -A a; a[$k]=1; printf '<%s>' "${a[$k]}""#,
            "<1>",
        ),
        (
            r#"declare -A a=(["two words"]="value"); printf '<%s>' "${a["two words"]}""#,
            "<value>",
        ),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn array_fields_keep_prefixes_and_suffixes() {
    let cases = [
        (
            r#"a=(one "two three" ""); printf '<%s>' pre"${a[@]}"post"#,
            "<preone><two three><post>",
        ),
        (
            r#"a=(one "two three" ""); printf '<%s>' "pre${a[@]}post""#,
            "<preone><two three><post>",
        ),
        (
            r#"a=(one two); printf '<%s>' "${a[@]^^}""#,
            "<ONE><TWO>",
        ),
        (
            r#"a=(one two); printf '<%s>' pre"${a[*]}"post"#,
            "<preone twopost>",
        ),
        (
            r#"set -- a b; printf '<%s>' pre"$@"post"#,
            "<prea><bpost>",
        ),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn sparse_array_slices_follow_indexes() {
    let cases = [
        (r#"a[2]=two; a[5]=five; printf '<%s>' "${a[@]:2:1}""#, "<two>"),
        (r#"a[2]=two; a[5]=five; printf '<%s>' "${a[@]:0:1}""#, "<two>"),
        (r#"a[0]=zero; a[2]=two; a[5]=five; printf '<%s>' "${a[@]:2:2}""#, "<two><five>"),
        (r#"a[0]=zero; a[2]=two; a[5]=five; printf '<%s>' "${a[@]:3:1}""#, "<five>"),
        (r#"a[0]=zero; a[2]=two; a[5]=five; printf '<%s>' "${a[@]: -1}""#, "<five>"),
        (r#"a[0]=zero; a[2]=two; a[5]=five; printf '<%s>' "${a[@]: -6}""#, "<zero><two><five>"),
        (r#"a=(one two); printf '<%s>' "${a[@]: -3}""#, "<>"),
        (
            r#"a[2]=two; a[5]=five; unset "a[5]"; printf '<%s>' "${a[@]: -1}""#,
            "<two>",
        ),
        (r#"a=("" x); printf '<%s>' pre${a[@]}post"#, "<pre><xpost>"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn unset_accepts_quoted_associative_subscripts() {
    let out = sharky(&[
        "-c",
        r#"declare -A a; a["two words"]=v; unset 'a["two words"]'; printf '%s' "${#a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");

    let out = sharky(&[
        "-c",
        r#"declare -A a; a["two words"]=v; k="two words"; unset "a[$k]"; printf '%s' "${#a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");
}

#[test]
fn unset_leaves_a_hole_in_indexed_arrays() {
    let out = sharky(&[
        "-c",
        r#"a=(one two three); unset 'a[1]'; printf '<%s>|<%s>' "${a[1]}" "${a[2]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<>|<three>");

    let out = sharky(&[
        "-c",
        r#"a=(one two three); unset 'a[1]'; printf '<%s>' "${a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<one><three>");

    let out = sharky(&[
        "-c",
        r#"a=(one two three); unset 'a[1]'; printf '<%s>' "${!a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<0><2>");
}

#[test]
fn empty_quoted_arguments_keep_their_field() {
    let cases = [
        (r#"set -- ""; printf '%s' "$#""#, "1"),
        (r#"set -- "" a ""; printf '<%s>' pre"$@"post"#, "<pre><a><post>"),
        (r#"set -- ""; printf '<%s>' pre"$@"post"#, "<prepost>"),
        (r#"set --; printf '<%s>' pre"$@"post"#, "<prepost>"),
        (r#"a=(); set -- "${a[*]}"; printf '%s' "$#""#, "1"),
        (r#"a=("" x ""); printf '<%s>' ${a[@]}"#, "<x>"),
        (r#"a=(); printf '<%s>' "${a[*]}""#, "<>"),
        (r#"a=(); printf '<%s>' "${a[@]}""#, "<>"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }

    let out = sharky(&["-c", r#"f() { printf '%s' "$#"; }; set --; f "$@""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");

    let out = sharky(&["-c", r#"x=; y=value; printf '<%s>|<%s>' "$x" "$y""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<>|<value>");
}

#[test]
fn set_e_ignores_a_negated_pipeline_status() {
    let out = sharky(&["-c", "set -e; ! false; printf ALIVE"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ALIVE");
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "set -e; ! true; printf ALIVE"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ALIVE");
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "set -e; false; printf AFTER"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "{out:?}");
}

#[test]
fn key_and_name_expansions_keep_one_field_per_item() {
    let out = sharky(&[
        "-c",
        r#"declare -A a; a[b]=2; a["two words"]=1; for k in "${!a[@]}"; do printf '<%s>' "$k"; done"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<b><two words>");

    let out = sharky(&["-c", r#"prefix_a=1; prefix_b=2; printf '<%s>' "${!prefix_@}""#]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<prefix_a><prefix_b>"
    );

    let out = sharky(&["-c", r#"prefix_a=1; prefix_b=2; printf '<%s>' "${!prefix_*}""#]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<prefix_a prefix_b>"
    );
}

#[test]
fn quoted_array_expansion_keeps_one_field_per_element() {
    let out = sharky(&["-c", r#"a=(one two three); printf '<%s>' "${a[@]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<one><two><three>");

    let out = sharky(&["-c", r#"a=(one two three); printf '<%s>' "${a[@]:1:2}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<two><three>");

    let out = sharky(&["-c", r#"a=(one two); printf '%s' "${#a[0]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3");
}

#[test]
fn indexed_arrays_accept_arithmetic_indexes_and_unset_keys() {
    let out = sharky(&["-c", r#"a=(one two three); i=1; printf '%s' "${a[i]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "two");

    let out = sharky(&["-c", r#"a=(one two); a[1]=TWO; printf '%s' "${a[1]}""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "TWO");

    let out = sharky(&[
        "-c",
        r#"declare -A a; a[k]=v; unset 'a[k]'; printf '%s' "${#a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0");
}


#[test]
fn quoted_glob_characters_stay_literal() {
    let dir = temp_dir();
    std::fs::write(dir.join("x1"), "").unwrap();
    std::fs::write(dir.join("x2"), "").unwrap();
    let cases = [
        (r#"printf '<%s>' x"*""#, "<x*>"),
        (r#"printf '<%s>' x\*"#, "<x*>"),
        (r#"a=("*"); printf '<%s>' x"${a[@]}""#, "<x*>"),
        (r#"p="*"; printf '<%s>' x${p}"#, "<x1><x2>"),
    ];
    for (script, expected) in cases {
        let out = sharky_in(&dir, &["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn mixed_quoted_glob_characters_keep_only_unquoted_parts_active() {
    let dir = temp_dir();
    std::fs::write(dir.join("alpha?"), "").unwrap();
    std::fs::write(dir.join("alphaX"), "").unwrap();

    let out = sharky_in(&dir, &["-c", r#"printf '<%s>' *"?""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<alpha?>");

    let out = sharky_in(
        &dir,
        &["-c", r#"a=("?"); printf '<%s>' alpha"${a[@]}""#],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<alpha?>");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn extglob_keeps_quoted_metacharacters_literal() {
    let out = sharky(&[
        "-c",
        r#"case x in @("*"|y)) printf BAD;; *) printf GOOD;; esac"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "GOOD");

    let out = sharky(&[
        "-c",
        r#"case "*" in @("*"|y)) printf GOOD;; *) printf BAD;; esac"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "GOOD");

    let out = sharky(&[
        "-c",
        r#"case ")" in @(\)|x)) printf HIT;; *) printf MISS;; esac"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "HIT");
}

#[test]
fn extglob_handles_nested_nullable_and_path_components() {
    let out = sharky(&["-c", r#"case '' in +(?(a))) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let out = sharky(&["-c", r#"case ')' in @(')'|x)) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    std::fs::write(dir.join("a/x.txt"), "").unwrap();
    std::fs::write(dir.join("b/x.txt"), "").unwrap();
    let out = sharky_in(&dir, &["-c", r#"printf '<%s>' @(a|b)/*.txt"#]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<a/x.txt><b/x.txt>"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn c_style_for_runs_init_condition_and_step() {
    let out = sharky(&[
        "-c",
        r#"for ((i = 0; i < 3; i++)); do printf '%s' "$i"; done"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "012");

    let out = sharky(&[
        "-c",
        r#"for ((i = 0; ; i++)); do ((i >= 2)) && break; printf '%s' "$i"; done"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "01");
}

#[test]
fn case_supports_fallthrough_terminators() {
    let out = sharky(&["-c", r#"x=ab; case $x in a*) printf A;;& *b) printf B;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AB");

    let out = sharky(&["-c", r#"x=ab; case $x in a*) printf A;& z*) printf B;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AB");

    let out = sharky(&["-c", r#"x=ab; case $x in a*) printf A;;& z*) printf Z;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "A");
}

#[test]
fn extglob_matches_in_case_and_paths() {
    let out = sharky(&["-c", r#"case foo in @(foo|bar)) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let out = sharky(&["-c", r#"case baz in !(foo|bar)) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let out = sharky(&["-c", r#"case foobar in +(foo|bar)) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let out = sharky(&["-c", r#"case foo in ?(foo)) printf yes;; esac"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");

    let dir = temp_dir();
    std::fs::write(dir.join("a.txt"), "").unwrap();
    std::fs::write(dir.join("b.txt"), "").unwrap();
    std::fs::write(dir.join("c.md"), "").unwrap();
    let out = sharky_in(&dir, &["-c", "printf '%s ' @(a|b).txt"]);
    std::fs::remove_dir_all(&dir).ok();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut files: Vec<&str> = text.split_whitespace().collect();
    files.sort();
    assert_eq!(files, vec!["a.txt", "b.txt"]);
}

#[test]
fn arithmetic_assigns_and_increments_variables() {
    let cases = [
        ("i=0; ((i = 3)); printf '%s' \"$i\"", "3"),
        ("i=1; ((i += 4)); printf '%s' \"$i\"", "5"),
        ("i=1; ((i++)); printf '%s' \"$i\"", "2"),
        ("i=1; ((++i)); printf '%s' \"$i\"", "2"),
        ("i=2; printf '%s' \"$((i--))\"; printf '%s' \"$i\"", "21"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn process_substitution_accepts_case_lists() {
    let cases = [
        ("cat <(case x in x) printf hi;; esac)", "hi"),
        ("cat <(case a in @(a|b)) printf hit;; esac)", "hit"),
        ("cat <(case x in (x) printf hi;; esac)", "hi"),
        ("cat <(case x in x) case y in y) printf nested;; esac;; esac)", "nested"),
        (r#"cat <(case x in "x") printf q;; esac)"#, "q"),
        ("cat <(echo case in)", "case in\n"),
        ("cat <(if true; then case x in x) printf A;; esac; fi)", "A"),
        ("cat <(for i in 1; do case x in x) printf B;; esac; done)", "B"),
        ("cat <(case x in esac)", ""),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
        assert_eq!(out.status.code(), Some(0), "{script}");
    }
}

#[test]
fn process_substitution_fifo_fallback_works() {
    let run = |script: &str| {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["-c", script])
            .env("SHARKY_CONFIG", NO_CONFIG)
            .env("SHARKY_PROCESS_SUBST_FIFO", "1")
            .output()
            .expect("failed to run sharky")
    };

    let out = run("cat <(printf hi)");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    assert_eq!(out.status.code(), Some(0));

    let out = run("diff <(printf a) <(printf a)");
    assert_eq!(out.status.code(), Some(0));

    let out = run(r#"f() { true; cat "$1"; }; f <(printf hi)"#);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");

    let dir = temp_dir();
    let out = run(&format!(
        "printf hello > {dir}/in; tee >(cat > {dir}/out) < {dir}/in > /dev/null; wait; cat {dir}/out",
        dir = dir.display()
    ));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hello");
    std::fs::remove_dir_all(&dir).ok();

    let out = run("true <(printf hi); wait; printf DONE");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "DONE");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn fifo_process_substitution_leaves_no_files() {
    let dir = temp_dir();
    let user = dir.join("sharky-psub-2147483647-userdata");
    std::fs::write(&user, "KEEP").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args([
            "-c",
            "printf x | cat <(printf y); printf hello > in; printf x | tee >(cat > out) < in > /dev/null; wait; cat out",
        ])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("SHARKY_PROCESS_SUBST_FIFO", "1")
        .env("TMPDIR", &dir)
        .current_dir(&dir)
        .output()
        .expect("failed to run sharky");
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.starts_with("sharky-psub-") && name != "sharky-psub-2147483647-userdata"
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert_eq!(std::fs::read_to_string(&user).unwrap(), "KEEP");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fifo_cleanup_does_not_remove_unregistered_fifos() {
    let dir = temp_dir();
    let sh = common::resolve("sh");
    let script = format!(
        "{} -c 'mkfifo \"$TMPDIR/sharky-psub-$$-unregistered\"' | cat; printf DONE",
        sh.display()
    );
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", &script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TMPDIR", &dir)
        .current_dir(&dir)
        .output()
        .expect("failed to run sharky");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "DONE");
    let fifos: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_fifo())
                .unwrap_or(false)
        })
        .collect();
    assert!(!fifos.is_empty(), "unregistered FIFO was removed: {out:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fifo_process_substitution_waits_for_a_slow_consumer() {
    let dir = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", r#"f() { sleep 2.2; cat "$1"; }; f <(printf hi)"#])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("SHARKY_PROCESS_SUBST_FIFO", "1")
        .env("TMPDIR", &dir)
        .output()
        .expect("failed to run sharky");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi", "{out:?}");
    assert_eq!(out.status.code(), Some(0));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn process_substitution_survives_a_function_body() {
    let out = sharky(&[
        "-c",
        r#"f() { true; cat "$1"; }; f <(printf hi)"#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn process_substitution_feeds_and_reads_pipes() {
    let out = sharky(&["-c", "cat <(printf hi)"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");

    let out = sharky(&["-c", "diff <(printf a) <(printf a)"]);
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "diff <(printf a) <(printf b)"]);
    assert_eq!(out.status.code(), Some(1));

    let dir = temp_dir();
    let script = "printf hi | tee >(cat > out) >/dev/null; wait; cat out";
    let out = sharky_in(&dir, &["-c", script]);
    let text = std::fs::read_to_string(dir.join("out")).unwrap_or_default();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(text, "hi");
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "cat <(cat <(printf nested))"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "nested");

    let out = sharky(&["-c", "cat <(false); printf '|%s' \"$?\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "|0");
}

#[test]
fn history_expansion_never_runs_in_c() {
    let out = sharky(&["-c", "echo hi; !!"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(out.status.code(), Some(127));
}

#[test]
fn exec_replaces_the_shell() {
    let echo = common::resolve("echo");
    let out = sharky(&["-c", &format!("exec {} hi", echo.display())]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(out.status.code(), Some(0));

    let sh = common::resolve("sh");
    let out = sharky(&[
        "-c",
        &format!("exec -a custom {} -c 'printf \"%s\" \"$0\"'", sh.display()),
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "custom");

    let out = sharky(&["-c", "exec"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(out.stdout.is_empty());
}

#[test]
fn exec_failure_stops_a_script() {
    let out = sharky(&["-c", "exec /nonexistent/sharky; echo after"]);
    assert_eq!(out.status.code(), Some(127));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("exec"));
}

#[test]
fn exec_redirections_persist_in_the_shell() {
    let dir = temp_dir();
    let out = sharky_in(&dir, &["-c", "exec > out 2>&1; echo hi; echo err >&2"]);
    let text = std::fs::read_to_string(dir.join("out")).unwrap_or_default();
    std::fs::remove_dir_all(&dir).ok();
    assert!(out.stdout.is_empty());
    assert_eq!(text, "hi\nerr\n");
}

// A redirection attached to a command the shell runs itself must apply for the
// duration of that command only: its effects stay, the descriptors go back.
#[test]
fn redirected_cd_changes_directory_and_restores_stdout() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(&dir, &["-c", "cd sub > out; pwd; printf 'after\\n'"]);
    let target = file_text(&dir.join("out"));
    let stray = dir.join("sub/out").exists();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stderr, "", "cd failed under the redirection");
    let mut lines = stdout.lines();
    assert!(
        lines.next().is_some_and(|line| line.ends_with("/sub")),
        "cd did not change the shell directory: {stdout:?}"
    );
    assert_eq!(
        lines.next(),
        Some("after"),
        "stdout was not restored after the redirection: {stdout:?}"
    );
    assert_eq!(target, "", "the redirected cd wrote to its target");
    assert!(
        !stray,
        "the target must be opened before cd changes directory"
    );
}

#[test]
fn redirected_read_sets_variables_in_the_shell() {
    let dir = temp_dir();
    std::fs::write(dir.join("input"), "value\n").unwrap();
    let out = sharky_in(&dir, &["-c", "read name < input; printf '<%s>' \"$name\""]);
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<value>",
        "read did not reach the shell environment (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn redirected_function_call_keeps_shell_effects() {
    let dir = temp_dir();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { X=set; printf body; }; f > out; printf '<%s>' \"$X\"; cat out",
        ],
    );
    let target = file_text(&dir.join("out"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<set>body",
        "the function did not run in the shell (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(target, "body", "the call's redirection captured the output");
}

#[test]
fn redirected_function_restores_stderr() {
    let dir = temp_dir();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { printf oops >&2; }; f 2> err; printf 'after\\n'",
        ],
    );
    let target = file_text(&dir.join("err"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(String::from_utf8_lossy(&out.stdout), "after\n");
    assert_eq!(target, "oops", "stderr was not captured by the redirection");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "stderr was not restored after the redirection"
    );
}

#[test]
fn redirected_function_reads_from_a_file_and_keeps_variables() {
    let dir = temp_dir();
    std::fs::write(dir.join("input"), "value\n").unwrap();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { read line; }; f < input; printf '<%s>' \"$line\"",
        ],
    );
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<value>",
        "the redirected stdin did not reach the function's read"
    );
}

#[test]
fn redirected_group_changes_directory_and_restores_stdout() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(&dir, &["-c", "{ cd sub; } > out; pwd; printf 'after\\n'"]);
    let target = file_text(&dir.join("out"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    let mut lines = stdout.lines();
    assert!(
        lines.next().is_some_and(|line| line.ends_with("/sub")),
        "the group did not change the shell directory: {stdout:?}"
    );
    assert_eq!(
        lines.next(),
        Some("after"),
        "stdout not restored: {stdout:?}"
    );
    assert_eq!(target, "");
}

// A redirection error must not skip only the redirection: the command does not
// run, the shell keeps its own descriptors, and later commands still work.
#[test]
fn failed_redirection_skips_the_builtin_and_keeps_streams() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "cd sub > /nonexistent/sharky-redirect/out; pwd; printf 'after\\n'",
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        stderr.contains("/nonexistent/sharky-redirect/out"),
        "no diagnostic for the failed redirection: {stderr:?}"
    );
    let mut lines = stdout.lines();
    assert!(
        lines.next().is_some_and(|line| !line.ends_with("/sub")),
        "cd ran despite the failed redirection: {stdout:?}"
    );
    assert_eq!(
        lines.next(),
        Some("after"),
        "stdout was not restored after the failed redirection: {stdout:?}"
    );
}

// The first redirect was already applied when the second one fails, so the
// shell must roll the first back instead of keeping its own stdout on it.
#[test]
fn failed_redirection_after_a_successful_one_keeps_the_shell_streams() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "cd sub > good 2> /nonexistent/sharky-redirect/bad; printf 'status=%s\\n' \"$?\"; pwd; printf 'after\\n'",
        ],
    );
    let good = file_text(&dir.join("good"));
    let stray = dir.join("sub/good").exists();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    assert!(
        stderr.contains("/nonexistent/sharky-redirect/bad"),
        "no diagnostic for the failed redirection: {stderr:?}"
    );
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("status=1"), "stdout: {stdout:?}");
    assert!(
        lines.next().is_some_and(|line| !line.ends_with("/sub")),
        "cd ran despite the failed redirection: {stdout:?}"
    );
    assert_eq!(
        lines.next(),
        Some("after"),
        "the shell stdout stayed on the partially applied target: {stdout:?}"
    );
    assert_eq!(good, "", "the applied redirect captured the shell output");
    assert!(
        !stray,
        "the target must be opened before cd changes directory"
    );
}

// A target fd that was closed before the command must be closed again after it.
#[test]
fn redirected_builtin_restores_a_previously_closed_fd() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "printf 'p' 2>/dev/null >&9; printf 'pre=<%s>\\n' \"$?\"; cd sub 9> out9; pwd; printf 'q' 2>/dev/null >&9; printf 'post=<%s>\\n' \"$?\"",
        ],
    );
    let target = file_text(&dir.join("out9"));
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    let mut lines = stdout.lines();
    assert_eq!(
        lines.next(),
        Some("pre=<1>"),
        "fd 9 was open before the redirection, so this case cannot observe it: {stdout:?}"
    );
    assert!(
        lines.next().is_some_and(|line| line.ends_with("/sub")),
        "cd did not change the shell directory: {stdout:?}"
    );
    assert_eq!(
        lines.next(),
        Some("post=<1>"),
        "fd 9 stayed open after the redirection: {stdout:?}"
    );
    assert_eq!(target, "");
}

// The inner call redirects the same descriptor the outer call redirected; the
// restore must return to the enclosing target, not to the shell's original one.
#[test]
fn nested_redirected_function_restores_the_enclosing_target() {
    let dir = temp_dir();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { printf body; }; g() { f > inner; printf g; }; g > outer; printf 'end\\n'",
        ],
    );
    let outer = file_text(&dir.join("outer"));
    let inner = file_text(&dir.join("inner"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "end\n",
        "stdout was not restored after the nested redirections (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(outer, "g", "the inner restore lost the outer target");
    assert_eq!(inner, "body", "the inner redirect did not capture its call");
}

// The temporary stdin redirect covers one read; the next one must reach the
// shell's own stdin again.
#[test]
fn redirected_function_uses_the_fd_the_target_opened() {
    let dir = temp_dir();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { printf x >&3; }; f 3> out; printf '<%s>' \"$?\"; cat out",
        ],
    );
    let target = file_text(&dir.join("out"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<0>x",
        "fd 3 was not the redirection target inside the function (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(target, "x");
}

// The opened file takes the lowest free descriptor, which can be the target
// itself; it must stay open and stay inheritable.
#[test]
fn redirected_external_uses_the_fd_the_target_opened() {
    let dir = temp_dir();
    let sh = common::resolve("sh");
    let script = format!(
        "{} -c 'printf x >/dev/fd/3' 3> out; printf '<%s>' \"$?\"; cat out",
        sh.display()
    );
    let out = sharky_in(&dir, &["-c", &script]);
    let target = file_text(&dir.join("out"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<0>x",
        "fd 3 did not reach the child (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(target, "x");
}

// The inner targets 10 and 11, the slots the outer save of fds 1 and 2 takes
// in a shell that starts with only 0..2 open.
#[test]
fn nested_redirects_keep_the_enclosing_target() {
    let dir = temp_dir();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "f() { printf body >&10; printf errb >&11; }; g() { f 10> inner 11> innererr; printf g; }; g > outer 2> outerr; printf 'end\\n'",
        ],
    );
    let outer = file_text(&dir.join("outer"));
    let outerr = file_text(&dir.join("outerr"));
    let inner = file_text(&dir.join("inner"));
    let innererr = file_text(&dir.join("innererr"));
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "end\n",
        "stdout was not restored (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        outer, "g",
        "the inner redirect corrupted the enclosing target"
    );
    assert_eq!(outerr, "");
    assert_eq!(inner, "body");
    assert_eq!(innererr, "errb");
}

// The numeric prefix saturates at i32::MAX; saving that descriptor must not
// overflow, and the shell must report the invalid target and carry on.
#[test]
fn redirected_saturated_fd_does_not_overflow() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let out = sharky_in(
        &dir,
        &[
            "-c",
            "cd sub 2147483647> out; printf 'status=%s\\n' \"$?\"; pwd",
        ],
    );
    let stray = dir.join("sub/out").exists();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let out = sharky_in(
        &dir,
        &["-c", "cd sub 1>&2147483647; printf 'status=%s\\n' \"$?\""],
    );
    let dup = String::from_utf8_lossy(&out.stdout).into_owned();
    let dup_stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        out.status.code(),
        Some(0),
        "the shell aborted: {dup_stderr}"
    );
    assert!(!stderr.is_empty(), "no diagnostic for the invalid target");
    let mut lines = stdout.lines();
    assert_eq!(lines.next(), Some("status=1"), "stdout: {stdout:?}");
    assert!(
        lines.next().is_some_and(|line| !line.ends_with("/sub")),
        "cd ran despite the failed redirection: {stdout:?}"
    );
    assert!(!stray, "the target must not move with the directory");
    assert!(
        !dup_stderr.is_empty(),
        "no diagnostic for the invalid dup source"
    );
    assert_eq!(dup.trim_end(), "status=1", "stdout: {dup:?}");
}

#[test]
fn redirected_read_restores_the_shell_stdin() {
    let dir = temp_dir();
    std::fs::write(dir.join("data"), "one\n").unwrap();
    let out = sharky_in_with_stdin(
        &dir,
        &[
            "-c",
            "read first < data; read second; printf '<%s|%s>' \"$first\" \"$second\"",
        ],
        "two\n",
    );
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "<one|two>",
        "stdin was not restored after the temporary redirect (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn time_reports_real_user_and_system_time() {
    let out = sharky(&["-c", "time printf hi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for label in ["real", "user", "sys"] {
        assert!(stderr.contains(label), "missing {label} in {stderr:?}");
    }
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "time -p false"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("real 0."), "posix format missing in {stderr:?}");

    let out = sharky(&["-c", "time printf hi | cat"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi");
    assert!(String::from_utf8_lossy(&out.stderr).contains("real"));
}

#[test]
fn json_command_reports_status_streams_and_schema() {
    let out = sharky(&["--json", "-c", "printf hi"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "\"schema_version\":1",
        "\"status\":0",
        "\"stdout\":\"hi\"",
        "\"stderr\":\"\"",
        "\"duration_ms\":",
        "\"env_delta\":{}",
    ] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(!text.contains("133;"), "OSC marks in JSON: {text}");

    let out = sharky(&["--json", "-c", "echo err >&2"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"stdout\":\"\""), "{text}");
    assert!(text.contains("\"stderr\":\"err\\n\""), "{text}");

    let out = sharky(&["--json", "-c", "exit 7"]);
    assert_eq!(out.status.code(), Some(7));
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"status\":7"));

    let printf = common::resolve("printf");
    let out = sharky(&["--json", "-c", &format!("{} '\\377'", printf.display())]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains('\u{fffd}'), "non-UTF-8 not replaced: {text}");
}

#[test]
fn json_command_reports_environment_delta_and_cwd() {
    let out = sharky(&["--json", "-c", "export FOO=bar"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"FOO\":\"bar\""), "{text}");

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "--json", "-c", "unset FOO"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("FOO", "initial")
        .output()
        .expect("failed to run sharky");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"FOO\":null"), "{text}");

    let out = sharky(&["--json", "-c", "cd /tmp"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"cwd\":\"/tmp\""), "{text}");
}

#[test]
fn json_command_wraps_exec_in_the_envelope() {
    let echo = common::resolve("echo");
    let out = sharky(&["--json", "-c", &format!("exec {} hi", echo.display())]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with('{'), "{text}");
    assert!(text.contains("\"stdout\":\"hi\\n\""), "{text}");
}

#[test]
fn json_exec_publishes_the_state_before_the_process_replacement() {
    let echo = common::resolve("echo");
    let out = sharky(&[
        "--json",
        "-c",
        &format!("export AUDIT=changed; cd /; exec {} hi", echo.display()),
    ]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("\"cwd\":\"/\""), "{text}");
    assert!(text.contains("\"AUDIT\":\"changed\""), "{text}");
    assert!(text.contains("\"stdout\":\"hi\\n\""), "{text}");
}

#[test]
fn json_capture_files_are_private_and_removed() {
    let dir = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--json", "-c", "stat -L -c %a /proc/self/fd/1"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TMPDIR", &dir)
        .output()
        .expect("failed to run sharky");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"stdout\":\"600\\n\""), "{text}");
    let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn json_capture_failure_does_not_run_the_command() {
    let dir = temp_dir();
    let missing = dir.join("missing");
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--json", "-c", "printf HI"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TMPDIR", &missing)
        .output()
        .expect("failed to run sharky");
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("capture"), "{out:?}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn json_exec_from_a_profile_keeps_the_state() {
    use std::os::unix::fs::PermissionsExt;

    let home = login_home("json-exec-profile");
    // NixOS coreutils is a multicall binary that dispatches on argv[0], so
    // `exec -a NAME <echo>` alone would be seen as an unknown applet. Run echo
    // through a tiny wrapper to keep the `exec -a` semantics under test while
    // preserving echo's observable output.
    let sh = common::resolve("sh");
    let echo = common::resolve("echo");
    let wrapper = home.join("profile-echo");
    std::fs::write(
        &wrapper,
        format!("#!{}\nexec {} \"$@\"\n", sh.display(), echo.display()),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        format!(
            "export AUDIT_PROFILE=changed\ncd /\nexec -a NAME {} PROFILE\n",
            wrapper.display()
        ),
    )
    .unwrap();
    let out = sharky_with_home(&home, &["-l", "--json", "-c", "true"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"stdout\":\"PROFILE\\n\""), "{text}");
    assert!(text.contains("\"cwd\":\"/\""), "{text}");
    assert!(text.contains("\"AUDIT_PROFILE\":\"changed\""), "{text}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profiles_stay_inside_the_json_envelope() {
    let home = login_home("json-envelope");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "printf PROFILE\n",
    )
    .unwrap();
    let out = sharky_with_home(&home, &["-l", "--json", "-c", "printf COMMAND"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.starts_with('{'), "{text}");
    assert!(text.contains("\"stdout\":\"PROFILECOMMAND\""), "{text}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn state_json_replaces_non_utf8_environment_values() {
    use std::os::unix::ffi::OsStrExt;

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "state", "--json", "--env"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("AUDIT_BAD", std::ffi::OsStr::from_bytes(b"A\xff"))
        .output()
        .expect("failed to run sharky");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"AUDIT_BAD\":\"A\u{fffd}\""), "{text}");
}

#[test]
fn state_json_reports_a_stable_shape() {
    let out = sharky(&["--no-config", "state", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for expected in [
        "\"schema_version\":1",
        "\"cwd\":",
        "\"last_status\":0",
        "\"pid\":",
    ] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(!text.contains("\"env\":"), "env without --env: {text}");

    let out = sharky(&["--no-config", "state", "--json", "--env"]);
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"env\":{"), "missing env: {text}");
    assert!(text.contains("\"PATH\":"), "missing PATH: {text}");
}

#[test]
fn cli_validates_agent_options() {
    let cases = [
        (vec!["state"], "state requires --json"),
        (
            vec!["--env", "-c", "true"],
            "--env is only supported with state",
        ),
        (
            vec!["--json", "missing-script.sh"],
            "--json is only supported with -c or state",
        ),
        (vec!["--json"], "--json is only supported with -c or state"),
        (
            vec!["-c", "true", "missing-script.sh"],
            "cannot use -c with a script",
        ),
        (
            vec!["state", "--json", "-c", "true"],
            "cannot use state with -c",
        ),
    ];
    for (args, message) in cases {
        let out = sharky(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(message),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let out = sharky(&["-i"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("interactive mode requires a terminal")
    );
}

#[test]
fn state_json_does_not_create_history_or_z_files() {
    let data = std::env::temp_dir().join(format!("sharky-state-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "state", "--json"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("XDG_DATA_HOME", &data)
        .output()
        .expect("failed to run sharky");
    assert!(out.status.success());
    assert!(!data.join("sharky/history").exists());
    assert!(!data.join("sharky/z.jsonl").exists());
    std::fs::remove_dir_all(&data).ok();
}

#[test]
fn help_lists_private_flag_but_no_merge_flag() {
    let out = sharky(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for flag in ["-P", "--private"] {
        assert!(text.contains(flag), "missing {flag} in help: {text}");
    }
    assert!(!text.contains("--merge"), "unrequested flag in help: {text}");
}

#[test]
fn merge_startup_flag_is_rejected() {
    let out = sharky(&["--merge", "-c", "true"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("unknown option: --merge"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_explain_without_topic_prints_a_text_index() {
    let out = sharky(&["config", "explain"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("tide history"), "{text}");
    assert!(text.contains("anchor"), "{text}");
    assert!(!text.contains('{'), "index must be plain text: {text}");
}

#[test]
fn config_explain_joins_space_separated_topic_words() {
    let out = sharky(&["config", "explain", "tide", "history"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("tide history"), "{text}");
    assert!(text.contains("1000"), "{text}");
    assert!(text.contains("expects a number"), "{text}");
    assert!(!text.contains('{'), "topic must be plain text: {text}");
}

#[test]
fn config_explain_exposes_every_recognized_directive() {
    for directive in [
        "tide", "pearl", "current", "reef", "shoal", "anchor", "coral", "wave", "wake",
    ] {
        let out = sharky(&["config", "explain", directive]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{directive}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains(directive), "{directive}: {text}");
    }
}

#[test]
fn config_explain_loads_no_config_and_has_no_side_effects() {
    let dir = temp_dir();
    std::fs::write(
        dir.join("config.sharky"),
        "wake \"printf WAKE\"\ntide greeting yes\n",
    )
    .unwrap();
    let data = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["config", "explain", "tide", "history"])
        .env("SHARKY_CONFIG", &dir)
        .env("XDG_DATA_HOME", &data)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "config must not be read: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!String::from_utf8_lossy(&out.stdout).contains("WAKE"));
    assert!(!data.join("sharky/history").exists());
    assert!(!data.join("sharky/z.jsonl").exists());
}

#[test]
fn config_explain_unknown_topic_exits_two() {
    let out = sharky(&["config", "explain", "not-a-topic"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("config explain: unknown topic 'not-a-topic'"),
        "{err}"
    );
}

#[test]
fn config_explain_rejects_options_and_mixed_requests() {
    let cases = [
        (
            vec!["config", "explain", "--json"],
            "config explain: unexpected option '--json'",
        ),
        (
            vec!["config", "explain", "-c", "true"],
            "config explain: unexpected option '-c'",
        ),
        (
            vec!["--json", "config", "explain"],
            "sharky: --json is only supported with -c or state",
        ),
    ];
    for (args, message) in cases {
        let out = sharky(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(message), "{args:?}: {err}");
    }
}

#[test]
fn help_lists_the_config_explain_subcommand() {
    let out = sharky(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("config explain"), "{text}");
}

#[test]
fn config_explain_has_no_top_level_alias() {
    let out = sharky(&["explain"]);
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Configuration guide topics"),
        "top-level explain must only be a subcommand after config"
    );
}

#[test]
fn config_explain_after_a_script_is_passed_as_positional_arguments() {
    let dir = temp_dir();
    let script = dir.join("positional.sh");
    std::fs::write(&script, "printf '<%s>|<%s>|<%s>' \"$1\" \"$2\" \"$#\"\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&script)
        .args(["config", "explain"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "<config>|<explain>|<2>");
}

#[test]
fn config_explain_after_a_command_does_not_hijack_execution() {
    let out = sharky(&["-c", "echo hi", "config", "explain"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("cannot use -c with a script"), "{err}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("Configuration guide topics"),
        "-c invocation must not print the guide"
    );
}

fn history_fixture(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new("/tmp/opencode").join(format!(
        "sharky-cli-history-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_with_history(dir: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("SHARKY_HISTORY", dir.join("history"))
        .env("HOME", dir)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn private_short_flag_keeps_command_semantics_and_leaves_history_alone() {
    let dir = history_fixture("private-short");
    let history = dir.join("history");
    std::fs::write(&history, "seed command\n").unwrap();
    let before = std::fs::read(&history).unwrap();

    let out = sharky_with_history(&dir, &["-P", "--json", "-c", "printf ok"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"stdout\":\"ok\""), "{text}");
    assert_eq!(std::fs::read(&history).unwrap(), before, "private mode wrote history");

    std::fs::remove_dir_all(&dir).ok();
}

fn login_home(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sharky-login-{}-{name}", std::process::id()));
    std::fs::create_dir_all(dir.join(".config/sharky")).unwrap();
    dir
}

fn sharky_with_home(home: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", home)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn login_profiles_run_before_the_command() {
    let home = login_home("profile");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "LOGIN_VAR=from-profile\nalias loginhi 'printf ALIASED'\n",
    )
    .unwrap();

    let out = sharky_with_home(&home, &["-l", "-c", r#"printf '%s|' "$LOGIN_VAR"; loginhi"#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "from-profile|ALIASED");
    assert!(out.status.success());

    let out = sharky_with_home(&home, &["-c", r#"printf '%s' "$LOGIN_VAR""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profiles_run_control_flow_as_one_unit() {
    let home = login_home("flow");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "if false; then\nprintf UNEXPECTED\nfi\nprintf VISIBLE\n",
    )
    .unwrap();

    let out = sharky_with_home(&home, &["-l", "-c", "printf COMMAND"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "VISIBLECOMMAND");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("UNEXPECTED"));
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn wake_is_a_builtin_and_profiles_can_register_it() {
    let home = login_home("wake");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "wake \"printf WAKE\"\n",
    )
    .unwrap();

    let out = sharky_with_home(&home, &["-l", "-c", "printf AFTER"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "AFTER");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("wake"),
        "{out:?}"
    );
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profile_errors_report_the_real_line() {
    let home = login_home("line");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "X=1\nY=2\n)\nprintf OK\n",
    )
    .unwrap();

    let out = sharky_with_home(&home, &["-l", "-c", "printf COMMAND"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "COMMAND");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("profile.sharky:3:"), "{stderr}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profile_errors_report_file_and_line_and_stop_the_file() {
    let home = login_home("error");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        ")\nAUDIT_PROFILE_ONLY=yes\n",
    )
    .unwrap();
    std::fs::write(home.join(".bash_profile"), "PLAIN=value\n").unwrap();

    let out = sharky_with_home(
        &home,
        &[
            "-l",
            "-c",
            r#"printf '%s|%s' "$AUDIT_PROFILE_ONLY" "$PLAIN""#,
        ],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "|value");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("profile.sharky:1:"), "{stderr}");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_imports_only_literal_standard_assignments() {
    let home = login_home("imports");
    std::fs::write(
        home.join(".profile"),
        "export IMPORTED=literal\nBAD=$(date)\nDANGER=foo; rm -rf /tmp/sharky-login\n",
    )
    .unwrap();
    std::fs::write(home.join(".bash_profile"), "PLAIN=value\n").unwrap();

    let out = sharky_with_home(
        &home,
        &[
            "-l",
            "-c",
            r#"printf '%s|%s|%s' "$IMPORTED" "$PLAIN" "$BAD""#,
        ],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "literal|value|");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn argv_zero_starting_with_a_dash_enters_login_mode() {
    use std::os::unix::process::CommandExt;

    let home = login_home("argv0");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "ARGV_LOGIN=yes\n",
    )
    .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg0("-sharky")
        .args(["-c", r#"printf '%s' "$ARGV_LOGIN""#])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", &home)
        .output()
        .expect("failed to run sharky");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn login_profiles_run_before_a_script() {
    let home = login_home("script");
    std::fs::write(
        home.join(".config/sharky/profile.sharky"),
        "SCRIPT_LOGIN=yes\n",
    )
    .unwrap();
    let script = home.join("show.sh");
    std::fs::write(&script, r#"printf '%s' "$SCRIPT_LOGIN""#).unwrap();

    let out = sharky_with_home(
        &home,
        &["-l", script.to_str().unwrap()],
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "yes");
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn set_reports_and_toggles_vi_mode() {
    let out = sharky(&["-c", "set -o vi; set -o"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("vi"));

    let out = sharky(&["-c", "set -o vi; set +o vi; set -o"]);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("vi"));
}

#[test]
fn status_reports_version_resolved_paths_and_feature_flags() {
    let dir = temp_dir();
    std::fs::write(dir.join("config.sharky"), "tide greeting off\n").unwrap();
    let data = temp_dir();
    let history = data.join("my-history");
    std::fs::write(&history, "previous command\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("status")
        .env("SHARKY_CONFIG", &dir)
        .env("SHARKY_HISTORY", &history)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains(&format!("sharky {}", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "config: {} (exists)",
            dir.join("config.sharky").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "theme: {} (missing)",
            dir.join("theme.sharky").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!("history: {} (exists)", history.display())),
        "{text}"
    );
    assert!(text.contains("features: none"), "{text}");
    assert!(!text.contains('{'), "status must be plain text: {text}");
}

#[test]
fn status_honours_config_and_history_environment_precedence() {
    let chosen = temp_dir();
    let xdg_config = temp_dir();
    let home = temp_dir();
    let chosen_data = temp_dir();
    let xdg_data = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("status")
        .env("SHARKY_CONFIG", &chosen)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("HOME", &home)
        .env("SHARKY_HISTORY", chosen_data.join("history"))
        .env("XDG_DATA_HOME", &xdg_data)
        .output()
        .expect("failed to run sharky");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(&format!("config: {}", chosen.join("config.sharky").display())),
        "{text}"
    );
    assert!(!text.contains(&xdg_config.display().to_string()), "{text}");
    assert!(
        text.contains(&format!("history: {}", chosen_data.join("history").display())),
        "{text}"
    );
    assert!(!text.contains(&xdg_data.display().to_string()), "{text}");

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("status")
        .env_remove("SHARKY_CONFIG")
        .env_remove("SHARKY_HISTORY")
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .env("HOME", &home)
        .output()
        .expect("failed to run sharky");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(
        text.contains(&format!(
            "config: {}",
            xdg_config.join("sharky/config.sharky").display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "history: {}",
            xdg_data.join("sharky/history").display()
        )),
        "{text}"
    );
    for dir in [chosen, xdg_config, home, chosen_data, xdg_data] {
        std::fs::remove_dir_all(dir).ok();
    }
}

#[test]
fn status_never_prints_secret_values_or_pearl_directives() {
    let dir = temp_dir();
    std::fs::write(
        dir.join("config.sharky"),
        "pearl TOP_SECRET_STATUS_TOKEN \"s3cr3t-status-leak\"\nwake \"printf STATUS_LEAK\"\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("status")
        .env("SHARKY_CONFIG", &dir)
        .env("SHARKY_SUPER_SECRET", "s3cr3t-env-leak")
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(!text.contains("s3cr3t-status-leak"), "{text}");
    assert!(!text.contains("TOP_SECRET_STATUS_TOKEN"), "{text}");
    assert!(!text.contains("s3cr3t-env-leak"), "{text}");
    assert!(!text.contains("SHARKY_SUPER_SECRET"), "{text}");
    assert!(!text.contains("STATUS_LEAK"), "{text}");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("s3cr3t"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn status_creates_no_config_history_or_z_file() {
    let dir = temp_dir();
    let data = temp_dir();
    let home = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("status")
        .env("SHARKY_CONFIG", &dir)
        .env("XDG_DATA_HOME", &data)
        .env("HOME", &home)
        .output()
        .expect("failed to run sharky");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!dir.join("config.sharky").exists());
    assert!(!dir.join("theme.sharky").exists());
    assert!(!dir.join("profile.sharky").exists());
    assert!(!data.join("sharky/history").exists());
    assert!(!data.join("sharky/z.jsonl").exists());
    assert!(!home.join(".local/share/sharky/history").exists());
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn status_rejects_options_and_extra_arguments() {
    for args in [vec!["status", "--json"], vec!["status", "extra"]] {
        let out = sharky(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("status:"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn sharky_doctor(config: &std::path::Path, data: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("doctor")
        .env("SHARKY_CONFIG", config)
        .env("XDG_DATA_HOME", data)
        .env("HOME", data)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn doctor_reports_every_check_and_exits_zero_when_healthy() {
    let dir = temp_dir();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = sharky_doctor(&dir, &data);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 6, "{text}");
    for (name, line) in ["stdin", "stdout", "config", "theme", "history", "tools"]
        .iter()
        .zip(&lines)
    {
        assert!(line.starts_with("OK   "), "{line}");
        assert!(line.contains(&format!("{name}:")), "{line}");
    }
}

#[test]
fn doctor_reports_a_config_parse_error_and_exits_one() {
    let dir = temp_dir();
    std::fs::write(dir.join("config.sharky"), "coral kelp\n").unwrap();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = sharky_doctor(&dir, &data);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("FAIL config:"), "{text}");
    assert!(text.contains("line 1"), "{text}");
}

#[test]
fn doctor_detects_an_unreadable_config_file_that_load_would_skip() {
    let dir = temp_dir();
    std::fs::create_dir(dir.join("config.sharky")).unwrap();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = sharky_doctor(&dir, &data);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("FAIL config:"), "{text}");
    assert!(text.contains("load would skip it"), "{text}");
}

#[test]
fn doctor_reports_a_theme_parse_error_and_exits_one() {
    let dir = temp_dir();
    std::fs::write(dir.join("theme.sharky"), "coral kelp not-a-colour\n").unwrap();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = sharky_doctor(&dir, &data);
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("FAIL theme:"), "{text}");
    assert!(text.contains("line 1"), "{text}");
}

#[test]
fn doctor_checks_history_permissions_without_opening_or_changing_the_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = temp_dir();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let history = data.join("sharky/history");
    std::fs::write(&history, "previous command\n").unwrap();
    std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o666)).unwrap();

    let out = sharky_doctor(&dir, &data);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("FAIL history: world-writable"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(std::fs::read_to_string(&history).unwrap(), "previous command\n");
    assert_eq!(std::fs::metadata(&history).unwrap().permissions().mode() & 0o777, 0o666);

    std::fs::set_permissions(&history, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = sharky_doctor(&dir, &data);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(std::fs::read_to_string(&history).unwrap(), "previous command\n");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
}

#[test]
fn doctor_detects_tools_missing_from_path() {
    let dir = temp_dir();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let empty_path = temp_dir();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("doctor")
        .env("SHARKY_CONFIG", &dir)
        .env("XDG_DATA_HOME", &data)
        .env("HOME", &data)
        .env("PATH", &empty_path)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    std::fs::remove_dir_all(&empty_path).ok();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("FAIL tools:"), "{text}");
    assert!(text.contains("sh"), "{text}");
}

#[test]
fn doctor_never_prints_secret_values_or_pearl_directives() {
    let dir = temp_dir();
    std::fs::write(
        dir.join("config.sharky"),
        "pearl TOP_SECRET_DOCTOR_TOKEN \"s3cr3t-doctor-leak\"\n",
    )
    .unwrap();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("doctor")
        .env("SHARKY_CONFIG", &dir)
        .env("XDG_DATA_HOME", &data)
        .env("HOME", &data)
        .env("SHARKY_SUPER_SECRET", "s3cr3t-env-leak")
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(out.status.code(), Some(0));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!all.contains("s3cr3t-doctor-leak"), "{all}");
    assert!(!all.contains("TOP_SECRET_DOCTOR_TOKEN"), "{all}");
    assert!(!all.contains("s3cr3t-env-leak"), "{all}");
    assert!(!all.contains("SHARKY_SUPER_SECRET"), "{all}");
}

#[test]
fn doctor_creates_no_config_history_or_z_file() {
    let dir = temp_dir();
    let data = temp_dir();
    std::fs::create_dir_all(data.join("sharky")).unwrap();
    let out = sharky_doctor(&dir, &data);
    assert!(!dir.join("config.sharky").exists());
    assert!(!dir.join("theme.sharky").exists());
    assert!(!dir.join("profile.sharky").exists());
    assert!(!data.join("sharky/history").exists());
    assert!(!data.join("sharky/z.jsonl").exists());
    assert!(!data.join(".local/share/sharky/history").exists());
    // A healthy environment is required for this assertion to be meaningful.
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&data).ok();
}

#[test]
fn doctor_rejects_options_and_extra_arguments() {
    for args in [vec!["doctor", "--json"], vec!["doctor", "extra"]] {
        let out = sharky(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("doctor:"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn help_lists_status_and_doctor_subcommands() {
    let out = sharky(&["--help"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("status"), "{text}");
    assert!(text.contains("doctor"), "{text}");
}
