//! `set -C` (noclobber) and the `>|` override operator, checked against bash
//! 5.3.20. Each test's comment names the measured bash result.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "sharky-noclobber-{tag}-{}-{count}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    dir
}

fn sharky_in(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .current_dir(dir)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn run(script: &str) -> (Output, PathBuf) {
    let dir = temp_dir("run");
    let out = sharky_in(&dir, script);
    (out, dir)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_else(|err| panic!("read {name}: {err}"))
}

#[test]
fn refuses_to_truncate_an_existing_regular_file() {
    // bash: status 1, `f: cannot overwrite existing file`, f keeps "a".
    let (out, dir) = run("printf a > f; set -C; echo b > f");
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot overwrite existing file"),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(read(&dir, "f"), "a");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn force_operator_overrides_noclobber() {
    // bash: status 0, f == "b".
    let (out, dir) = run("printf a > f; set -C; echo b >| f");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(read(&dir, "f"), "b\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn append_is_not_affected() {
    // bash: status 0, f == "ab".
    let (out, dir) = run("printf a > f; set -C; printf b >> f");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stderr(&out), "");
    assert_eq!(read(&dir, "f"), "ab");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_new_file_is_created() {
    // bash: status 0, f == "b".
    let (out, dir) = run("set -C; echo b > f");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stderr(&out), "");
    assert_eq!(read(&dir, "f"), "b\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_null_device_stays_writable() {
    // bash: a non-regular target is not protected; status 0, prints "ok".
    let (out, _dir) = run("set -C; echo b > /dev/null; echo ok");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn every_descriptor_is_protected() {
    // bash: noclobber covers `2>` too; status 1, f keeps "a".
    let (out, dir) = run("printf a > f; set -C; echo hi 2> f; echo st=$?");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "st=1\n");
    assert_eq!(read(&dir, "f"), "a");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn set_plus_c_restores_clobbering() {
    // bash: status 0, f == "b".
    let (out, dir) = run("printf a > f; set -C; set +C; echo b > f");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stderr(&out), "");
    assert_eq!(read(&dir, "f"), "b\n");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_named_option_enables_and_disables_it() {
    // bash: `-o noclobber` refuses; `+o noclobber` turns it back off.
    let (out, dir) = run("printf a > f; set -o noclobber; echo b > f");
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert_eq!(read(&dir, "f"), "a");

    let (out, dir2) = run("printf a > f; set -o noclobber; set +o noclobber; echo b > f");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(read(&dir2, "f"), "b\n");
    std::fs::remove_dir_all(&dir).ok();
    std::fs::remove_dir_all(&dir2).ok();
}

#[test]
fn the_named_option_is_listed_as_active() {
    // bash lists the option while it is on.
    let (out, _dir) = run("set -C; set +o");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("set -o noclobber"),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn a_symlink_to_a_regular_file_is_protected() {
    // bash: the resolved target is a regular file, so the write is refused.
    let (out, dir) = run("printf a > t; ln -s t l; set -C; echo b > l");
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert_eq!(read(&dir, "t"), "a");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_symlink_to_a_device_is_not_protected() {
    // bash: the resolved target is not a regular file; status 0.
    let (out, _dir) = run("ln -s /dev/null l; set -C; echo b > l; echo ok");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
}

#[test]
fn a_dangling_symlink_is_refused() {
    // bash: `l: cannot overwrite existing file`; the missing target is not created.
    let (out, dir) = run("ln -s missing l; set -C; echo b > l");
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("cannot overwrite existing file"),
        "stderr: {}",
        stderr(&out)
    );
    assert!(!dir.join("missing").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn heredocs_still_work() {
    // bash: the heredoc temp file is internal and unaffected; status 0.
    let (out, _dir) = run("set -C; cat <<EOF\nhi\nEOF");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}

#[test]
fn pipes_still_work() {
    // bash: a pipe is not a file target; status 0.
    let (out, _dir) = run("set -C; echo hi | cat");
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}
