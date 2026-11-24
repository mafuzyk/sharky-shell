//! `PIPESTATUS` regressions. Every expected value below was measured against
//! bash 5.3.20 first; the comment on each test names the observed result.

use std::process::{Command, Output};

mod common;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", common::tool_path())
        .output()
        .expect("failed to run sharky")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// bash: `false | true; echo "${PIPESTATUS[@]}"` prints `1 0`.
#[test]
fn pipeline_status_reports_every_stage() {
    let out = sharky(r#"false | true; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1 0\n");
}

// bash: `true | false | true; echo "${PIPESTATUS[@]}"` prints `0 1 0`.
#[test]
fn pipeline_status_reports_three_stages() {
    let out = sharky(r#"true | false | true; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0 1 0\n");
}

// bash: `false | true; :; echo "${PIPESTATUS[@]}"` prints `0`: any later
// command replaces the array with a single element holding its status.
#[test]
fn a_builtin_replaces_the_array_with_its_own_status() {
    let out = sharky(r#"false | true; :; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0\n");
}

// bash: `true; echo "${#PIPESTATUS[@]}"` prints `1`.
#[test]
fn a_simple_command_leaves_one_element() {
    let out = sharky(r#"true; echo "${#PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1\n");
}

// bash: `! false | true; echo "${PIPESTATUS[@]} $?"` prints `1 0 1`: the
// array keeps the un-negated stage statuses while `$?` is the negated one.
#[test]
fn negation_keeps_the_stage_statuses() {
    let out = sharky(r#"! false | true; echo "${PIPESTATUS[@]} $?""#);
    assert_eq!(stdout(&out), "1 0 1\n");
}

// bash: `! false; echo "${PIPESTATUS[@]}"` prints `1`.
#[test]
fn negation_of_a_simple_command_records_the_inner_status() {
    let out = sharky(r#"! false; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1\n");
}

// bash: `x=$(false | true); echo "${PIPESTATUS[@]}"` prints `0`: the
// assignment command's status replaces the substitution's array.
#[test]
fn assignment_replaces_the_array_with_its_own_status() {
    let out = sharky(r#"x=$(false | true); echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0\n");
}

// bash: `set -o pipefail; false | true; echo "${PIPESTATUS[@]} $?"` prints
// `1 0 1`: pipefail only changes `$?`.
#[test]
fn pipefail_changes_only_the_exit_status() {
    let out = sharky(r#"set -o pipefail; false | true; echo "${PIPESTATUS[@]} $?""#);
    assert_eq!(stdout(&out), "1 0 1\n");
}

// bash: `false | true; (echo "${PIPESTATUS[@]}"); echo "${PIPESTATUS[@]}"`
// prints `1 0` from the child, then `0`: the subshell inherits the array but
// its completion does not clobber the parent's with the inner values.
#[test]
fn a_subshell_inherits_and_isolates_the_array() {
    let out = sharky(r#"false | true; (echo "${PIPESTATUS[@]}"); echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1 0\n0\n");
}

// bash: `false | true; y=$(echo "${PIPESTATUS[@]}"); echo "$y"; echo
// "${PIPESTATUS[@]}"` prints `1 0` inherited by the substitution, then `0`.
#[test]
fn command_substitution_inherits_and_isolates_the_array() {
    let out =
        sharky(r#"false | true; y=$(echo "${PIPESTATUS[@]}"); echo "$y"; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1 0\n0\n");
}

// bash: `f() { false | true; }; f; echo "${PIPESTATUS[@]}"` prints `0`: the
// call is a simple command, so its status replaces the body's array.
#[test]
fn a_function_call_replaces_the_array_with_its_status() {
    let out = sharky(r#"f() { false | true; }; f; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0\n");
}

// bash: `if true | false; then :; fi; echo "${PIPESTATUS[@]}"` prints `0 1`
// and `for i in 1; do false | true; done` prints `1 0`: compound commands do
// not replace the array, only the commands they run.
#[test]
fn compound_commands_keep_the_last_inner_array() {
    let out = sharky(r#"if true | false; then :; fi; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0 1\n");

    let out = sharky(r#"for i in 1; do false | true; done; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1 0\n");
}

// bash: `true | { false; true; }; echo "${PIPESTATUS[@]}"` prints `0 0`: the
// parent records the stage statuses, not the group's inner commands.
#[test]
fn pipeline_stage_statuses_come_from_the_parent() {
    let out = sharky(r#"true | { false; true; }; echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "0 0\n");
}

// bash: `false | true; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]}
// ${#PIPESTATUS[@]}"` after `echo hi | cat` prints `0 0 2`, and the star form
// joins with a space.
#[test]
fn indexes_length_and_star_form_work() {
    let out = sharky(
        r#"echo hi | cat; echo "${PIPESTATUS[0]} ${PIPESTATUS[1]} ${#PIPESTATUS[@]}"; false | true; echo "${PIPESTATUS[*]}""#,
    );
    assert_eq!(stdout(&out), "hi\n0 0 2\n1 0\n");
}

// bash: `false | true; false & echo "${PIPESTATUS[@]}"` prints `1 0`: starting
// a background job does not replace the array.
#[test]
fn a_background_launch_keeps_the_array() {
    let out = sharky(r#"false | true; false & echo "${PIPESTATUS[@]}""#);
    assert_eq!(stdout(&out), "1 0\n");
}

// bash: `sleep 5 | true` with the sleep killed reports `143 0`; here the first
// stage kills itself with SIGTERM.
#[test]
fn a_signaled_stage_reports_128_plus_the_signal() {
    let sh = common::resolve("sh");
    let script = format!(
        r#"'{}' -c 'kill -TERM $$' | true; echo "${{PIPESTATUS[@]}}""#,
        sh.display()
    );
    let out = sharky(&script);
    assert_eq!(stdout(&out), "143 0\n");
}

// bash: `${PIPESTATUS[5]}` is unset while the array is shorter, and the
// default operator applies.
#[test]
fn a_missing_index_is_unset() {
    let out = sharky(r#"false | true; echo "${PIPESTATUS[5]-unset}""#);
    assert_eq!(stdout(&out), "unset\n");
}

// bash: the array is empty before the first command runs.
#[test]
fn the_array_starts_empty() {
    let out = sharky(r#"echo "[${PIPESTATUS[@]}]""#);
    assert_eq!(stdout(&out), "[]\n");
}
