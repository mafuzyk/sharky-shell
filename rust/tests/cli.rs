use std::process::{Command, Output};

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
fn case_fallthrough_is_a_syntax_error_not_a_panic() {
    let out = sharky(&["-c", "case a in a) :;;& esac"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("syntax error"));
}

#[test]
fn for_without_a_variable_is_a_syntax_error() {
    let out = sharky(&["-c", "for ; do :; done"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("syntax error"));
}

#[test]
fn c_style_for_is_rejected_cleanly() {
    let out = sharky(&["-c", "for ((i=0;i<2;i++)); do :; done"]);
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
