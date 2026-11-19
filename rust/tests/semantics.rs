use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

// Parameter expansion: ${name-word} substitutes only for an unset parameter,
// while ${name:-word} also substitutes for a null one.

#[test]
fn unset_parameter_uses_the_default_word_without_a_colon() {
    let out = sharky(r#"unset SHARKY_UNSET_A; printf '[%s]' "${SHARKY_UNSET_A-default}""#);
    assert_eq!(stdout(&out), "[default]");
    assert_eq!(status(&out), 0);
}

#[test]
fn null_parameter_uses_the_default_word_with_a_colon() {
    let out = sharky(r#"SHARKY_NULL_A=; printf '[%s]' "${SHARKY_NULL_A:-fallback}""#);
    assert_eq!(stdout(&out), "[fallback]");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_parameter_without_a_colon_keeps_the_empty_value() {
    let out = sharky(r#"SHARKY_NULL_A=; printf '[%s]' "${SHARKY_NULL_A-default}""#);
    assert_eq!(stdout(&out), "[]");
    assert_eq!(status(&out), 0);
}

#[test]
fn set_parameter_without_a_colon_keeps_its_value() {
    let out = sharky(r#"SHARKY_SET_A=value; printf '[%s]' "${SHARKY_SET_A-default}""#);
    assert_eq!(stdout(&out), "[value]");
    assert_eq!(status(&out), 0);
}

#[test]
fn unset_parameter_assignment_without_a_colon_stores_the_word() {
    let out = sharky(
        r#"unset SHARKY_ASSIGN_A; printf '[%s][%s]' "${SHARKY_ASSIGN_A=made}" "$SHARKY_ASSIGN_A""#,
    );
    assert_eq!(stdout(&out), "[made][made]");
    assert_eq!(status(&out), 0);
}

#[test]
fn alternate_without_a_colon_expands_only_when_set() {
    let out = sharky(
        r#"unset SHARKY_UNSET_B; SHARKY_SET_B=v; printf '[%s][%s]' "${SHARKY_UNSET_B+yes}" "${SHARKY_SET_B+yes}""#,
    );
    assert_eq!(stdout(&out), "[][yes]");
    assert_eq!(status(&out), 0);
}

#[test]
fn unset_parameter_error_without_a_colon_aborts() {
    let out = sharky(r#"unset SHARKY_UNSET_C; printf '[%s]' "${SHARKY_UNSET_C?boom}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
}

// Unquoted expansions split their result on IFS; quoted expansions never do.

#[test]
fn unquoted_parameter_splits_on_ifs() {
    let out = sharky(
        r#"IFS=,; v=a,b,c; set -- $v; printf '%s' "$#"; printf '<%s>' "$@""#,
    );
    assert_eq!(stdout(&out), "3<a><b><c>");
    assert_eq!(status(&out), 0);
}

#[test]
fn quoted_parameter_does_not_split_on_ifs() {
    let out = sharky(r#"IFS=,; v=a,b; set -- "$v"; printf '%s' "$#"; printf '<%s>' "$@""#);
    assert_eq!(stdout(&out), "1<a,b>");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_disables_field_splitting() {
    let out = sharky(r#"IFS=; v='a b'; set -- $v; printf '%s' "$#"; printf '<%s>' "$@""#);
    assert_eq!(stdout(&out), "1<a b>");
    assert_eq!(status(&out), 0);
}

#[test]
fn ifs_keeps_empty_fields_between_repeated_delimiters() {
    let out = sharky(
        r#"IFS=,; v=a,,b; set -- $v; printf '%s' "$#"; printf '<%s>' "$@""#,
    );
    assert_eq!(stdout(&out), "3<a><><b>");
    assert_eq!(status(&out), 0);
}

#[test]
fn default_ifs_still_splits_on_whitespace() {
    let out = sharky(r#"v='a b'; set -- $v; printf '%s' "$#"; printf '<%s>' "$@""#);
    assert_eq!(stdout(&out), "2<a><b>");
    assert_eq!(status(&out), 0);
}

#[test]
fn literal_text_is_not_split_on_ifs() {
    let out = sharky(r#"IFS=,; printf '<%s>' a,b"#);
    assert_eq!(stdout(&out), "<a,b>");
    assert_eq!(status(&out), 0);
}
