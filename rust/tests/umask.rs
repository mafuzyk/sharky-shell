//! `umask`: display and set the file-creation mask.
//!
//! Expected bytes and statuses were measured against bash 5.3.20. The mask is a
//! process attribute, so the tests that matter most prove it reaches files the
//! shell creates and processes it starts.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-umask-test";
const TOOL_PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-umask-builtin-{}-{name}",
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
        .env("PATH", TOOL_PATH)
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
fn prints_the_mask_as_four_digit_octal() {
    let home = scratch("print");
    let out = sharky(&home, "umask 0022; umask");
    assert_eq!(stdout(&out), "0022\n", "{out:?}");
    assert_eq!(status(&out), 0);
}

#[test]
fn sets_an_octal_mask() {
    let home = scratch("octal");
    let out = sharky(&home, "umask 027; umask");
    assert_eq!(stdout(&out), "0027\n", "{out:?}");
}

#[test]
fn short_octal_is_padded_in_the_report() {
    let home = scratch("short");
    let out = sharky(&home, "umask 22; umask");
    assert_eq!(stdout(&out), "0022\n", "{out:?}");
}

#[test]
fn prints_the_symbolic_form() {
    let home = scratch("symbolic-print");
    let out = sharky(&home, "umask 0022; umask -S");
    assert_eq!(stdout(&out), "u=rwx,g=rx,o=rx\n", "{out:?}");
    let out = sharky(&home, "umask 027; umask -S");
    assert_eq!(stdout(&out), "u=rwx,g=rx,o=\n", "{out:?}");
}

#[test]
fn sets_a_symbolic_mask() {
    let home = scratch("symbolic-set");
    let out = sharky(&home, "umask u=rwx,g=rx,o=; umask");
    assert_eq!(stdout(&out), "0027\n", "{out:?}");
}

#[test]
fn symbolic_copy_reads_the_current_mask() {
    let home = scratch("copy");
    let out = sharky(&home, "umask 077; umask g=u; umask");
    assert_eq!(stdout(&out), "0007\n", "{out:?}");
}

#[test]
fn capital_x_needs_an_existing_execute_bit() {
    let home = scratch("capital-x");
    // With 077 the owner may execute, so `a+X` adds the execute bits and the
    // mask loses them: 077 -> ~770 = 007, then 0666 clears x everywhere.
    let out = sharky(&home, "umask 077; umask a+X; umask");
    assert_eq!(stdout(&out), "0066\n", "{out:?}");
    let out = sharky(&home, "umask 077; umask a=x; umask");
    assert_eq!(stdout(&out), "0666\n", "{out:?}");
}

#[test]
fn capital_x_is_ignored_when_no_execute_bit_is_allowed() {
    let home = scratch("capital-x-off");
    // With 0777 nothing may execute, so `a+X` is a no-op.
    let out = sharky(&home, "umask 0777; umask a+X; umask");
    assert_eq!(stdout(&out), "0777\n", "{out:?}");
}

#[test]
fn invalid_octal_digit_is_rejected() {
    let home = scratch("bad-octal-digit");
    let out = sharky(&home, "umask 999; echo \"s=$?\"; umask");
    assert_eq!(stdout(&out), "s=1\n0022\n", "{out:?}");
    assert!(
        stderr(&out).contains("octal number out of range"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn out_of_range_octal_is_rejected() {
    let home = scratch("bad-octal-range");
    let out = sharky(&home, "umask 0022; umask 10000; echo \"s=$?\"; umask");
    assert_eq!(stdout(&out), "s=1\n0022\n", "{out:?}");
    assert!(stderr(&out).contains("octal number out of range"), "{out:?}");
}

#[test]
fn invalid_symbolic_character_is_rejected() {
    let home = scratch("bad-symbolic-char");
    let out = sharky(&home, "umask 0022; umask u+z; echo \"s=$?\"; umask");
    assert_eq!(stdout(&out), "s=1\n0022\n", "{out:?}");
    assert!(
        stderr(&out).contains("invalid symbolic mode character"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn missing_symbolic_operator_is_rejected() {
    let home = scratch("bad-symbolic-op");
    let out = sharky(&home, "umask 0022; umask u; echo \"s=$?\"; umask");
    assert_eq!(stdout(&out), "s=1\n0022\n", "{out:?}");
    assert!(
        stderr(&out).contains("invalid symbolic mode operator"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn invalid_option_reports_usage_and_status_two() {
    let home = scratch("bad-option");
    let out = sharky(&home, "umask -Z; echo \"s=$?\"");
    assert_eq!(stdout(&out), "s=2\n", "{out:?}");
    assert!(stderr(&out).contains("usage: umask"), "{:?}", stderr(&out));
}

#[test]
fn p_flag_prints_a_reusable_command() {
    let home = scratch("p-flag");
    let out = sharky(&home, "umask 0022; umask -p");
    assert_eq!(stdout(&out), "umask 0022\n", "{out:?}");
}

#[test]
fn p_and_s_combined_print_a_reusable_symbolic_command() {
    let home = scratch("ps-flag");
    let out = sharky(&home, "umask 0022; umask -pS");
    assert_eq!(stdout(&out), "umask -S u=rwx,g=rx,o=rx\n", "{out:?}");
}

#[test]
fn mask_is_inherited_by_child_processes() {
    let home = scratch("inherit");
    let out = sharky(&home, "umask 077; sh -c umask");
    assert_eq!(stdout(&out), "0077\n", "{out:?}");
}

#[test]
fn mask_applies_to_files_the_shell_creates() {
    let home = scratch("files");
    let out = sharky(&home, "cd \"$HOME\"; umask 077; touch made; stat -c %a made");
    assert_eq!(stdout(&out), "600\n", "{out:?}");
    let mode = std::fs::metadata(home.join("made"))
        .expect("touch must create the file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn setting_with_s_prints_the_new_symbolic_mask() {
    let home = scratch("s-with-mode");
    let out = sharky(&home, "umask 0022; umask -S 027");
    assert_eq!(stdout(&out), "u=rwx,g=rx,o=\n", "{out:?}");
}
