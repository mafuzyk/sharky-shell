//! `eval`: parse and run the joined arguments in the current shell.
//!
//! Every expectation here was measured against bash 5.3.20; the login helper
//! shape (`eval "$(...)") is the one the real /etc/bashrc depends on.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-eval-test";

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("sharky-eval-builtin-{}-{name}", std::process::id()));
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

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

#[test]
fn assignment_persists_in_the_current_shell() {
    let home = scratch("assign");
    let out = sharky(&home, r#"eval 'x=5'; echo "$x""#);
    assert_eq!(stdout(&out), "5\n", "{out:?}");
    assert_eq!(status(&out), 0);
}

#[test]
fn export_persists_into_child_environments() {
    let home = scratch("export");
    let out = sharky(&home, r#"eval 'export EV=1'; printenv EV"#);
    assert_eq!(stdout(&out), "1\n", "{out:?}");
}

#[test]
fn status_comes_from_the_last_command() {
    let home = scratch("status");
    let out = sharky(&home, r#"eval 'echo one; false'; echo "s=$?""#);
    assert_eq!(stdout(&out), "one\ns=1\n", "{out:?}");
}

#[test]
fn no_arguments_succeeds_without_changing_state() {
    let home = scratch("noargs");
    let out = sharky(&home, r#"false; eval; echo "s=$?""#);
    assert_eq!(stdout(&out), "s=0\n", "{out:?}");
}

#[test]
fn arguments_are_joined_with_spaces() {
    let home = scratch("join");
    let out = sharky(&home, r#"eval 'echo one' 'echo two'"#);
    assert_eq!(stdout(&out), "one echo two\n", "{out:?}");
}

#[test]
fn expansions_redirections_and_pipelines_are_parsed() {
    let home = scratch("parse");
    let path = home.join("out");
    let script = format!(
        r#"eval 'echo hi | tr a-z A-Z > {}'; cat {}"#,
        path.display(),
        path.display()
    );
    let out = sharky(&home, &script);
    assert_eq!(stdout(&out), "HI\n", "{out:?}");
}

#[test]
fn nested_eval_runs_its_inner_string() {
    let home = scratch("nested");
    let out = sharky(&home, r#"eval 'eval "y=7"'; echo "$y""#);
    assert_eq!(stdout(&out), "7\n", "{out:?}");
}

#[test]
fn functions_defined_by_eval_are_callable() {
    let home = scratch("function");
    let out = sharky(&home, r#"eval 'f() { echo fn; }'; f"#);
    assert_eq!(stdout(&out), "fn\n", "{out:?}");
}

#[test]
fn syntax_error_reports_status_two_and_a_diagnostic() {
    let home = scratch("syntax");
    let out = sharky(&home, r#"eval 'if'; echo "s=$?""#);
    assert_eq!(stdout(&out), "s=2\n", "{out:?}");
    let err = stderr(&out);
    assert!(err.contains("eval"), "diagnostic must name eval: {err:?}");
}

#[test]
fn return_leaves_the_enclosing_function() {
    let home = scratch("return");
    let out = sharky(
        &home,
        r#"f() { eval 'return 42'; echo unreached; }; f; echo "s=$?""#,
    );
    assert_eq!(stdout(&out), "s=42\n", "{out:?}");
}

#[test]
fn break_leaves_the_enclosing_loop() {
    let home = scratch("break");
    let out = sharky(
        &home,
        r#"for i in 1 2 3; do eval 'break'; echo loop; done; echo done"#,
    );
    assert_eq!(stdout(&out), "done\n", "{out:?}");
}

#[test]
fn exit_leaves_the_shell_with_its_status() {
    let home = scratch("exit");
    let out = sharky(&home, r#"eval 'echo start; exit 5'; echo unreached"#);
    assert_eq!(stdout(&out), "start\n", "{out:?}");
    assert_eq!(status(&out), 5);
}

#[test]
fn runs_inside_a_command_substitution() {
    let home = scratch("substitution");
    let out = sharky(&home, r#"x=$(eval 'echo sub'); echo "[$x]""#);
    assert_eq!(stdout(&out), "[sub]\n", "{out:?}");
}

#[test]
fn double_dash_is_skipped() {
    let home = scratch("dashdash");
    let out = sharky(&home, r#"eval -- echo hi"#);
    assert_eq!(stdout(&out), "hi\n", "{out:?}");
}

#[test]
fn login_profile_shape_works() {
    let home = scratch("profile");
    // The exact idiom the NixOS /etc/bashrc uses to pull in helper settings.
    let out = sharky(
        &home,
        r#"helper() { printf 'export MARKED=1\n'; }; eval "$(helper)"; echo "$MARKED""#,
    );
    assert_eq!(stdout(&out), "1\n", "{out:?}");
}
