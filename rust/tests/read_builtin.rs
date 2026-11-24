//! `read` option parsing: `-r`, `-n`/`-N`, `-a`, `-t`, `-d`, `-p` and the
//! `REPLY` fallback. Expected values were measured against bash 5.3.20.
//!
//! The timeout status is pinned to `142`: bash returns `128 + SIGALRM`, and the
//! shell adopts the same number so scripts that test `> 128` keep working.

use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const NO_CONFIG: &str = "/nonexistent/sharky-read-test";

fn sharky_with_stdin(source: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", source])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run sharky");
    if let Err(err) = child
        .stdin
        .take()
        .expect("sharky stdin")
        .write_all(input.as_bytes())
        && err.kind() != std::io::ErrorKind::BrokenPipe
    {
        panic!("failed to write sharky stdin: {err}");
    }
    child.wait_with_output().expect("failed to collect output")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn assert_read(source: &str, input: &str, expected: &str) {
    let out = sharky_with_stdin(source, input);
    assert_eq!(
        out.status.code(),
        Some(0),
        "source {source:?}: {}",
        stderr(&out)
    );
    assert_eq!(stdout(&out), expected, "source {source:?}; {out:?}");
}

#[test]
fn dash_n_reads_one_character() {
    assert_read(
        r#"read -n 1 x; printf '<%s|%s>' "$x" "$?""#,
        "abc\n",
        "<a|0>",
    );
}

#[test]
fn attached_option_value_is_accepted() {
    assert_read(
        r#"read -n1 x; printf '<%s|%s>' "$x" "$?""#,
        "abc\n",
        "<a|0>",
    );
}

#[test]
fn dash_n_succeeds_when_the_count_is_reached_before_eof() {
    assert_read(
        r#"read -n 2 x; printf '<%s|%s>' "$x" "$?""#,
        "abc",
        "<ab|0>",
    );
}

#[test]
fn dash_n_fails_when_eof_arrives_before_the_count() {
    assert_read(
        r#"read -n 5 x; printf '<%s|%s>' "$x" "$?""#,
        "ab",
        "<ab|1>",
    );
}

#[test]
fn dash_n_counts_characters_not_bytes() {
    assert_read(
        r#"read -n 2 x; printf '<%s|%s>' "$x" "${#x}""#,
        "héllo\n",
        "<hé|2>",
    );
}

#[test]
fn dash_capital_n_ignores_the_delimiter() {
    assert_read(
        r#"read -N 5 x; printf '<%s>' "${#x}""#,
        "abcd\n",
        "<5>",
    );
}

#[test]
fn dash_capital_n_fails_at_eof() {
    assert_read(
        r#"read -N 5 x; printf '<%s|%s>' "$x" "$?""#,
        "ab",
        "<ab|1>",
    );
}

#[test]
fn dash_a_splits_into_an_array() {
    assert_read(
        r#"read -a arr; printf '<%s|%s|%s>' "${#arr[@]}" "${arr[0]}" "${arr[2]}""#,
        "a b c\nd e\n",
        "<3|a|c>",
    );
}

#[test]
fn dash_a_honours_ifs() {
    assert_read(
        r#"IFS=: read -a arr; printf '<%s|%s|%s|%s|%s>' "${#arr[@]}" "${arr[0]}" "${arr[1]}" "${arr[2]}" "${arr[3]}""#,
        "a:b::c\n",
        "<4|a|b||c>",
    );
}

#[test]
fn dash_a_keeps_empty_fields_from_consecutive_delimiters() {
    assert_read(
        r#"IFS=: read -a arr; printf '<%s|%s|%s>' "${#arr[@]}" "${arr[0]}" "${arr[1]}""#,
        "a::\n",
        "<2|a|>",
    );
    assert_read(
        r#"IFS=: read -a arr; printf '<%s>' "${#arr[@]}""#,
        "a:\n",
        "<1>",
    );
}

#[test]
fn combined_flags_are_parsed_together() {
    assert_read(
        r#"read -rn1 x; printf '<%s|%s>' "$x" "$?""#,
        "abc\n",
        "<a|0>",
    );
}

#[test]
fn dash_d_uses_a_custom_delimiter() {
    assert_read(
        r#"read -d : x; printf '<%s|%s>' "$x" "$?""#,
        "ab:cd\n",
        "<ab|0>",
    );
}

#[test]
fn dash_p_is_silent_when_stdin_is_not_a_terminal() {
    assert_read(
        r#"read -p 'PROMPT>' x; printf '<%s>' "$x""#,
        "abc\n",
        "<abc>",
    );
}

#[test]
fn no_names_stores_the_input_in_reply() {
    assert_read(
        r#"read -n 1; printf '<%s|%s>' "$REPLY" "${#REPLY}""#,
        "abc\n",
        "<a|1>",
    );
}

#[test]
fn invalid_option_reports_usage_and_status_2() {
    let out = sharky_with_stdin(r#"read -z x; printf '<%s>' "$?""#, "abc\n");
    assert_eq!(stdout(&out), "<2>", "{out:?}");
    let err = stderr(&out);
    assert!(
        err.contains("read: -z: invalid option"),
        "missing diagnostic: {err:?}"
    );
    assert!(err.contains("usage"), "missing usage: {err:?}");
}

#[test]
fn timeout_status_is_above_128() {
    // The input side stays open for two seconds and never delivers a byte, so
    // a shell without `-t` support blocks until EOF and reports status 1.
    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args([
            "--no-config",
            "-c",
            r#"read -t 0.2 x; printf '<%s|%s>' "$x" "$?""#,
        ])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to run sharky");
    let stdin = child.stdin.take().expect("sharky stdin");
    let released = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(2));
        drop(stdin);
    });

    let started = Instant::now();
    let out = child.wait_with_output().expect("failed to collect output");
    let elapsed = started.elapsed();
    released.join().unwrap();

    assert_eq!(stdout(&out), "<|142>", "{out:?}");
    assert!(
        elapsed < Duration::from_millis(1500),
        "the builtin waited for EOF instead of the timeout: {elapsed:?}"
    );
}

#[test]
fn timeout_reports_eof_immediately_on_closed_input() {
    let out = sharky_with_stdin(
        r#"read -t 0.2 x; printf '<%s|%s>' "$x" "$?""#,
        "",
    );
    assert_eq!(stdout(&out), "<|1>", "{out:?}");
}

#[test]
fn overflowing_timeout_is_rejected_without_panicking() {
    let out = sharky_with_stdin(
        r#"read -t 1e300 x <<< hi; echo "st=$?"; echo alive"#,
        "",
    );
    assert_eq!(out.status.code(), Some(0), "shell died: {out:?}");
    assert_eq!(stdout(&out), "st=1\nalive\n", "{out:?}");
    assert!(
        stderr(&out).contains("read: 1e300: invalid timeout"),
        "missing diagnostic: {:?}",
        stderr(&out)
    );
}

#[test]
fn infinite_timeout_is_rejected_without_panicking() {
    let out = sharky_with_stdin(
        r#"read -t 1e400 x <<< hi; echo "st=$?"; echo alive"#,
        "",
    );
    assert_eq!(out.status.code(), Some(0), "shell died: {out:?}");
    assert_eq!(stdout(&out), "st=1\nalive\n", "{out:?}");
    assert!(
        stderr(&out).contains("read: 1e400: invalid timeout"),
        "missing diagnostic: {:?}",
        stderr(&out)
    );
}

#[test]
fn representable_timeout_still_works() {
    assert_read(r#"read -t 1 x <<< hi; echo "$x:$?""#, "", "hi:0\n");
}
