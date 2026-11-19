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

// set: clustered option letters, pipefail, errexit and nounset.

#[test]
fn clustered_option_letters_enable_each_option() {
    let out = sharky("set -euo pipefail; set +o");
    let listing = stdout(&out);
    assert_eq!(status(&out), 0);
    assert!(listing.contains("set -o errexit"), "{listing:?}");
    assert!(listing.contains("set -o nounset"), "{listing:?}");
    assert!(listing.contains("set -o pipefail"), "{listing:?}");
}

#[test]
fn clustered_option_letters_only_enable_listed_options() {
    let out = sharky("set -eu; set +o");
    let listing = stdout(&out);
    assert_eq!(status(&out), 0);
    assert!(listing.contains("set -o errexit"), "{listing:?}");
    assert!(!listing.contains("set -o pipefail"), "{listing:?}");
    assert!(!listing.contains("set -o xtrace"), "{listing:?}");
}

#[test]
fn pipefail_reports_the_rightmost_failure_through_a_condition() {
    let out = sharky(
        r#"set -o pipefail; if false | true; then printf 'ok\n'; else printf 'rc=%s\n' "$?"; fi"#,
    );
    assert_eq!(stdout(&out), "rc=1\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_aborts_after_a_failing_simple_command() {
    let out = sharky("set -e; false; printf unreachable");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn errexit_is_suspended_inside_conditions_and_and_or_lists() {
    let out = sharky(
        "set -e; if false; then printf no; else printf cond; fi; false && printf no; false || printf or; printf end",
    );
    assert_eq!(stdout(&out), "condorend");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_aborts_on_the_last_command_of_an_and_list() {
    let out = sharky("set -e; true && false; printf unreachable");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn nounset_errors_on_an_unset_expansion() {
    let out = sharky(r#"set -u; printf '%s' "$SHARKY_UNSET_D"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound"));
}

// local: function-scoped variables with dynamic scope.

#[test]
fn local_restores_the_previous_value_on_return() {
    let out = sharky(r#"v=outer; f() { local v=inner; printf '%s;' "$v"; }; f; printf '%s' "$v""#);
    assert_eq!(stdout(&out), "inner;outer");
    assert_eq!(status(&out), 0);
}

#[test]
fn local_without_a_value_starts_empty() {
    let out = sharky(r#"v=outer; f() { local v; printf '[%s]' "$v"; }; f"#);
    assert_eq!(stdout(&out), "[]");
    assert_eq!(status(&out), 0);
}

#[test]
fn local_is_visible_to_called_functions() {
    let out = sharky(
        r#"g() { printf '%s;' "$v"; }; f() { local v=inner; g; }; v=outer; f; printf '%s' "$v""#,
    );
    assert_eq!(stdout(&out), "inner;outer");
    assert_eq!(status(&out), 0);
}

#[test]
fn local_unsets_a_previously_unset_binding_on_return() {
    let out = sharky(
        r#"unset SHARKY_NEW_A; f() { local SHARKY_NEW_A=x; printf '%s;' "$SHARKY_NEW_A"; }; f; printf '[%s]' "${SHARKY_NEW_A-unset}""#,
    );
    assert_eq!(stdout(&out), "x;[unset]");
    assert_eq!(status(&out), 0);
}

#[test]
fn local_outside_a_function_fails() {
    let out = sharky("local SHARKY_OUT_A=1");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("local"));
}

#[test]
fn local_rejects_an_invalid_name() {
    let out = sharky("f() { local 1bad=x; }; f");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("local"));
}

#[test]
fn local_rejects_an_unsupported_option() {
    let out = sharky("f() { local -r v=1; }; f");
    assert_eq!(status(&out), 2);
    assert!(String::from_utf8_lossy(&out.stderr).contains("local"));
}

#[test]
fn local_restores_a_shadowed_array_on_return() {
    let out = sharky(
        r#"declare -a a; a[0]=outer; f() { local -a a; a[0]=inner; printf '%s;' "${a[0]}"; }; f; printf '%s' "${a[0]}""#,
    );
    assert_eq!(stdout(&out), "inner;outer");
    assert_eq!(status(&out), 0);
}
