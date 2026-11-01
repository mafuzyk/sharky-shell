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
fn sparse_array_assignment_pads_with_empty_elements() {
    let out = sharky(&[
        "-c",
        r#"a=(one); a[3]=x; printf '%s|%s' "${#a[@]}" "${a[2]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "4|");
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
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound variable"));
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
    std::fs::write(&tool, "#!/bin/sh\nprintf 'spicetify-test'\n").unwrap();
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
        ("set -- a b c; printf '%s' \"${@:2}\"", "b c"),
    ];
    for (script, expected) in cases {
        let out = sharky(&["-c", script]);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
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

    let out = sharky(&["-c", "prefix_b=2; prefix_a=1; printf '%s' \"${!prefix_@}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "prefix_a prefix_b");

    let out = sharky(&["-c", "a=(x y z); printf '%s' \"${!a[@]}\""]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0 1 2");
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
        r#"declare -A a; a[one]=1; a[two]=2; printf '%s|%s|%s' "${a[one]}" "${#a[@]}" "${!a[@]}""#,
    ]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1|2|one two");
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
    let out = sharky(&["-c", "exec /bin/echo hi"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
    assert_eq!(out.status.code(), Some(0));

    let out = sharky(&["-c", "exec -a custom /bin/sh -c 'printf \"%s\" \"$0\"'"]);
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

    let out = sharky(&["--json", "-c", "/bin/printf '\\377'"]);
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
    assert!(!data.join("sharky/z").exists());
    std::fs::remove_dir_all(&data).ok();
}
