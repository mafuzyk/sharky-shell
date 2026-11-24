use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-syntax-check-config";

fn sharky(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn sharky_with_env(home: &Path, config: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", config)
        .env("HOME", home)
        .output()
        .expect("failed to run sharky")
}

fn temp_dir(tag: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("sharky-syntax-check-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn stderr(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        !text.contains("unknown option: --check"),
        "--check was not recognized: {text}"
    );
    text.into_owned()
}

fn assert_silent(out: &Output) {
    assert!(
        out.status.success(),
        "status: {:?}, stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn assert_rejected(args: &[&str], hints: &[&str]) {
    let out = sharky(args);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(2), "args: {args:?}; stderr: {text}");
    assert!(
        hints.iter().any(|hint| text.contains(hint)),
        "expected one of {hints:?} in stderr: {text}"
    );
}

#[test]
fn check_command_accepts_valid_source_silently() {
    assert_silent(&sharky(&["--check", "-c", "true"]));
}

#[test]
fn check_command_does_not_execute_valid_source() {
    let dir = temp_dir("no-exec");
    let marker = dir.join("ran");
    let source = format!("touch {}", marker.display());

    assert_silent(&sharky(&["--check", "-c", &source]));
    assert!(!marker.exists(), "check executed the command");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_command_reports_line_for_invalid_top_level_syntax() {
    let out = sharky(&["--check", "-c", "true\ntrue\nprintf \"x\n"]);
    assert_eq!(out.status.code(), Some(2));
    let text = stderr(&out);
    assert!(text.contains("syntax error"), "{text}");
    assert!(text.contains('3'), "missing line info: {text}");
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn check_file_accepts_valid_source_silently() {
    let dir = temp_dir("valid-file");
    let path = write_file(&dir, "valid.sharky", "printf SHOULD_NOT_PRINT\ntrue\n");

    assert_silent(&sharky(&["--check", path.to_str().unwrap()]));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_file_reports_line_for_invalid_top_level_syntax() {
    let dir = temp_dir("bad-file");
    let path = write_file(&dir, "bad.sharky", "true\nprintf \"x\n");

    let out = sharky(&["--check", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(2));
    let text = stderr(&out);
    assert!(text.contains("syntax error"), "{text}");
    assert!(text.contains('2'), "missing line info: {text}");
    assert!(
        out.stdout.is_empty(),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_without_a_source_is_rejected() {
    assert_rejected(&["--check"], &["check", "source", "-c"]);
}

#[test]
fn check_rejects_interactive_flag() {
    assert_rejected(&["--check", "-i", "-c", "true"], &["-i", "interactive"]);
}

#[test]
fn check_rejects_login_flag() {
    assert_rejected(&["--check", "-l", "-c", "true"], &["-l", "login"]);
}

#[test]
fn check_rejects_json_flag() {
    assert_rejected(&["--check", "--json", "-c", "true"], &["json"]);
}

#[test]
fn check_rejects_state_command() {
    assert_rejected(&["--check", "state", "--json"], &["state"]);
}

#[test]
fn check_does_not_load_configuration() {
    let dir = temp_dir("config");
    write_file(&dir, "config.sharky", "bogus anything value\n");

    assert_silent(&sharky_with_env(&dir, &dir, &["--check", "-c", "true"]));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_does_not_load_profiles_or_touch_history() {
    let home = temp_dir("home");
    let profile_marker = home.join("profile-ran");
    let startup_marker = home.join("startup-ran");
    std::fs::write(
        home.join(".profile"),
        format!("touch {}\n", profile_marker.display()),
    )
    .unwrap();
    let sharky_profile = home.join(".config/sharky/profile.sharky");
    std::fs::create_dir_all(sharky_profile.parent().unwrap()).unwrap();
    std::fs::write(
        sharky_profile,
        format!(")\ntouch {}\n", startup_marker.display()),
    )
    .unwrap();

    let out = sharky_with_env(&home, Path::new(NO_CONFIG), &["--check", "-c", "true"]);
    assert_silent(&out);
    assert!(!profile_marker.exists(), "check loaded a profile");
    assert!(!startup_marker.exists(), "check loaded a profile");

    std::fs::remove_dir_all(&home).ok();
}

#[test]
fn coproc_without_command_is_rejected_as_syntax_error() {
    assert_rejected(&["-c", "coproc"], &["coproc"]);
    assert_rejected(&["--check", "-c", "coproc"], &["coproc"]);

    assert_silent(&sharky(&["--check", "-c", "coproc echo hi"]));

    let literal = sharky(&["-c", "echo coproc"]);
    assert_eq!(literal.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&literal.stdout), "coproc\n");
}
