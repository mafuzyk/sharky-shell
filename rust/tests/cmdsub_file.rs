//! Regression tests for the `$(<file)` fast substitution (bash's
//! "read a file without spawning a command" shortcut).
//!
//! Expectations were measured against bash 5.3.20 with
//! `env -i PATH=... LC_ALL=C.UTF-8 bash --noprofile --norc -c`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";
const PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sharky-cmdsub-file-{}-{n}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_in(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", PATH)
        .env("LC_ALL", "C.UTF-8")
        .current_dir(dir)
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
fn basic_content_is_read_and_trailing_newlines_are_stripped() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "alpha\nbeta\n\n").unwrap();
    let out = sharky_in(&dir, "x=$(<f); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[alpha\nbeta] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn file_without_trailing_newline_is_kept_verbatim() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "no-newline").unwrap();
    let out = sharky_in(&dir, "x=$(<f); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[no-newline] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn empty_file_yields_empty_result_and_status_zero() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "").unwrap();
    let out = sharky_in(&dir, "x=$(<f); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn missing_file_reports_status_stderr_and_empty_result() {
    let dir = temp_dir();
    let out = sharky_in(&dir, "x=$(<missing); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[] status=1\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("missing"),
        "expected the path in stderr, got {:?}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("No such file or directory"),
        "expected an open error in stderr, got {:?}",
        stderr(&out)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn path_from_an_unquoted_variable_is_expanded() {
    let dir = temp_dir();
    std::fs::write(dir.join("data"), "alpha\nbeta\n").unwrap();
    let out = sharky_in(
        &dir,
        "f=data; x=$(<$f); printf '[%s] status=%s\\n' \"$x\" \"$?\"",
    );
    assert_eq!(stdout(&out), "[alpha\nbeta] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn path_from_a_quoted_variable_is_expanded() {
    let dir = temp_dir();
    std::fs::write(dir.join("data"), "alpha\nbeta\n").unwrap();
    let out = sharky_in(
        &dir,
        "g=data; x=$(<\"$g\"); printf '[%s] status=%s\\n' \"$x\" \"$?\"",
    );
    assert_eq!(stdout(&out), "[alpha\nbeta] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn space_after_the_redirect_operator_is_accepted() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "alpha\nbeta\n").unwrap();
    let out = sharky_in(&dir, "x=$(< f); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[alpha\nbeta] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn tilde_in_the_path_is_expanded() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "from-home\n").unwrap();
    let home = dir.to_string_lossy();
    let script = format!(
        "HOME={home}; x=$(<~/f); printf '[%s] status=%s\\n' \"$x\" \"$?\""
    );
    let out = sharky_in(&dir, &script);
    assert_eq!(stdout(&out), "[from-home] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn cat_with_an_input_redirect_still_runs_the_command() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "alpha\nbeta\n").unwrap();
    let out = sharky_in(
        &dir,
        "x=$(cat <f); printf '[%s] status=%s\\n' \"$x\" \"$?\"",
    );
    assert_eq!(stdout(&out), "[alpha\nbeta] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

// A command word makes this ordinary command substitution: the file is only a
// redirect source. If the fast path ever swallowed it, this would print the
// file content with status 0 instead of running the (missing) command.
#[test]
fn a_command_word_keeps_normal_substitution_semantics() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), "alpha\nbeta\n").unwrap();
    let out = sharky_in(
        &dir,
        "x=$(<f no_such_command_xyz); printf '[%s] status=%s\\n' \"$x\" \"$?\"",
    );
    assert_eq!(stdout(&out), "[] status=127\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("command not found"),
        "expected command execution, got {:?}",
        stderr(&out)
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_nul_byte_in_the_file_is_discarded_with_a_warning() {
    let dir = temp_dir();
    std::fs::write(dir.join("f"), [b'a', 0, b'b', b'\n']).unwrap();
    let out = sharky_in(&dir, "x=$(<f); printf '[%s] status=%s\\n' \"$x\" \"$?\"");
    assert_eq!(stdout(&out), "[ab] status=0\n", "stderr: {:?}", stderr(&out));
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("ignored null byte in input"),
        "expected a NUL-byte warning, got {:?}",
        stderr(&out)
    );
    std::fs::remove_dir_all(&dir).ok();
}
