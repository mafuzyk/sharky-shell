use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(source: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", source])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn assert_output(source: &str, expected: &str) {
    let out = sharky(source);
    assert_eq!(
        out.status.code(),
        Some(0),
        "source: {source:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected,
        "source: {source:?}"
    );
}

fn assert_rejected(source: &str) {
    let out = sharky(source);
    assert_eq!(
        out.status.code(),
        Some(2),
        "source: {source:?}; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected LParen"),
        "source: {source:?}; stderr: {stderr}"
    );
}

#[test]
fn local_with_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"f() { local -a a=(1 2); printf '%s' "${a[1]}"; }; f"#,
        "2",
    );
}

#[test]
fn local_without_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"f() { local a=(x y); printf '%s' "${a[0]}${a[1]}"; }; f"#,
        "xy",
    );
}

#[test]
fn readonly_with_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"readonly -a a=(p q); printf '%s' "${a[1]}""#,
        "q",
    );
}

#[test]
fn typeset_accepts_indexed_array_literal() {
    assert_output(r#"typeset a=(m n); printf '%s' "${a[0]}${a[1]}""#, "mn");
}

#[test]
fn declare_with_flag_still_accepts_indexed_array_literal() {
    assert_output(
        r#"declare -a a=(one two); printf '%s' "${a[1]}""#,
        "two",
    );
}

#[test]
fn declare_with_flag_still_accepts_associative_array_literal() {
    assert_output(
        r#"declare -A m=([k]=v); printf '%s' "${m[k]}""#,
        "v",
    );
}

#[test]
fn arbitrary_command_with_flag_rejects_array_literal() {
    assert_rejected("foo -a a=(1 2)");
}

#[test]
fn arbitrary_command_rejects_array_literal() {
    assert_rejected("foo a=(1 2)");
}
