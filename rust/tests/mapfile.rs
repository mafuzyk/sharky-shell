use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn temp_dir() -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sharky-mapfile-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_in(dir: &Path, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
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

fn run_with_input(name: &str, bytes: &[u8], script: &str) -> Output {
    let dir = temp_dir();
    std::fs::write(dir.join(name), bytes).unwrap();
    let out = sharky_in(&dir, script);
    std::fs::remove_dir_all(&dir).ok();
    out
}

#[test]
fn mapfile_t_reads_lines_from_a_parent_redirect() {
    let out = run_with_input(
        "input",
        b"alpha\nbeta\n",
        r#"mapfile -t arr < input; printf '%s;' "${#arr[@]}" "${arr[0]}" "${arr[1]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2;alpha;beta;");
}

#[test]
fn mapfile_defaults_to_the_mapfile_array() {
    let out = run_with_input(
        "input",
        b"x\ny\n",
        r#"mapfile -t < input; printf '%s;' "${#MAPFILE[@]}" "${MAPFILE[0]}" "${MAPFILE[1]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2;x;y;");
}

#[test]
fn mapfile_replaces_an_existing_array_with_empty_on_empty_input() {
    let out = run_with_input(
        "input",
        b"",
        r#"arr=(a b c); mapfile -t arr < input; printf '%s' "${#arr[@]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0");
}

#[test]
fn mapfile_preserves_blank_records() {
    let out = run_with_input(
        "input",
        b"a\n\nb\n",
        r#"mapfile -t arr < input; printf '%s;' "${#arr[@]}" "[${arr[0]}]" "[${arr[1]}]" "[${arr[2]}]""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3;[a];[];[b];");
}

#[test]
fn mapfile_keeps_backslashes_literal() {
    let out = run_with_input(
        "input",
        b"a\\b\nc\\\\d\n",
        r#"mapfile -t arr < input; printf '%s;' "${arr[0]}" "${arr[1]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "a\\b;c\\\\d;");
}

#[test]
fn mapfile_t_keeps_the_carriage_return_of_crlf_records() {
    let out = run_with_input(
        "input",
        b"a\r\nb\r\n",
        r#"mapfile -t arr < input; printf '%s' "${#arr[0]},${#arr[1]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2,2");
}

#[test]
fn mapfile_includes_a_final_unterminated_line() {
    let out = run_with_input(
        "input",
        b"one\ntwo",
        r#"mapfile -t arr < input; printf '%s;' "${#arr[@]}" "${arr[0]}" "${arr[1]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2;one;two;");
}

#[test]
fn mapfile_without_t_keeps_the_trailing_newline() {
    let out = run_with_input(
        "input",
        b"x\n",
        r#"mapfile arr < input; printf '%s' "${#arr[0]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2");
}

#[test]
fn mapfile_invalid_option_returns_two() {
    let out = sharky_in(
        &temp_dir(),
        r#"mapfile -z arr < /dev/null; printf '%s' "$?""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2");
    assert!(stderr(&out).contains("invalid option"), "stderr: {}", stderr(&out));
}

#[test]
fn mapfile_invalid_name_returns_one() {
    let out = sharky_in(
        &temp_dir(),
        r#"mapfile 'bad-name' < /dev/null; printf '%s' "$?""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1");
    assert!(stderr(&out).contains("invalid"), "stderr: {}", stderr(&out));
}

#[test]
fn mapfile_associative_target_returns_one() {
    let out = sharky_in(
        &temp_dir(),
        r#"declare -A m; mapfile m < /dev/null; printf '%s' "$?""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1");
    assert!(stderr(&out).contains("indexed"), "stderr: {}", stderr(&out));
}

#[test]
fn readarray_is_an_alias_for_mapfile() {
    let out = run_with_input(
        "input",
        b"x\n",
        r#"readarray -t arr < input; printf '%s' "${arr[0]}""#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "x");
}

#[test]
fn read_escaped_space_semantics_are_unchanged() {
    let out = sharky_in(
        &temp_dir(),
        r#"printf 'a\\ b\n' | { read x; printf '%s' "$x"; }"#,
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "a b");
}
