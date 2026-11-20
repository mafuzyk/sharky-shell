use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

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

fn temp_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sharky-semantics-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky_script(source: &str) -> Output {
    let dir = temp_dir();
    let path = dir.join("script.sh");
    std::fs::write(&path, source).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config"])
        .arg(&path)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky");
    std::fs::remove_dir_all(&dir).ok();
    out
}

fn sharky_stdin(source: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn sharky");
    child
        .stdin
        .as_mut()
        .expect("sharky stdin")
        .write_all(source.as_bytes())
        .expect("failed to write sharky stdin");
    child.wait_with_output().expect("failed to run sharky")
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
    assert_eq!(status(&out), 127);
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
    assert_eq!(status(&out), 127);
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

// errexit: suppression covers every command of a condition, including the last
// command of an and-or chain and a failing function called as a condition.

#[test]
fn errexit_stays_suppressed_in_the_last_command_of_a_while_condition() {
    let out = sharky("set -e; while false || false; do printf body; done; printf after");
    assert_eq!(stdout(&out), "after");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_stays_suppressed_in_the_last_command_of_an_until_condition() {
    let out = sharky("set -e; until false || true; do printf body; done; printf after");
    assert_eq!(stdout(&out), "after");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_stays_suppressed_when_a_function_condition_fails() {
    let out = sharky("f() { false || false; }; set -e; while f; do :; done; printf after");
    assert_eq!(stdout(&out), "after");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_stays_suppressed_in_the_last_command_of_an_if_condition() {
    let out = sharky("set -e; if false || false; then :; else printf else; fi; printf after");
    assert_eq!(stdout(&out), "elseafter");
    assert_eq!(status(&out), 0);
}

// "$*" and "${a[*]}" join with the first character of IFS, with nothing when
// IFS is empty.

#[test]
fn quoted_array_elements_join_with_ifs_first_char() {
    let out = sharky(r#"IFS=:; a=(x y); printf '%s' "${a[*]}""#);
    assert_eq!(stdout(&out), "x:y");
    assert_eq!(status(&out), 0);
}

#[test]
fn unquoted_array_elements_join_with_ifs_first_char() {
    let out = sharky(r#"IFS=:; a=('x y' z); set -- ${a[*]}; printf '<%s>' "$@""#);
    assert_eq!(stdout(&out), "<x y><z>");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_joins_positional_parameters_with_nothing() {
    let out = sharky(r#"IFS=; set -- a b; printf '%s' "$*""#);
    assert_eq!(stdout(&out), "ab");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_joins_array_elements_with_nothing() {
    let out = sharky(r#"IFS=; a=(x y); printf '%s' "${a[*]}""#);
    assert_eq!(stdout(&out), "xy");
    assert_eq!(status(&out), 0);
}

// "${!a[*]}" and "${!a[@]}" expand the array keys. The star form joins them
// with the first character of IFS and the at form keeps one word per key, as
// element expansion does. bash keeps an unquoted star join and a scalar at
// join apart with a space when IFS is empty.

#[test]
fn quoted_array_keys_join_with_ifs_first_char() {
    let out = sharky(r#"IFS=:; a=(x y); printf '%s' "${!a[*]}""#);
    assert_eq!(stdout(&out), "0:1");
    assert_eq!(status(&out), 0);
}

#[test]
fn quoted_array_keys_keep_one_word_per_key_with_at() {
    let out = sharky(r#"IFS=:; a=(x y); printf '<%s>' "${!a[@]}""#);
    assert_eq!(stdout(&out), "<0><1>");
    assert_eq!(status(&out), 0);
}

#[test]
fn unquoted_array_keys_join_then_split_on_ifs() {
    let out = sharky(r#"IFS=:; a=(x y); set -- ${!a[*]}; printf '%s|' "$@""#);
    assert_eq!(stdout(&out), "0|1|");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_joins_quoted_array_keys_with_nothing() {
    let out = sharky(r#"IFS=; a=(x y); printf '%s' "${!a[*]}""#);
    assert_eq!(stdout(&out), "01");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_keeps_joined_array_keys_in_one_word() {
    let out = sharky(r#"IFS=; a=(x y); for k in "${!a[*]}"; do printf '[%s]' "$k"; done"#);
    assert_eq!(stdout(&out), "[01]");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_keeps_unquoted_array_keys_in_one_word() {
    let out =
        sharky(r#"IFS=; a=(x y); set -- ${!a[*]}; printf '%s|' "$@"; printf '=%s' "$#""#);
    assert_eq!(stdout(&out), "0 1|=1");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_still_splits_unquoted_keys_with_at() {
    let out =
        sharky(r#"IFS=; a=(x y); set -- ${!a[@]}; printf '%s|' "$@"; printf '=%s' "$#""#);
    assert_eq!(stdout(&out), "0|1|=2");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_keeps_unquoted_array_elements_as_separate_words() {
    let out =
        sharky(r#"IFS=; a=(x y); set -- ${a[*]}; printf '<%s>' "$@"; printf '=%s' "$#""#);
    assert_eq!(stdout(&out), "<x><y>=2");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_quoted_array_keys_join_with_ifs_first_char() {
    let out = sharky(r#"IFS=:; a=(x y); v="${!a[*]}"; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0:1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_unquoted_array_keys_join_with_ifs_first_char() {
    let out = sharky(r#"IFS=:; a=(x y); v=${!a[*]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0:1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn associative_keys_join_with_ifs_first_char() {
    let out = sharky(
        r#"IFS=:; declare -A m=([k1]=v1 [k2]=v2); printf '%s' "${!m[*]}""#,
    );
    assert_eq!(stdout(&out), "k1:k2");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_associative_keys_join_with_ifs_first_char() {
    let out = sharky(
        r#"IFS=:; declare -A m=([k1]=v1 [k2]=v2); v="${!m[*]}"; printf '[%s]' "$v""#,
    );
    assert_eq!(stdout(&out), "[k1:k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_keeps_unquoted_associative_keys_in_one_word() {
    let out = sharky(
        r#"IFS=; declare -A m=([k1]=v1 [k2]=v2); set -- ${!m[*]}; printf '%s|' "$@"; printf '=%s' "$#""#,
    );
    assert_eq!(stdout(&out), "k1 k2|=1");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_ifs_joins_assigned_quoted_keys_with_a_space() {
    let out = sharky(r#"IFS=; a=(x y); v="${!a[@]}"; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0 1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn default_ifs_joins_array_keys_with_a_space() {
    let out = sharky(r#"a=(x y); printf '[%s]' "${!a[*]}""#);
    assert_eq!(stdout(&out), "[0 1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_array_has_no_keys() {
    let out = sharky(r#"a=(); printf '[%s]' "${!a[*]}"; set -- "${!a[@]}"; printf '=%s' "$#""#);
    assert_eq!(stdout(&out), "[]=0");
    assert_eq!(status(&out), 0);
}

#[test]
fn unquoted_empty_array_keys_expand_to_nothing() {
    let out = sharky(r#"a=(); printf '[%s]' ${!a[*]} ${!a[@]}; printf done"#);
    assert_eq!(stdout(&out), "[]done");
    assert_eq!(status(&out), 0);
}

#[test]
fn unset_array_has_no_keys() {
    let out = sharky(
        r#"unset u; printf '[%s]' "${!u[*]}"; set -- "${!u[@]}"; printf '=%s' "$#""#,
    );
    assert_eq!(stdout(&out), "[]=0");
    assert_eq!(status(&out), 0);
}

// A defined scalar is a one-element array whose only key is 0; an undefined
// name, or an empty array, has no keys at all.

#[test]
fn keys_of_a_defined_scalar_are_zero() {
    let out = sharky(r#"x=hi; printf '[%s][%s]' "${!x[@]}" "${!x[*]}""#);
    assert_eq!(stdout(&out), "[0][0]");
    assert_eq!(status(&out), 0);
}

#[test]
fn keys_of_an_empty_scalar_are_still_zero() {
    let out = sharky(r#"x=; printf '[%s]' "${!x[@]}""#);
    assert_eq!(stdout(&out), "[0]");
    assert_eq!(status(&out), 0);
}

#[test]
fn unquoted_keys_of_a_defined_scalar_expand_to_one_word() {
    let out = sharky(r#"x=hi; set -- ${!x[@]}; printf 'n=%s<%s>' "$#" "$1""#);
    assert_eq!(stdout(&out), "n=1<0>");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_keys_of_a_defined_scalar_are_zero() {
    let out = sharky(r#"x=hi; v="${!x[@]}"; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0]");
    assert_eq!(status(&out), 0);
}

#[test]
fn keys_of_an_undefined_name_expand_to_nothing() {
    let out = sharky(r#"unset u; set -- ${!u[@]}; printf 'n=%s[%s]' "$#" "${!u[*]}""#);
    assert_eq!(stdout(&out), "n=0[]");
    assert_eq!(status(&out), 0);
}

#[test]
fn array_key_counts_match_the_element_counts() {
    let out = sharky(r#"IFS=:; a=(x y); printf '%s%s' "${#a[*]}" "${#a[@]}""#);
    assert_eq!(stdout(&out), "22");
    assert_eq!(status(&out), 0);
}

#[test]
fn associative_key_counts_match_the_element_counts() {
    let out = sharky(r#"declare -A m=([k1]=v1); printf '%s%s' "${#m[*]}" "${#m[@]}""#);
    assert_eq!(stdout(&out), "11");
    assert_eq!(status(&out), 0);
}

#[test]
fn empty_array_key_counts_are_zero() {
    let out = sharky(r#"a=(); printf '%s%s' "${#a[*]}" "${#a[@]}""#);
    assert_eq!(stdout(&out), "00");
    assert_eq!(status(&out), 0);
}

// local: a name without a value shadows the previous binding without becoming
// set itself.

#[test]
fn local_without_a_value_leaves_the_name_unset() {
    let out = sharky(r#"v=outer; f() { local v; printf '[%s]' "${v-unset}"; }; f"#);
    assert_eq!(stdout(&out), "[unset]");
    assert_eq!(status(&out), 0);
}

#[test]
fn local_without_a_value_trips_nounset() {
    let out = sharky(r#"set -u; f() { local v; printf '%s' "$v"; }; f; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound"));
}

// With no positional parameters, "$@" and "$*" are unset for the operators
// without a colon.

#[test]
fn empty_positional_parameters_are_unset_for_alternate_operators() {
    let out = sharky(
        r#"printf '<%s>' "${@+set}"; printf '|'; printf '<%s>' "${@-fallback}"; printf '|'; printf '<%s>' "${*+set}"; printf '|'; printf '<%s>' "${*-fallback}""#,
    );
    assert_eq!(stdout(&out), "<>|<fallback>|<>|<fallback>");
    assert_eq!(status(&out), 0);
}

#[test]
fn colon_operators_still_see_empty_positional_parameters_as_null() {
    let out = sharky(r#"printf '[%s][%s]' "${@:-colon}" "${@:+colon}""#);
    assert_eq!(stdout(&out), "[colon][]");
    assert_eq!(status(&out), 0);
}

// ${v?word} expands its word before failing and reports it in the message.

#[test]
fn error_expansion_expands_its_word_before_failing() {
    let out = sharky(r#"unset v; printf '[%s]' "${v?$(echo boom)}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("v: boom"), "{stderr:?}");
}

#[test]
fn empty_error_word_uses_the_null_parameter_message() {
    let out = sharky(r#"unset v; printf '[%s]' "${v?}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("v: parameter null or not set"), "{stderr:?}");
}

// Bash reports a null or unset parameter error with 127 in a -c command string,
// including when it happens inside a function, but with 1 in a script or a
// subshell. Verify every measured shape in each of those contexts.

#[test]
fn command_string_null_parameter_with_a_word_uses_127() {
    let out = sharky(r#"unset v; printf '[%s]' "${v?boom}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn command_string_null_parameter_without_a_word_uses_127() {
    let out = sharky(r#"unset v; printf '[%s]' "${v?}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn command_string_nounset_parameter_uses_127() {
    let out = sharky(r#"set -u; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn command_string_nounset_in_a_for_list_uses_127() {
    let out = sharky(r#"set -u; for x in a$missing b; do :; done"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing: unbound variable"));
}

#[test]
fn command_string_error_after_another_command_uses_127() {
    let out = sharky(r#"unset v; echo ok; printf '[%s]' "${v?boom}""#);
    assert_eq!(stdout(&out), "ok\n");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn function_null_parameter_with_a_word_uses_127() {
    let out = sharky(r#"unset v; f(){ printf '[%s]' "${v?boom}"; }; f"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn function_null_parameter_without_a_word_uses_127() {
    let out = sharky(r#"unset v; f(){ printf '[%s]' "${v?}"; }; f"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn function_nounset_parameter_uses_127() {
    let out = sharky(r#"set -u; f(){ printf '[%s]' "$v"; }; f"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn function_nounset_in_a_for_list_uses_127() {
    let out = sharky(r#"set -u; f(){ for x in a$missing b; do :; done; }; f"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing: unbound variable"));
}

#[test]
fn function_error_after_another_command_uses_127() {
    let out = sharky(r#"unset v; f(){ echo ok; printf '[%s]' "${v?boom}"; }; f"#);
    assert_eq!(stdout(&out), "ok\n");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

// An expansion error inside an if or while condition aborts the whole shell in
// bash; it is not a false condition. The status follows the command-string
// rule, and no later command runs.

#[test]
fn command_string_error_word_in_an_if_condition_aborts() {
    let out = sharky(r#"unset v; if printf '%s' "${v?boom}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn command_string_error_word_in_a_while_condition_aborts() {
    let out = sharky(r#"unset v; while printf '%s' "${v?boom}"; do :; done; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn command_string_elif_error_word_aborts() {
    let out = sharky(r#"unset v; if false; then :; elif printf '%s' "${v?boom}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn command_string_error_without_a_word_in_an_if_condition_aborts() {
    let out = sharky(r#"unset v; if printf '%s' "${v?}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn command_string_nounset_in_an_if_condition_aborts() {
    let out = sharky(r#"set -u; if printf '%s' "$v"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn command_string_indirect_error_in_an_if_condition_aborts() {
    let out = sharky(r#"set -u; unset n; if printf '%s' "${!n}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("n: invalid indirect expansion"));
}

#[test]
fn script_error_word_in_an_if_condition_aborts() {
    let out = sharky_script(r#"unset v; if printf '%s' "${v?boom}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn script_nounset_in_an_if_condition_aborts() {
    let out = sharky_script(r#"set -u; if printf '%s' "$v"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn stdin_error_word_in_an_if_condition_aborts() {
    let out = sharky_stdin(r#"unset v; if printf '%s' "${v?boom}"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn stdin_nounset_in_an_if_condition_aborts() {
    let out = sharky_stdin(r#"set -u; if printf '%s' "$v"; then :; fi; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn interactive_shell_skips_the_body_of_a_failing_condition() {
    let seen = run_interactive(
        &[
            r#"unset v; if printf 'BODY%s' "${v?boom}"; then :; fi"#,
            r#"printf 'AFTER-IF-%s' 99"#,
        ],
        "AFTER-IF-99",
    );
    assert!(!seen.contains("BODY99"), "condition body ran: {seen:?}");
    assert!(seen.contains("AFTER-IF-99"), "shell died: {seen:?}");
}

#[test]
fn subshell_null_parameter_with_a_word_uses_1() {
    let out = sharky(r#"unset v; ( printf '[%s]' "${v?boom}" )"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn subshell_null_parameter_without_a_word_uses_1() {
    let out = sharky(r#"unset v; ( printf '[%s]' "${v?}" )"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn subshell_nounset_parameter_uses_1() {
    let out = sharky(r#"set -u; ( printf '[%s]' "$v" )"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn subshell_nounset_in_a_for_list_uses_1() {
    let out = sharky(r#"set -u; ( for x in a$missing b; do :; done )"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing: unbound variable"));
}

#[test]
fn subshell_error_after_another_command_uses_1() {
    let out = sharky(r#"unset v; ( echo ok; printf '[%s]' "${v?boom}" )"#);
    assert_eq!(stdout(&out), "ok\n");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn script_null_parameter_with_a_word_uses_1() {
    let out = sharky_script(r#"unset v; printf '[%s]' "${v?boom}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn script_null_parameter_without_a_word_uses_1() {
    let out = sharky_script(r#"unset v; printf '[%s]' "${v?}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn script_nounset_parameter_uses_1() {
    let out = sharky_script(r#"set -u; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn script_nounset_in_a_for_list_uses_1() {
    let out = sharky_script(r#"set -u; for x in a$missing b; do :; done"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("missing: unbound variable"));
}

#[test]
fn script_error_after_another_command_uses_1() {
    let out = sharky_script(r#"unset v; echo ok; printf '[%s]' "${v?boom}""#);
    assert_eq!(stdout(&out), "ok\n");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

// A script that arrives on standard input is not a -c command string, so bash
// reports a null or unset parameter error with 1 there as well.

#[test]
fn stdin_null_parameter_with_a_word_uses_1() {
    let out = sharky_stdin(r#"unset v; printf '[%s]' "${v?boom}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn stdin_null_parameter_without_a_word_uses_1() {
    let out = sharky_stdin(r#"unset v; printf '[%s]' "${v?}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("parameter null or not set"));
}

#[test]
fn stdin_nounset_parameter_uses_1() {
    let out = sharky_stdin(r#"set -u; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

#[test]
fn interactive_null_parameter_error_reports_status_1() {
    let seen = run_interactive(
        &[
            r#"unset v; printf '%s' "${v?boom}""#,
            r#"printf 'STATUS-%s' "$?""#,
            r#"printf 'AFTER-%s' 99"#,
        ],
        "AFTER-99",
    );
    assert!(seen.contains("STATUS-1"), "{seen:?}");
    assert!(seen.contains("AFTER-99"), "{seen:?}");
}

// Bash also reports 1 rather than 127 when errexit is enabled, including in a
// corpus-style `set -euo pipefail` script.
#[test]
fn errexit_null_parameter_error_uses_1() {
    let out = sharky(r#"set -e; unset v; printf '[%s]' "${v?boom}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn errexit_nounset_error_uses_1() {
    let out = sharky(r#"set -euo pipefail; printf 'start\n'; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "start\n");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: unbound variable"));
}

// set -o accepts the long option names of the flags it implements.

#[test]
fn set_o_errexit_aborts_after_a_failing_command() {
    let out = sharky("set -o errexit; false; printf no");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn set_o_nounset_errors_on_an_unset_expansion() {
    let out = sharky(r#"set -o nounset; printf '%s' "$SHARKY_UNSET_E"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound"));
}

#[test]
fn set_o_xtrace_traces_to_stderr() {
    let out = sharky("set -o xtrace; printf hi");
    assert_eq!(stdout(&out), "hi");
    assert_eq!(status(&out), 0);
    assert!(String::from_utf8_lossy(&out.stderr).contains("+ printf hi"));
}

#[test]
fn set_plus_o_long_name_disables_the_flag() {
    let out = sharky("set -e; set +o errexit; false; printf after");
    assert_eq!(stdout(&out), "after");
    assert_eq!(status(&out), 0);
}

// A simple assignment reports the status of its command substitution.

#[test]
fn assignment_status_comes_from_the_command_substitution() {
    let out = sharky(r#"x=$(exit 7); printf '[%s]' "$?""#);
    assert_eq!(stdout(&out), "[7]");
    assert_eq!(status(&out), 0);
}

#[test]
fn failing_assignment_under_errexit_aborts() {
    let out = sharky("set -e; x=$(false); printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// The word of a default expansion keeps its internal whitespace.

#[test]
fn default_word_keeps_internal_whitespace() {
    let out = sharky(r#"unset v; printf '[%s]' "${v:-a  b}""#);
    assert_eq!(stdout(&out), "[a  b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn default_word_keeps_leading_whitespace() {
    let out = sharky(r#"unset v; printf '[%s]' "${v:- a  b}""#);
    assert_eq!(stdout(&out), "[ a  b]");
    assert_eq!(status(&out), 0);
}

// Letters after an o in an option cluster are still applied.

#[test]
fn option_cluster_letters_after_o_are_still_applied() {
    let out = sharky("set -oe pipefail; false; printf no");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn option_cluster_after_o_keeps_pipefail_enabled() {
    let out = sharky(r#"set -oe pipefail; set +e; false | true; printf '[%s]' "$?""#);
    assert_eq!(stdout(&out), "[1]");
    assert_eq!(status(&out), 0);
}

// An interactive shell survives a failing command under errexit or nounset.

fn interactive() -> (std::process::Child, std::os::fd::OwnedFd) {
    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .spawn()
        .expect("spawn sharky");

    (child, pty.master)
}

fn read_until(fd: RawFd, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut collected = String::new();
    let mut buffer = [0u8; 1024];

    while Instant::now() < deadline {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and one valid read buffer.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if ready > 0 {
            // SAFETY: the descriptor is the pty master and the buffer is valid.
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };
            if count > 0 {
                collected.push_str(&String::from_utf8_lossy(&buffer[..count as usize]));
                if collected.contains(needle) {
                    return collected;
                }
            }
        }
    }

    collected
}

fn run_interactive(script: &[&str], needle: &str) -> String {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    read_until(fd, "❯", Duration::from_secs(5));
    for line in script {
        let _ = writer.write_all(format!("{line}\r").as_bytes());
    }

    let seen = read_until(fd, needle, Duration::from_secs(5));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    seen
}

#[test]
fn interactive_shell_survives_errexit() {
    let seen = run_interactive(
        &["set -e", "false", "printf 'AFTER-E-%s' 99"],
        "AFTER-E-99",
    );
    assert!(seen.contains("AFTER-E-99"), "shell died on errexit: {seen:?}");
}

#[test]
fn interactive_shell_survives_nounset() {
    let seen = run_interactive(
        &["set -u", "printf '%s' \"$SHARKY_NOPE\"", "printf 'AFTER-U-%s' 99"],
        "AFTER-U-99",
    );
    assert!(seen.contains("AFTER-U-99"), "shell died on nounset: {seen:?}");
}

#[test]
fn interactive_shell_survives_a_failing_if_condition() {
    let seen = run_interactive(
        &[
            "set -e",
            "if false || false; then :; else printf else; fi",
            "printf 'AFTER-I-%s' 99",
        ],
        "AFTER-I-99",
    );
    assert!(seen.contains("else"), "condition did not run: {seen:?}");
    assert!(seen.contains("AFTER-I-99"), "shell died on the condition: {seen:?}");
}

// An interactive shell keeps running after an expansion error, but the error
// still discards the whole compound command: bash runs no branch, not even
// else, and the if reports the error status.

#[test]
fn interactive_if_expansion_error_discards_the_else_branch() {
    let seen = run_interactive(
        &[
            "unset v",
            r#"if printf '%s' "${v?boom}"; then :; else printf 'ELSE-%s' 77; fi"#,
            r#"printf 'ST-%s' "$?""#,
            r#"printf 'AFTER-%s' 99"#,
        ],
        "AFTER-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("ELSE-77"), "else branch ran: {seen:?}");
    assert!(seen.contains("ST-1"), "wrong status: {seen:?}");
    assert!(seen.contains("AFTER-99"), "shell died: {seen:?}");
}

#[test]
fn interactive_elif_expansion_error_discards_the_else_branch() {
    let seen = run_interactive(
        &[
            "unset v",
            r#"if false; then :; elif printf '%s' "${v?boom}"; then printf 'THEN-%s' 55; else printf 'ELSE-%s' 77; fi"#,
            r#"printf 'ST-%s' "$?""#,
            r#"printf 'AFTER-%s' 99"#,
        ],
        "AFTER-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("THEN-55"), "then branch ran: {seen:?}");
    assert!(!seen.contains("ELSE-77"), "else branch ran: {seen:?}");
    assert!(seen.contains("ST-1"), "wrong status: {seen:?}");
    assert!(seen.contains("AFTER-99"), "shell died: {seen:?}");
}

#[test]
fn interactive_false_if_condition_still_runs_the_else_branch() {
    let seen = run_interactive(
        &[
            r#"if false; then printf 'THEN-%s' 55; else printf 'ELSE-%s' 77; fi"#,
            r#"printf 'ST-%s' "$?""#,
            r#"printf 'AFTER-%s' 99"#,
        ],
        "AFTER-99",
    );
    assert!(!seen.contains("THEN-55"), "then branch ran: {seen:?}");
    assert!(seen.contains("ELSE-77"), "else branch did not run: {seen:?}");
    assert!(seen.contains("ST-0"), "wrong status: {seen:?}");
    assert!(seen.contains("AFTER-99"), "shell died: {seen:?}");
}

// ${!a[0]} and ${!a[$i]} indirect through the value held by the element, while
// ${!a[@]} and ${!a[*]} keep expanding to the array keys. An unset element
// indirection is empty; an unset plain name, or a value that is not a name,
// fails the expansion.

#[test]
fn indirect_element_expands_the_name_it_holds() {
    let out =
        sharky(r#"a=(IND OTHER); IND=hello; i=0; printf '[%s][%s]' "${!a[$i]}" "${!a[0]}""#);
    assert_eq!(stdout(&out), "[hello][hello]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_element_uses_an_arithmetic_subscript() {
    let out = sharky(r#"a=(IND OTHER); OTHER=world; i=0; printf '[%s]' "${!a[i+1]}""#);
    assert_eq!(stdout(&out), "[world]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_of_an_unset_element_is_empty() {
    let out = sharky(r#"a=(IND); IND=hello; printf '[%s][%s]' "${!a[5]}" "${!a[0]}""#);
    assert_eq!(stdout(&out), "[][hello]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_element_reads_an_associative_value() {
    let out = sharky(r#"declare -A m=([k]=IND); IND=hello; printf '[%s]' "${!m[k]}""#);
    assert_eq!(stdout(&out), "[hello]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_plain_name_expands_its_value_as_a_name() {
    let out = sharky(r#"a=(IND OTHER); name=a; printf '[%s]' "${!name}""#);
    assert_eq!(stdout(&out), "[IND]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_plain_name_pointing_at_an_unset_name_is_empty() {
    let out = sharky(r#"name=nope; unset nope; printf '[%s]' "${!name}""#);
    assert_eq!(stdout(&out), "[]");
    assert_eq!(status(&out), 0);
}

#[test]
fn indirect_of_an_unset_name_fails() {
    let out = sharky(r#"unset name; printf '[%s]' "${!name}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("name: invalid indirect expansion"),
        "{stderr:?}"
    );
}

#[test]
fn indirect_of_a_value_that_is_not_a_name_fails() {
    let out = sharky(r#"name='a b'; printf '[%s]' "${!name}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a b: invalid variable name"), "{stderr:?}");
}

// Under set -u, bash reports the indirect form's own errors: an unset name is
// an invalid indirect expansion at status 1, and a name whose target is unset
// is a nounset violation blamed on the !name form.

#[test]
fn nounset_indirect_of_an_unset_name_reports_only_the_indirect_error() {
    let out = sharky(r#"set -u; unset n; printf '%s' "${!n}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("n: invalid indirect expansion"), "{stderr:?}");
    assert!(!stderr.contains("unbound variable"), "{stderr:?}");
}

#[test]
fn nounset_indirect_of_an_invalid_name_reports_the_name_error() {
    let out = sharky(r#"set -u; n='a b'; printf '%s' "${!n}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("a b: invalid variable name"), "{stderr:?}");
    assert!(!stderr.contains("unbound variable"), "{stderr:?}");
}

#[test]
fn nounset_indirect_of_an_unset_target_uses_127() {
    let out = sharky(r#"set -u; n=target; printf '%s' "${!n}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("!n: unbound variable"));
}

#[test]
fn script_nounset_indirect_of_an_unset_name_uses_1() {
    let out = sharky_script(r#"set -u; unset n; printf '%s' "${!n}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("n: invalid indirect expansion"));
}

#[test]
fn stdin_nounset_indirect_of_an_unset_name_uses_1() {
    let out = sharky_stdin(r#"set -u; unset n; printf '%s' "${!n}""#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("n: invalid indirect expansion"));
}

#[test]
fn interactive_shell_survives_a_nounset_indirect_error() {
    let seen = run_interactive(
        &[
            "set +H",
            "set -u",
            r#"unset n; printf '%s' "${!n}""#,
            r#"printf 'AFTER-IND-%s' 99"#,
        ],
        "AFTER-IND-99",
    );
    assert!(
        seen.contains("n: invalid indirect expansion"),
        "missing error: {seen:?}"
    );
    assert!(!seen.contains("n: unbound variable"), "{seen:?}");
    assert!(seen.contains("AFTER-IND-99"), "shell died: {seen:?}");
}

#[test]
fn indirect_key_list_forms_stay_key_lists() {
    let out =
        sharky(r#"a=(IND OTHER); IND=hello; printf '[%s]' "${!a[@]}"; printf '<%s>' "${!a[*]}""#);
    assert_eq!(stdout(&out), "[0][1]<0 1>");
    assert_eq!(status(&out), 0);
}

// In a context without word splitting — an assignment, a here-document — the at
// form of an array expands with its elements joined by a space, whatever IFS
// holds; the star form keeps the first character of IFS.

#[test]
fn assigned_array_elements_join_with_a_space() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v=${a[@]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[k1 k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn quoted_assigned_array_elements_join_with_a_space() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v="x${a[@]}y"; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[xk1 k2y]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_array_elements_join_with_a_space_with_an_empty_ifs() {
    let out = sharky(r#"IFS=; a=(k1 k2); v=${a[@]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[k1 k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_array_elements_keep_their_spaces() {
    let out = sharky(r#"IFS=,; a=('k 1' k2); v=${a[@]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[k 1 k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn consecutive_assigned_array_expansions_stay_space_joined() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v=${a[@]}${a[@]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[k1 k2k1 k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_array_keys_still_join_with_a_space() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v=${!a[@]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0 1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn quoted_assigned_array_keys_keep_the_ifs_join() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v="${!a[@]}"; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[0,1]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_array_star_form_keeps_the_ifs_join() {
    let out = sharky(r#"IFS=,; a=(k1 k2); v=${a[*]}; printf '[%s]' "$v""#);
    assert_eq!(stdout(&out), "[k1,k2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn assigned_associative_values_join_with_a_space() {
    let out = sharky(
        r#"IFS=,; declare -A m=([k1]=v1 [k2]=v2); v=${m[@]}; printf '[%s]' "$v""#,
    );
    assert_eq!(stdout(&out), "[v1 v2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn here_document_joins_array_elements_with_a_space() {
    let out = sharky("IFS=,; a=(k1 k2); cat <<EOF\n[${a[@]}]\nEOF");
    assert_eq!(stdout(&out), "[k1 k2]\n");
    assert_eq!(status(&out), 0);
}

// `read` splits on IFS like word splitting, except that the last name receives
// the remainder of the line with its delimiters. Leading IFS whitespace is
// skipped and trailing IFS whitespace is dropped.

#[test]
fn read_splits_on_a_custom_ifs_delimiter() {
    let out = sharky(r#"IFS=,; printf 'a,b\n' | { read x y; printf '%s|%s' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "a|b");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_gives_the_last_name_the_remainder_with_delimiters() {
    let out = sharky(r#"IFS=,; printf 'a,b,c\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b,c]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_keeps_empty_fields_between_non_whitespace_delimiters() {
    let out = sharky(
        r#"IFS=:; printf 'a::b:\n' | { read x y z; printf '[%s][%s][%s]' "$x" "$y" "$z"; }"#,
    );
    assert_eq!(stdout(&out), "[a][][b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_drops_trailing_ifs_whitespace_from_the_last_name() {
    let out = sharky(r#"printf '  a  b  c  \n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b  c]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_treats_ifs_whitespace_next_to_a_delimiter_as_one_separator() {
    let out = sharky(r#"IFS=' ,'; printf 'a , b\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_with_a_leading_delimiter_produces_an_empty_field() {
    let out = sharky(r#"IFS=' ,'; printf ', a,b\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[][a,b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_with_an_empty_ifs_keeps_the_line_whole() {
    let out = sharky(r#"IFS=; printf ' a b \n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[ a b ][]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_without_names_keeps_the_line_verbatim_in_reply() {
    let out = sharky(r#"printf '  a  \n' | { read; printf '[%s]' "$REPLY"; }"#);
    assert_eq!(stdout(&out), "[  a  ]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_into_a_named_reply_still_splits() {
    let out = sharky(r#"printf '  a  \n' | { read REPLY; printf '[%s]' "$REPLY"; }"#);
    assert_eq!(stdout(&out), "[a]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_consumes_one_trailing_delimiter_of_the_remainder() {
    let out = sharky(r#"IFS=,; printf 'a,b,\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_keeps_a_remainder_that_does_not_end_in_a_delimiter() {
    let out = sharky(r#"IFS=,; printf 'a,b,,\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b,,]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_fills_every_name_when_fields_run_out() {
    let out = sharky(
        r#"IFS=,; printf 'a,b,c\n' | { read w x y z; printf '[%s][%s][%s][%s]' "$w" "$x" "$y" "$z"; }"#,
    );
    assert_eq!(stdout(&out), "[a][b][c][]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_keeps_a_tab_when_it_is_not_in_ifs() {
    let out = sharky(
        r#"IFS=' '; printf 'x\t y\n' | { read a b; printf '[%s][%s]' "$a" "$b"; }"#,
    );
    assert_eq!(stdout(&out), "[x\t][y]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_keeps_a_space_that_is_not_an_ifs_delimiter() {
    let out = sharky(r#"IFS=,; printf 'a, b\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][ b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_with_one_name_keeps_the_line_but_trims_ifs_whitespace() {
    let out = sharky(r#"IFS=:; printf 'a:\n' | { read x; printf '[%s]' "$x"; }"#);
    assert_eq!(stdout(&out), "[a]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_r_splits_on_ifs_without_escapes() {
    let out = sharky(r#"IFS=,; printf 'a,b\n' | { read -r x y; printf '[%s][%s]' "$x" "$y"; }"#);
    assert_eq!(stdout(&out), "[a][b]");
    assert_eq!(status(&out), 0);
}

#[test]
fn read_keeps_an_escaped_delimiter_in_the_field() {
    let out = sharky(
        r#"IFS=,; printf 'a\\,b\n' | { read x y; printf '[%s][%s]' "$x" "$y"; }"#,
    );
    assert_eq!(stdout(&out), "[a,b][]");
    assert_eq!(status(&out), 0);
}

#[test]
fn mapfile_stays_line_based_with_a_custom_ifs() {
    let out = sharky(
        r#"IFS=,; printf 'a,b\nc,d\n' | { mapfile -t arr; printf '[%s][%s]' "${arr[0]}" "${arr[1]}"; }"#,
    );
    assert_eq!(stdout(&out), "[a,b][c,d]");
    assert_eq!(status(&out), 0);
}

// An expansion failure inside for, case or [[ belongs to that command. Bash
// runs no iteration or arm there, and the flag cannot survive into the command
// that follows: interactive bash aborts only the failing input line.

#[test]
fn expansion_error_in_for_runs_no_iteration() {
    let out =
        sharky(r#"for x in ${SHARKY_UNSET_F?boom} later; do [[ -n "$x" ]]; done"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("boom"), "{stderr:?}");
}

#[test]
fn expansion_error_in_case_runs_no_arm() {
    let out = sharky(r#"case ${SHARKY_UNSET_F?boom} in *) [[ -n x ]] ;; esac"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("boom"), "{stderr:?}");
}

#[test]
fn expansion_error_in_a_test_keeps_the_command_failure() {
    let out = sharky(r#"[[ -z ${SHARKY_UNSET_F?boom} ]]"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
}

#[test]
fn nounset_in_a_for_word_list_runs_no_iteration() {
    let out = sharky(r#"set -u; for x in a$SHARKY_UNSET_F b; do [[ -n "$x" ]]; done"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unbound"));
}

#[test]
fn interactive_shell_reports_a_for_expansion_error_on_its_own_line() {
    let seen = run_interactive(
        &[
            "for x in ${SHARKY_NOPE?boom}; do printf 'BODY%s' 99; done",
            "printf 'AFTER-F-%s' 99",
        ],
        "AFTER-F-99",
    );
    assert!(!seen.contains("BODY99"), "loop body ran: {seen:?}");
    assert!(
        seen.contains("AFTER-F-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_nounset_in_a_for_word_list_runs_no_iteration() {
    let seen = run_interactive(
        &[
            "set -u",
            "for x in a${SHARKY_NOPE} b; do printf 'BODY%s' \"$x\"; done",
            "printf 'AFTER-U-%s' 99",
        ],
        "AFTER-U-99",
    );
    assert!(
        !seen.contains("BODYa") && !seen.contains("BODYb"),
        "loop body ran: {seen:?}"
    );
    assert!(
        seen.contains("AFTER-U-99"),
        "next line was swallowed: {seen:?}"
    );
}

// Bash discards everything left on the failing input line, not only the
// command that reported the error: a later command on the same line never
// runs, while the next input line does. A pipeline element, a command
// substitution and a subshell are separate processes, so their error is
// contained and the parent keeps running the rest of its line.

#[test]
fn interactive_expansion_error_discards_the_rest_of_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; printf '%s' "${v?boom}"; printf 'SAME-%s' 99"#,
            r#"printf 'STATUS-%s' "$?""#,
            r#"printf 'AFTER-L-%s' 99"#,
        ],
        "AFTER-L-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("SAME-99"), "rest of line ran: {seen:?}");
    assert!(seen.contains("STATUS-1"), "wrong status: {seen:?}");
    assert!(
        seen.contains("AFTER-L-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_expansion_error_discards_a_group_and_the_rest_of_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; { printf '%s' "${v?boom}"; printf 'GROUP-%s' 99; }; printf 'AFTER-G-%s' 99"#,
            r#"printf 'NEXT-G-%s' 99"#,
        ],
        "NEXT-G-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("GROUP-99"), "group body ran: {seen:?}");
    assert!(!seen.contains("AFTER-G-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-G-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_expansion_error_in_an_if_body_discards_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; if true; then printf '%s' "${v?boom}"; printf 'THEN-%s' 99; fi; printf 'AFTER-I-%s' 99"#,
            r#"printf 'NEXT-I-%s' 99"#,
        ],
        "NEXT-I-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("THEN-99"), "then body ran: {seen:?}");
    assert!(!seen.contains("AFTER-I-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-I-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_expansion_error_in_a_loop_body_discards_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; for i in 1 2; do printf '%s' "${v?boom}"; printf 'BODY-%s' 99; done; printf 'AFTER-W-%s' 99"#,
            r#"printf 'NEXT-W-%s' 99"#,
        ],
        "NEXT-W-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("BODY-99"), "loop body ran: {seen:?}");
    assert!(!seen.contains("AFTER-W-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-W-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_expansion_error_in_a_case_body_discards_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; case x in x) printf '%s' "${v?boom}"; printf 'ARM-%s' 99;; esac; printf 'AFTER-C-%s' 99"#,
            r#"printf 'NEXT-C-%s' 99"#,
        ],
        "NEXT-C-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("ARM-99"), "case arm ran: {seen:?}");
    assert!(!seen.contains("AFTER-C-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-C-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_expansion_error_in_a_function_discards_the_callers_line() {
    let seen = run_interactive(
        &[
            r#"f() { printf '%s' "${v?boom}"; printf 'FBODY-%s' 99; }"#,
            r#"unset v; f; printf 'AFTER-FN-%s' 99"#,
            r#"printf 'NEXT-FN-%s' 99"#,
        ],
        "NEXT-FN-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("FBODY-99"), "function body ran: {seen:?}");
    assert!(!seen.contains("AFTER-FN-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-FN-99"),
        "next line was swallowed: {seen:?}"
    );
}

// The discard covers the whole input unit: a compound command that spans lines
// also drops the commands that follow its closing keyword on that line.
#[test]
fn interactive_expansion_error_discards_a_multiline_compound_and_its_line() {
    let seen = run_interactive(
        &[
            "unset v",
            "if true; then",
            r#"printf '%s' "${v?boom}""#,
            r#"fi; printf 'AFTER-ML-%s' 99"#,
            r#"printf 'NEXT-ML-%s' 99"#,
        ],
        "NEXT-ML-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("AFTER-ML-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-ML-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_negated_expansion_error_discards_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; ! printf '%s' "${v?boom}"; printf 'AFTER-N-%s' 99"#,
            r#"printf 'NEXT-N-%s' 99"#,
        ],
        "NEXT-N-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("AFTER-N-99"), "rest of line ran: {seen:?}");
    assert!(
        seen.contains("NEXT-N-99"),
        "next line was swallowed: {seen:?}"
    );
}

#[test]
fn interactive_pipeline_expansion_error_keeps_the_rest_of_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; printf '%s' "${v?boom}" | cat; printf 'AFTER-P-%s' 99"#,
            r#"printf 'NEXT-P-%s' 99"#,
        ],
        "NEXT-P-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(
        seen.contains("AFTER-P-99"),
        "rest of line was discarded: {seen:?}"
    );
    assert!(seen.contains("NEXT-P-99"), "next line was swallowed: {seen:?}");
}

#[test]
fn interactive_subshell_expansion_error_discards_only_the_subshell() {
    let seen = run_interactive(
        &[
            r#"unset v; ( printf '%s' "${v?boom}"; printf 'SUBSH-%s' 99 ); printf 'AFTER-S-%s' 99"#,
            r#"printf 'NEXT-S-%s' 99"#,
        ],
        "NEXT-S-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(!seen.contains("SUBSH-99"), "subshell body ran: {seen:?}");
    assert!(
        seen.contains("AFTER-S-99"),
        "parent line was discarded: {seen:?}"
    );
    assert!(seen.contains("NEXT-S-99"), "next line was swallowed: {seen:?}");
}

#[test]
fn interactive_command_substitution_expansion_error_keeps_the_rest_of_the_line() {
    let seen = run_interactive(
        &[
            r#"unset v; printf '%s' "$(printf '%s' "${v?boom}")"; printf 'AFTER-CS-%s' 99"#,
            r#"printf 'NEXT-CS-%s' 99"#,
        ],
        "NEXT-CS-99",
    );
    assert!(seen.contains("v: boom"), "missing error: {seen:?}");
    assert!(
        seen.contains("AFTER-CS-99"),
        "rest of line was discarded: {seen:?}"
    );
    assert!(
        seen.contains("NEXT-CS-99"),
        "next line was swallowed: {seen:?}"
    );
}

// Outside an interactive shell the error ends the whole input: the command
// string, the script and the standard-input source all stop there.

#[test]
fn command_string_expansion_error_stops_the_rest_of_the_string() {
    let out = sharky(r#"unset v; printf '%s' "${v?boom}"; printf 'SAME-%s' 99"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn script_expansion_error_stops_the_rest_of_the_script() {
    let out = sharky_script(
        "unset v\nprintf '%s' \"${v?boom}\"; printf 'SAME-%s' 99\nprintf 'AFTER-SCRIPT-%s' 99\n",
    );
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

#[test]
fn standard_input_expansion_error_stops_the_rest_of_the_input() {
    let out = sharky_stdin("unset v\nprintf '%s' \"${v?boom}\"; printf 'SAME-%s' 99\necho AFTER\n");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("v: boom"));
}

// An assignment-only command keeps its assignments in the shell and performs
// its redirections, reporting the command-substitution status.

#[test]
fn assignment_with_a_redirection_keeps_the_value_and_creates_the_file() {
    let dir = temp_dir();
    let file = dir.join("out");
    let out = sharky(&format!(
        "x=hello > {path}; printf '[%s]' \"$x\"; printf '[%s]' \"$(cat {path})\"",
        path = file.display()
    ));
    assert_eq!(stdout(&out), "[hello][]");
    assert_eq!(status(&out), 0);
    assert!(file.exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn assignment_with_a_redirection_reports_the_substitution_status() {
    let dir = temp_dir();
    let file = dir.join("out");
    let out = sharky(&format!(
        "x=$(false) > {path}; printf 'st=%s x=[%s]' \"$?\" \"$x\"",
        path = file.display()
    ));
    assert_eq!(stdout(&out), "st=1 x=[]");
    assert_eq!(status(&out), 0);
    assert!(file.exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn failing_assignment_redirection_aborts_under_errexit() {
    let dir = temp_dir();
    let file = dir.join("out");
    let out = sharky(&format!(
        "set -e; x=$(false) > {path}; printf after",
        path = file.display()
    ));
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(file.exists());
    std::fs::remove_dir_all(&dir).ok();
}

// A name is either a scalar or an array. A scalar write to an array name lands
// in element 0, a subscript on a scalar turns it into an array, and a name that
// became an array stops being exported as a scalar.

#[test]
fn scalar_assignment_to_an_array_name_writes_element_zero() {
    let out = sharky(r#"a=(1 2); a=3; printf '[%s][%s][%s]' "$a" "${#a[@]}" "${a[1]}""#);
    assert_eq!(stdout(&out), "[3][2][2]");
    assert_eq!(status(&out), 0);
}

#[test]
fn loop_and_read_writes_to_an_array_name_write_element_zero() {
    let out = sharky(r#"a=(1 2); for a in x y; do :; done; printf '[%s][%s]' "$a" "${#a[@]}""#);
    assert_eq!(stdout(&out), "[y][2]");
    let out = sharky(r#"a=(1 2); read -r a <<< hi; printf '[%s][%s]' "$a" "${#a[@]}""#);
    assert_eq!(stdout(&out), "[hi][2]");
}

#[test]
fn subscript_on_a_scalar_name_moves_it_into_the_array() {
    let out = sharky(r#"x=old; x[1]=new; printf '[%s][%s][%s]' "${x[0]}" "${x[1]}" "${#x[@]}""#);
    assert_eq!(stdout(&out), "[old][new][2]");
}

#[test]
fn indexed_declaration_over_a_scalar_keeps_it_as_element_zero() {
    let out = sharky(r#"x=old; declare -a x; printf '[%s][%s]' "$x" "${#x[@]}""#);
    assert_eq!(stdout(&out), "[old][1]");
}

#[test]
fn associative_declaration_over_a_scalar_keeps_it_under_key_zero() {
    let out = sharky(r#"x=old; declare -A x; printf '[%s][%s]' "${x[0]}" "${#x[@]}""#);
    assert_eq!(stdout(&out), "[old][1]");
}

#[test]
fn array_declaration_stops_exporting_a_stale_scalar() {
    let out = sharky(
        r#"SHARKY_D=old; declare -a SHARKY_D=(a b); printf '[%s]' "${SHARKY_D[0]}"; sh -c 'printf "<%s>" "$SHARKY_D"'"#,
    );
    assert_eq!(stdout(&out), "[a]<>");
    assert_eq!(status(&out), 0);
}

#[test]
fn mapfile_stops_exporting_a_stale_scalar() {
    let dir = temp_dir();
    let file = dir.join("lines");
    std::fs::write(&file, "a\nb\n").unwrap();
    let out = sharky(&format!(
        "SHARKY_M=old; mapfile -t SHARKY_M < {path}; printf '[%s][%s]' \"${{SHARKY_M[0]}}\" \"${{SHARKY_M[1]}}\"; sh -c 'printf \"<%s>\" \"$SHARKY_M\"'",
        path = file.display()
    ));
    assert_eq!(stdout(&out), "[a][b]<>");
    assert_eq!(status(&out), 0);
    std::fs::remove_dir_all(&dir).ok();
}

// bash's declaration builtins ignore the status of a command substitution in
// their arguments: unlike a plain assignment, they return 0 and do not trip
// errexit.

#[test]
fn declaration_builtins_keep_status_zero_for_a_failing_substitution() {
    for command in ["local", "declare", "export", "readonly", "typeset"] {
        let script = format!("set -e; f() {{ {command} x=$(false); }}; f; printf after");
        let out = sharky(&script);
        assert_eq!(stdout(&out), "after", "{command}: {out:?}");
        assert_eq!(status(&out), 0, "{command}");
    }
}

// Converting an array rejects the change, reports it and keeps the original
// value, unlike silently dropping the elements.

#[test]
fn indexed_array_rejects_conversion_to_associative() {
    let out = sharky(r#"a=(x y); declare -A a; printf 'st=%s n=%s e0=%s' "$?" "${#a[@]}" "${a[0]}""#);
    assert_eq!(stdout(&out), "st=1 n=2 e0=x");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot convert indexed to associative array"),
        "{stderr:?}"
    );
}

#[test]
fn associative_array_rejects_conversion_to_indexed() {
    let out = sharky(r#"declare -A a=([k]=v); declare -a a; printf 'st=%s n=%s k=%s' "$?" "${#a[@]}" "${a[k]}""#);
    assert_eq!(stdout(&out), "st=1 n=1 k=v");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot convert associative to indexed array"),
        "{stderr:?}"
    );
}

// `command` reports names through the same precedence table `type` uses, so a
// function still wins over a builtin and an alias still wins over both.

fn sharky_with_path(script: &str, path: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", path)
        .output()
        .expect("failed to run sharky")
}

#[test]
fn command_v_prints_the_bare_name_of_a_builtin() {
    let out = sharky("command -v cd");
    assert_eq!(stdout(&out), "cd\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_prints_the_bare_name_of_a_function() {
    let out = sharky("probe() { :; }; command -v probe");
    assert_eq!(stdout(&out), "probe\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_prints_the_path_of_an_external() {
    let out = sharky("command -v ls");
    let text = stdout(&out);
    assert_eq!(status(&out), 0);
    assert!(text.starts_with('/'), "{text:?}");
    assert!(text.trim_end().ends_with("ls"), "{text:?}");
}

#[test]
fn command_v_reports_a_missing_name_silently() {
    let out = sharky("command -v definitely-not-a-command");
    assert_eq!(stdout(&out), "");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 1);
}

// Bash reports 0 for -v/-V when *any* name resolves, regardless of where the
// missing names sit, and 1 only when none of them resolve. The missing names
// are simply skipped; they do not turn an otherwise successful query into a
// failure.
#[test]
fn command_v_succeeds_when_a_later_name_is_missing() {
    let out = sharky("command -v cd definitely-not-a-command");
    assert_eq!(stdout(&out), "cd\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_succeeds_when_an_earlier_name_is_missing() {
    let out = sharky("command -v definitely-not-a-command cd");
    assert_eq!(stdout(&out), "cd\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_fails_when_every_name_is_missing() {
    let out = sharky("command -v definitely-not-a-command also-missing");
    assert_eq!(stdout(&out), "");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn command_v_succeeds_when_every_name_resolves() {
    let out = sharky("probe() { :; }; command -v cd probe ls");
    let text = stdout(&out);
    assert_eq!(status(&out), 0);
    assert!(text.starts_with("cd\nprobe\n/"), "{text:?}");
}

#[test]
fn command_big_v_succeeds_when_a_later_name_is_missing() {
    let out = sharky("command -V cd definitely-not-a-command");
    assert_eq!(stdout(&out), "cd is a shell builtin\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("command: definitely-not-a-command: not found"),
        "{stderr:?}"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_succeeds_when_an_earlier_name_is_missing() {
    let out = sharky("command -V definitely-not-a-command cd");
    assert_eq!(stdout(&out), "cd is a shell builtin\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("command: definitely-not-a-command: not found"),
        "{stderr:?}"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_prints_the_alias_definition() {
    let out = sharky("alias hi='echo hello'; command -v hi");
    assert_eq!(stdout(&out), "alias hi='echo hello'\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_describes_a_builtin() {
    let out = sharky("command -V cd");
    assert_eq!(stdout(&out), "cd is a shell builtin\n");
    assert_eq!(status(&out), 0);
}

// When -v and -V are both given, the last one wins, in a cluster or across
// words.

#[test]
fn command_clustered_big_v_then_v_reports_the_name() {
    let out = sharky("command -Vv cd");
    assert_eq!(stdout(&out), "cd\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_clustered_v_then_big_v_reports_the_description() {
    let out = sharky("command -vV cd");
    assert_eq!(stdout(&out), "cd is a shell builtin\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_separate_big_v_then_v_reports_the_name() {
    let out = sharky("command -V -v cd");
    assert_eq!(stdout(&out), "cd\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_separate_v_then_big_v_reports_the_description() {
    let out = sharky("command -v -V cd");
    assert_eq!(stdout(&out), "cd is a shell builtin\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_function_follows_the_last_flag() {
    let out = sharky("probe() { :; }; command -Vv probe");
    assert_eq!(stdout(&out), "probe\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_describes_a_function() {
    let out = sharky("probe() { :; }; command -V probe");
    assert_eq!(
        stdout(&out),
        "probe is a function\nprobe () \n{ \n    :\n}\n"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_prints_every_statement_of_a_function() {
    let out = sharky("probe() { printf '%s' one; printf '%s' two; }; command -V probe");
    assert_eq!(
        stdout(&out),
        "probe is a function\nprobe () \n{ \n    printf '%s' one;\n    printf '%s' two\n}\n"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_describes_an_external_with_its_path() {
    let out = sharky("command -V ls");
    let text = stdout(&out);
    assert_eq!(status(&out), 0);
    let described = text.trim_end();
    let path = described
        .strip_prefix("ls is ")
        .unwrap_or_else(|| panic!("unexpected description: {described:?}"));
    assert!(path.starts_with('/'), "{described:?}");
    assert!(path.ends_with("ls"), "{described:?}");
}

#[test]
fn command_big_v_describes_an_alias() {
    let out = sharky("alias hi='echo hello'; command -V hi");
    assert_eq!(stdout(&out), "hi is aliased to `echo hello'\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_big_v_reports_a_missing_name_on_stderr_and_fails() {
    let out = sharky("command -V definitely-not-a-command");
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("command: definitely-not-a-command: not found"),
        "{stderr:?}"
    );
    assert_eq!(status(&out), 1);
}

// A name containing a slash is only a command when it is an executable file.
// Bash never searches PATH for it and never reports a plain file, a directory
// or a symlink to a non-executable one.

fn temp_file(name: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = temp_dir().join(name);
    std::fs::write(&path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

#[test]
fn command_v_prints_the_path_of_an_executable_file() {
    let path = temp_file("probe.sh", 0o755);
    let out = sharky(&format!("command -v {}", path.display()));
    assert_eq!(stdout(&out), format!("{}\n", path.display()));
    assert_eq!(status(&out), 0);
}

#[test]
fn command_v_rejects_a_non_executable_file() {
    let path = temp_file("plain.sh", 0o644);
    let out = sharky(&format!("command -v {}", path.display()));
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn command_v_rejects_a_directory() {
    let dir = temp_dir();
    let out = sharky(&format!("command -v {}", dir.display()));
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn command_big_v_rejects_a_non_executable_file() {
    let path = temp_file("plain.sh", 0o644);
    let out = sharky(&format!("command -V {}", path.display()));
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("command: "), "{stderr:?}");
    assert!(stderr.contains("not found"), "{stderr:?}");
    assert_eq!(status(&out), 1);
}

#[test]
fn type_rejects_a_non_executable_file() {
    let path = temp_file("plain.sh", 0o644);
    let out = sharky(&format!("type {}", path.display()));
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("type: "), "{stderr:?}");
    assert!(stderr.contains("not found"), "{stderr:?}");
    assert_eq!(status(&out), 1);
}

#[test]
fn running_a_non_executable_file_reports_permission_denied() {
    let path = temp_file("plain.sh", 0o644);
    let out = sharky(&format!("{}", path.display()));
    assert_eq!(status(&out), 126);
}

#[test]
fn command_runs_a_builtin_when_no_function_shadows_it() {
    let out = sharky("command cd /; pwd");
    assert_eq!(stdout(&out), "/\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_bypasses_a_function_of_the_same_name() {
    let out = sharky(r#"printf() { echo FUNC; }; printf SHADOWED; command printf '[%s]' real"#);
    assert_eq!(stdout(&out), "FUNC\n[real]");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_falls_back_to_a_builtin_hidden_by_a_function() {
    let out = sharky("type() { echo FUNC; }; type cd; command type cd");
    assert_eq!(stdout(&out), "FUNC\ncd is a shell builtin\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn command_returns_the_status_of_the_command_it_runs() {
    let out = sharky("command false; printf 'st=%s' \"$?\"");
    assert_eq!(stdout(&out), "st=1");
}

#[test]
fn command_propagates_an_external_exit_status() {
    let out = sharky("command sh -c 'exit 7'; printf 'st=%s' \"$?\"");
    assert_eq!(stdout(&out), "st=7");
}

#[test]
fn command_reports_a_missing_command_with_status_127() {
    let out = sharky("command definitely-not-a-command; printf 'st=%s' \"$?\"");
    assert_eq!(stdout(&out), "st=127");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("command not found: definitely-not-a-command"),
        "{stderr:?}"
    );
}

#[test]
fn command_without_a_name_succeeds_and_prints_nothing() {
    for script in ["command", "command -p", "command -v", "command -V"] {
        let out = sharky(script);
        assert_eq!(stdout(&out), "", "{script}");
        assert_eq!(String::from_utf8_lossy(&out.stderr), "", "{script}");
        assert_eq!(status(&out), 0, "{script}");
    }
}

#[test]
fn command_rejects_an_unknown_option_with_status_two() {
    let out = sharky("command -z cd");
    assert_eq!(stdout(&out), "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("command: -z: invalid option"), "{stderr:?}");
    assert!(
        stderr.contains("usage: command [-pVv] command"),
        "{stderr:?}"
    );
    assert_eq!(status(&out), 2);
}

#[test]
fn command_double_dash_ends_the_options() {
    let out = sharky("command -- printf ok");
    assert_eq!(stdout(&out), "ok");
    assert_eq!(status(&out), 0);
}

// -p searches the shell's default PATH instead of the current one, while
// builtins and functions stay reachable without any PATH at all.

#[test]
fn command_dash_p_ignores_a_hostile_path() {
    let dir = temp_dir();
    let probe = dir.join("sharky-command-p-probe");
    std::fs::write(&probe, "#!/bin/sh\nexit 0\n").unwrap();
    let mut permissions = std::fs::metadata(&probe).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    permissions.set_mode(0o755);
    std::fs::set_permissions(&probe, permissions).unwrap();
    let path = dir.display().to_string();

    let out = sharky_with_path("command -v sharky-command-p-probe", &path);
    assert_eq!(stdout(&out), format!("{}\n", probe.display()));
    assert_eq!(status(&out), 0);

    let out = sharky_with_path("command -p -v sharky-command-p-probe", &path);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);

    let out = sharky_with_path("command -p sharky-command-p-probe", &path);
    assert_eq!(status(&out), 127);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("command not found: sharky-command-p-probe"),
        "{stderr:?}"
    );

    let out = sharky_with_path("command -p -v cd; command -pv cd", &path);
    assert_eq!(stdout(&out), "cd\ncd\n");
    assert_eq!(status(&out), 0);

    // An external that only the default PATH can find still runs with -p.
    let out = sharky_with_path("command -p sh -c 'printf p'", "/nowhere");
    assert_eq!(stdout(&out), "p");
    assert_eq!(status(&out), 0);

    let out = sharky_with_path("command sh -c 'printf p'", "/nowhere");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 127);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn command_dash_p_still_runs_builtins_under_a_hostile_path() {
    let out = sharky_with_path("command -p cd /; pwd; command -p false || pwd", "/nowhere");
    assert_eq!(stdout(&out), "/\n/\n");
    assert_eq!(status(&out), 0);
}

// `type` describes a name the way `command -V` does; for a function it also
// prints the definition in the layout `declare -f` uses.

#[test]
fn type_keeps_its_formatting_for_every_kind_of_name() {
    let out = sharky(
        r#"probe() { printf '%s' one; printf '%s' two; }; alias hi='echo hello'; type cd; type probe; type hi; type ls; type definitely-not-a-command; echo "st=$?""#,
    );
    let text = stdout(&out);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "cd is a shell builtin");
    assert_eq!(lines[1], "probe is a function");
    assert_eq!(lines[2], "probe () ");
    assert_eq!(lines[3], "{ ");
    assert_eq!(lines[4], "    printf '%s' one;");
    assert_eq!(lines[5], "    printf '%s' two");
    assert_eq!(lines[6], "}");
    assert_eq!(lines[7], "hi is aliased to `echo hello'");
    assert!(lines[8].starts_with("ls is /"), "{text:?}");
    assert_eq!(lines[9], "st=1");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("type: definitely-not-a-command: not found"),
        "{stderr:?}"
    );
}

#[test]
fn type_prints_a_function_definition_after_the_description() {
    let out = sharky("probe() { :; }; type probe");
    assert_eq!(
        stdout(&out),
        "probe is a function\nprobe () \n{ \n    :\n}\n"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn type_with_no_arguments_is_silent_and_succeeds() {
    let out = sharky("type; echo \"st=$?\"");
    assert_eq!(stdout(&out), "st=0\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 0);
}

#[test]
fn type_treats_a_leading_double_dash_as_end_of_options() {
    let out = sharky("type --; echo \"st=$?\"");
    assert_eq!(stdout(&out), "st=0\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
    assert_eq!(status(&out), 0);
}

#[test]
fn type_leading_double_dash_still_reports_following_names() {
    let out = sharky("type -- cd; echo \"st=$?\"");
    assert_eq!(stdout(&out), "cd is a shell builtin\nst=0\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn type_empty_string_argument_is_reported_as_not_found() {
    let out = sharky("type ''; echo \"st=$?\"");
    assert_eq!(stdout(&out), "st=1\n");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("type: : not found"),
        "{:?}",
        out.stderr
    );
    assert_eq!(status(&out), 0);
}

// ---------------------------------------------------------------------------
// echo and printf
//
// Both are builtins, so these cases compare the shell's own output with bytes
// measured from bash 5.3.20: the same stdout and the same exit status for each
// snippet, down to NUL bytes and raw UTF-8. The tuple is (script, stdout,
// status).
// ---------------------------------------------------------------------------

const ECHO_CASES: &[(&str, &[u8], i32)] = &[
    ("echo", b"\n", 0),
    ("echo hello", b"hello\n", 0),
    ("echo -n hello", b"hello", 0),
    ("echo -n", b"", 0),
    ("echo -e", b"\n", 0),
    ("echo a b c", b"a b c\n", 0),
    ("echo one two three", b"one two three\n", 0),
    ("echo -n -n hi", b"hi", 0),
    ("echo -- hello", b"-- hello\n", 0),
    ("echo - hello", b"- hello\n", 0),
    ("echo ---", b"---\n", 0),
    ("echo -x hi", b"-x hi\n", 0),
    ("echo -nex hi", b"-nex hi\n", 0),
    ("echo -e 'a\\tb'", b"a\tb\n", 0),
    ("echo 'a\\tb'", b"a\\tb\n", 0),
    ("echo -E 'a\\tb'", b"a\\tb\n", 0),
    ("echo -ne 'a\\tb'", b"a\tb", 0),
    ("echo -en 'a\\tb'", b"a\tb", 0),
    ("echo -e -n 'a\\tb'", b"a\tb", 0),
    ("echo -nEe 'a\\tb'", b"a\tb", 0),
    ("echo -eE 'a\\tb'", b"a\\tb\n", 0),
    ("echo -Ee 'a\\tb'", b"a\tb\n", 0),
    ("echo -e 'a\\eb\\Ec'", b"a\x1bb\x1bc\n", 0),
    ("echo -e 'a\\ab\\bb\\fb\\nb\\rb\\tb\\vb'", b"a\x07b\x08b\x0cb\nb\x0db\tb\x0bb\n", 0),
    ("echo -e 'a\\\\b'", b"a\\b\n", 0),
    ("echo -e 'a\\cb'; echo after", b"aafter\n", 0),
    ("echo -e one two '\\c' three", b"one two ", 0),
    ("echo -e 'a\\0a'", b"a\x00a\n", 0),
    ("echo -e 'a\\0101b'", b"aAb\n", 0),
    ("echo -e 'a\\0400b'", b"a\x00b\n", 0),
    ("echo -e 'a\\08b'", b"a\x008b\n", 0),
    ("echo -e 'a\\x41b'", b"aAb\n", 0),
    ("echo -e 'a\\x4b'", b"aK\n", 0),
    ("echo -e 'a\\x411b'", b"aA1b\n", 0),
    ("echo -e 'a\\x'", b"a\\x\n", 0),
    ("echo -e 'a\\xZZb'", b"a\\xZZb\n", 0),
    ("echo -e 'a\\101b'", b"a\\101b\n", 0),
    ("echo -e 'a\\7b'", b"a\\7b\n", 0),
    ("echo -e 'a\\q'", b"a\\q\n", 0),
    ("echo -e 'a\\u0041b'", b"aAb\n", 0),
    ("echo -e 'a\\u41b'", b"a\xd0\x9b\n", 0),
    ("echo -e 'a\\U0001F600b'", b"a\xf0\x9f\x98\x80b\n", 0),
    ("echo -e 'a\\uZZZZb'", b"a\\uZZZZb\n", 0),
    ("echo -e 'a\\U00110000b'", b"a\xf4\x90\x80\x80b\n", 0),
    ("echo -e 'trailing\\'", b"trailing\\\n", 0),
    ("echo -e '' x", b" x\n", 0),
    ("echo \"\"", b"\n", 0),
    ("echo -e 'a\\tb' -n", b"a\tb -n\n", 0),
];

#[test]
fn echo_matches_bash_for_flags_and_escapes() {
    for (script, expected, code) in ECHO_CASES {
        let out = sharky(script);
        assert_eq!(out.stdout, *expected, "{script}");
        assert_eq!(status(&out), *code, "{script}");
    }
}

const PRINTF_FORMAT_CASES: &[(&str, &[u8], i32)] = &[
    ("printf hello", b"hello", 0),
    ("printf \"\"", b"", 0),
    ("printf 'abc\\n'", b"abc\n", 0),
    ("printf '%s' abc", b"abc", 0),
    ("printf '%s\\n' a b c", b"a\nb\nc\n", 0),
    ("printf '%s-%s\\n' a b c", b"a-b\nc-\n", 0),
    ("printf '%s %s\\n' a b c d e", b"a b\nc d\ne \n", 0),
    ("printf '%d\\n'", b"0\n", 0),
    ("printf '%d %d\\n' 1", b"1 0\n", 0),
    ("printf '%s|%s\\n' a", b"a|\n", 0),
    ("printf '%s' a b", b"ab", 0),
    ("printf 'abc\\n' x y", b"abc\n", 0),
    ("printf '%%' a b", b"%", 0),
    ("printf 'x%%y\\n'", b"x%y\n", 0),
    ("printf '%s' ", b"", 0),
    ("printf '[%c]' abc", b"[a]", 0),
    ("printf '[%c]' ''", b"[\x00]", 0),
    ("printf '[%c]' 65", b"[6]", 0),
    ("printf '[%5c]' a", b"[    a]", 0),
    ("printf '[%-5c]' a", b"[a    ]", 0),
    ("printf '[%08c]' a", b"[       a]", 0),
    ("printf '[%5s]' ab", b"[   ab]", 0),
    ("printf '[%-5s]' ab", b"[ab   ]", 0),
    ("printf '[%05d]' 42", b"[00042]", 0),
    ("printf '[%+d]' 42", b"[+42]", 0),
    ("printf '[%+d]' -42", b"[-42]", 0),
    ("printf '[% d]' 42", b"[ 42]", 0),
    ("printf '[% d]' -42", b"[-42]", 0),
    ("printf '[%#x]' 255", b"[0xff]", 0),
    ("printf '[%#o]' 8", b"[010]", 0),
    ("printf '[%#o]' 0", b"[0]", 0),
    ("printf '[%#x]' 0", b"[0]", 0),
    ("printf '[%x]' 255", b"[ff]", 0),
    ("printf '[%X]' 255", b"[FF]", 0),
    ("printf '[%u]' -1", b"[18446744073709551615]", 0),
    ("printf '[%u]' 42", b"[42]", 0),
    ("printf '[%o]' 8", b"[10]", 0),
    ("printf '[%i]' -7", b"[-7]", 0),
    ("printf '[%#8x]' 255", b"[    0xff]", 0),
    ("printf '[%#08x]' 255", b"[0x0000ff]", 0),
    ("printf '[%.4d]' 42", b"[0042]", 0),
    ("printf '[%8.4d]' 42", b"[    0042]", 0),
    ("printf '[%-8.4d]' 42", b"[0042    ]", 0),
    ("printf '[%08.4d]' 42", b"[    0042]", 0),
    ("printf '[%#8.4x]' 255", b"[  0x00ff]", 0),
    ("printf '[%.3d]' 5", b"[005]", 0),
    ("printf '[%5.3d]' 5", b"[  005]", 0),
    ("printf '[%.0d]' 0", b"[]", 0),
    ("printf '[%#.3o]' 8", b"[010]", 0),
    ("printf '[%+x]' 255", b"[ff]", 0),
    ("printf '[%.2s]' abcdef", b"[ab]", 0),
    ("printf '[%10.3s]' abcdef", b"[       abc]", 0),
    ("printf '[%-10.3s]' abcdef", b"[abc       ]", 0),
    ("printf '[%.2s]' 'ação'", b"[a\xc3]", 0),
    ("printf '[%.0s]' ab", b"[]", 0),
    ("printf '[%*d]' 5 42", b"[   42]", 0),
    ("printf '[%*d]' -5 42", b"[42   ]", 0),
    ("printf '[%-*d]' 6 42", b"[42    ]", 0),
    ("printf '[%.*f]' 2 1.234", b"[1.23]", 0),
    ("printf '[%*.*f]' 10 2 1.23456", b"[      1.23]", 0),
    ("printf '[%.*d]' 3 5", b"[005]", 0),
    ("printf '[%*.*s]' 8 2 abcdef", b"[      ab]", 0),
    ("printf '[%.*s]' -1 abcdef", b"[abcdef]", 0),
    ("printf '[%*s]' -3 ab", b"[ab ]", 0),
    ("printf '[%*d]' ; echo \"st=$?\"", b"[0]st=0\n", 0),
    ("printf '[%f]' 1.5", b"[1.500000]", 0),
    ("printf '[%.2f]' 1.567", b"[1.57]", 0),
    ("printf '[%f]' 1.5 2.5", b"[1.500000][2.500000]", 0),
    ("printf '[%e]' 1500", b"[1.500000e+03]", 0),
    ("printf '[%.2e]' 1500", b"[1.50e+03]", 0),
    ("printf '[%E]' 1500", b"[1.500000E+03]", 0),
    ("printf '[%g]' 1500", b"[1500]", 0),
    ("printf '[%g]' 0", b"[0]", 0),
    ("printf '[%g]' 0.0001", b"[0.0001]", 0),
    ("printf '[%g]' 0.00001", b"[1e-05]", 0),
    ("printf '[%g]' 1234567", b"[1.23457e+06]", 0),
    ("printf '[%g]' 100000", b"[100000]", 0),
    ("printf '[%g]' 1000000", b"[1e+06]", 0),
    ("printf '[%g]' 1.23456789", b"[1.23457]", 0),
    ("printf '[%G]' 0.00015", b"[0.00015]", 0),
    ("printf '[%#g]' 1.5", b"[1.50000]", 0),
    ("printf '[%#g]' 100000", b"[100000.]", 0),
    ("printf '[%#g]' 0", b"[0.00000]", 0),
    ("printf '[%.3g]' 1234.5", b"[1.23e+03]", 0),
    ("printf '[%.3g]' 12345", b"[1.23e+04]", 0),
    ("printf '[%.1g]' 9.99", b"[1e+01]", 0),
    ("printf '[%.2g]' 0.0001234", b"[0.00012]", 0),
    ("printf '[%f]' inf", b"[inf]", 0),
    ("printf '[%F]' inf", b"[INF]", 0),
    ("printf '[%f]' nan", b"[nan]", 0),
    ("printf '[%G]' nan", b"[NAN]", 0),
    ("printf '[%f]' -0", b"[-0.000000]", 0),
    ("printf '[%g]' -0", b"[-0]", 0),
    ("printf '[%e]' 0", b"[0.000000e+00]", 0),
    ("printf '[%e]' -0.0", b"[-0.000000e+00]", 0),
    ("printf '[%f]' 0x10", b"[16.000000]", 0),
    ("printf '[%f]' 0x1.8p1", b"[3.000000]", 0),
    ("printf '[%.0f]' 2.5", b"[2]", 0),
    ("printf '[%.0f]' 3.5", b"[4]", 0),
    ("printf '[%8.2f]' 1.5", b"[    1.50]", 0),
    ("printf '[%08.2f]' -1.5", b"[-0001.50]", 0),
    ("printf '[%-8.2f]' 1.5", b"[1.50    ]", 0),
    ("printf '[%+8.2f]' 1.5", b"[   +1.50]", 0),
    ("printf '[% 8.2e]' 1.5", b"[ 1.50e+00]", 0),
    ("printf '[%08.2e]' 1.5", b"[1.50e+00]", 0),
    ("printf '[%8.3g]' 1.5", b"[     1.5]", 0),
    ("printf '[%#.0e]' 1.5", b"[2.e+00]", 0),
    ("printf '[%#.0f]' 1.5", b"[2.]", 0),
    ("printf '[%q]' 'a b'", b"[a\\ b]", 0),
    ("printf '[%q]' ''", b"['']", 0),
    ("printf \"[%q]\" \"it's\"", b"[it\\'s]", 0),
    ("printf '[%q]' 'a*b~#!?[]{};&|<>()`$,'", b"[a\\*b~#\\!\\?\\[\\]\\{\\}\\;\\&\\|\\<\\>\\(\\)\\`\\$\\,]", 0),
    ("printf '[%q]' 'a.b-c_d/e:f@g+h=i%j~k#l'", b"[a.b-c_d/e:f@g+h=i%j~k#l]", 0),
    ("printf '[%q]' $'a\\nb'", b"[$'a\\nb']", 0),
    ("printf '[%q]' $'a\\tb'", b"[$'a\\tb']", 0),
    ("printf '[%q]' $'a\\rb'", b"[$'a\\rb']", 0),
    ("printf '[%q]' 'ação'", b"[a\xc3\xa7\xc3\xa3o]", 0),
    ("printf '[%q]' '\\'", b"[\\\\]", 0),
    ("printf '[%8q]' 'ab'", b"[      ab]", 0),
    ("printf '[%.2q]' 'abc'", b"[ab]", 0),
    ("printf '[%5.1q]' 'abc def'", b"[    a]", 0),
    ("printf '[%8b]' 'a b'", b"[     a b]", 0),
    ("printf '[%-8b]' 'a b'", b"[a b     ]", 0),
    ("printf '[%08b]' 'a b'", b"[     a b]", 0),
    ("printf '[%.2b]' 'a b'", b"[a ]", 0),
    ("printf '[%b]' 'a\\tb'", b"[a\tb]", 0),
    ("printf '[%b]' 'a\\\\b'", b"[a\\b]", 0),
    ("printf '[%b]' 'a\\123b'", b"[aSb]", 0),
    ("printf '[%b]' 'a\\1b'", b"[a\x01b]", 0),
    ("printf '[%b]' 'a\\0777b'", b"[a\xffb]", 0),
    ("printf '[%b]' 'a\\777b'", b"[a\xffb]", 0),
    ("printf '[%b]' 'a\\0b'", b"[a\x00b]", 0),
    ("printf '[%b]' 'a\\08b'", b"[a\x008b]", 0),
    ("printf '[%b]' 'a\\0101b'", b"[aAb]", 0),
    ("printf '[%b]' 'a\\x41b'", b"[aAb]", 0),
    ("printf '[%b]' 'a\\xb'", b"[a\x0b]", 0),
    ("printf '[%b]' 'a\\u0041b'", b"[aAb]", 0),
    ("printf '[%b]' 'a\\zb'", b"[a\\zb]", 0),
    ("printf '%b' 'a\\cb'; echo next", b"anext\n", 0),
    ("printf 'x%by' 'a\\c' 'b'; echo next", b"xanext\n", 0),
    ("printf '%b' 'trailing\\'", b"trailing\\", 0),
    ("printf '%s\\n' 'a\\cb'", b"a\\cb\n", 0),
    ("printf '[%n]' x; echo \"st=$?\"", b"[]st=0\n", 0),
    ("printf '%n%s' a b", b"b", 0),
    ("printf '[%ld]' 5", b"[5]", 0),
    ("printf '[%lld]' 5", b"[5]", 0),
    ("printf '[%hd]' 5", b"[5]", 0),
    ("printf '[%Lf]' 1.5", b"[1.500000]", 0),
    ("printf '[%zu]' 5", b"[5]", 0),
    ("printf '[%jd]' 5", b"[5]", 0),
    ("printf '[%'\"'\"'d]' 1234", b"[1234]", 0),
    ("printf 'a\\141b\\n'", b"aab\n", 0),
    ("printf 'a\\0b\\n'", b"a\x00b\n", 0),
    ("printf 'a\\0101b\\n'", b"a\x081b\n", 0),
    ("printf 'a\\1b\\n'", b"a\x01b\n", 0),
    ("printf 'a\\12b\\n'", b"a\nb\n", 0),
    ("printf 'a\\777b\\n'", b"a\xffb\n", 0),
    ("printf 'a\\400b\\n'", b"a\x00b\n", 0),
    ("printf '\\x41\\x4a\\n'", b"AJ\n", 0),
    ("printf 'a\\xZZ\\n'", b"a\\xZZ\n", 0),
    ("printf 'a\\tb\\vc\\ed\\n'", b"a\tb\x0bc\x1bd\n", 0),
    ("printf 'a\\db\\n'", b"a\\db\n", 0),
    ("printf 'a\\%zb'; echo \"st=$?\"", b"a\\st=0\n", 0),
    ("printf 'a\\%sb' x", b"a\\xb", 0),
    ("printf '\\045s' x; echo", b"%s\n", 0),
    ("printf \"a\\\\'b\\n\"", b"a'b\n", 0),
    ("printf 'a\\\"b\\n'", b"a\"b\n", 0),
    ("printf 'a\\?b\\n'", b"a?b\n", 0),
    ("printf 'a\\ b\\n'", b"a\\ b\n", 0),
    ("printf 'trailing\\'", b"trailing\\", 0),
    ("printf -v x hi; echo \"x=$x\"", b"x=hi\n", 0),
    ("printf -v x '%s-%s' a b; echo \"x=$x\"", b"x=a-b\n", 0),
    ("printf -v x '%d' 5; echo \"x=$x\"", b"x=5\n", 0),
    ("printf -- '%s\\n' x", b"x\n", 0),
    ("printf -- ; echo \"st=$?\"", b"st=2\n", 0),
    ("printf '-x'; echo \"st=$?\"", b"st=2\n", 0),
    ("printf -x; echo \"st=$?\"", b"st=2\n", 0),
    ("printf -v 'bad-name' hi; echo \"st=$?\"", b"st=2\n", 0),
    ("printf -v 2x hi; echo \"st=$?\"", b"st=2\n", 0),
    ("printf '[%.0c]' X", b"[X]", 0),
    ("printf '[%.2c]' X", b"[X]", 0),
    ("printf '[%.1c]' X", b"[X]", 0),
    ("printf '%c' 'ação'", b"a", 0),
    ("printf '%.0b]' 'a'", b"]", 0),
    ("printf '[%-8.2b]' 'abcd'", b"[ab      ]", 0),
    ("printf '[%8.2q]' 'a b c'", b"[      a\\]", 0),
    ("printf '[%c]' 0x41", b"[0]", 0),
    ("printf '[%5.2c]' 'ab'", b"[    a]", 0),
];

#[test]
fn printf_matches_bash_for_formats_and_conversions() {
    for (script, expected, code) in PRINTF_FORMAT_CASES {
        let out = sharky(script);
        assert_eq!(out.stdout, *expected, "{script}");
        assert_eq!(status(&out), *code, "{script}");
    }
}

const PRINTF_NUMBER_CASES: &[(&str, &[u8], i32)] = &[
    ("printf '[%d]' 0x10", b"[16]", 0),
    ("printf '[%d]' 010", b"[8]", 0),
    ("printf '[%d]' abc; echo \"st=$?\"", b"[0]st=1\n", 0),
    ("printf '[%d]' 12abc; echo \"st=$?\"", b"[12]st=1\n", 0),
    ("printf '[%d]' ' 5'; echo \"st=$?\"", b"[5]st=0\n", 0),
    ("printf '[%d]' '5 '; echo \"st=$?\"", b"[5]st=1\n", 0),
    ("printf '[%d]' +5", b"[5]", 0),
    ("printf '[%d]' -0x10", b"[-16]", 0),
    ("printf '[%i]' 1e3; echo \"st=$?\"", b"[1]st=1\n", 0),
    ("printf '[%d]' 99999999999999999999; echo \"st=$?\"", b"[9223372036854775807]st=1\n", 0),
    ("printf '[%d]' 0x7fffffffffffffff", b"[9223372036854775807]", 0),
    ("printf '[%d]' 0x8000000000000000; echo \"st=$?\"", b"[9223372036854775807]st=1\n", 0),
    ("printf '[%d]' -9223372036854775808", b"[-9223372036854775808]", 0),
    ("printf '[%u]' 18446744073709551615", b"[18446744073709551615]", 0),
    ("printf '[%u]' 18446744073709551616; echo \"st=$?\"", b"[18446744073709551615]st=1\n", 0),
    ("printf '[%x]' 9223372036854775807", b"[7fffffffffffffff]", 0),
    ("printf '[%d]' 0b101", b"[5]", 0),
    ("printf '[%d]' 0B11", b"[3]", 0),
    ("printf '[%d]' 0b2; echo \"st=$?\"", b"[0]st=1\n", 0),
    ("printf '[%d]' 08; echo \"st=$?\"", b"[0]st=1\n", 0),
    ("printf '[%d]' \"'x'\"", b"[120]", 0),
    ("printf '[%d]' \"'é'\"", b"[233]", 0),
    ("printf '[%d]' '\"ab\"'", b"[97]", 0),
    ("printf '[%d]' \"'\"", b"[0]", 0),
    ("printf '[%d]' ''; echo \"st=$?\"", b"[0]st=1\n", 0),
    ("printf '[%d]' 4 2; echo \"st=$?\"", b"[4][2]st=0\n", 0),
    ("printf '[%f]' 5x; echo \"st=$?\"", b"[5.000000]st=1\n", 0),
    ("printf '[%f]' ''; echo \"st=$?\"", b"[0.000000]st=1\n", 0),
    ("printf '[%f]' .5", b"[0.500000]", 0),
    ("printf '[%f]' '5.'", b"[5.000000]", 0),
    ("printf '[%f]' '1.5x'; echo \"st=$?\"", b"[1.500000]st=1\n", 0),
    ("printf '[%f]' ' 1.5 '; echo \"st=$?\"", b"[1.500000]st=1\n", 0),
    ("printf '[%f]' Infinity", b"[inf]", 0),
    ("printf '[%f]' nAn", b"[nan]", 0),
    ("printf 'a%y' x; echo \"st=$?\"", b"ast=1\n", 0),
    ("printf '%' x; echo \"st=$?\"", b"st=1\n", 0),
    ("printf '%5' x; echo \"st=$?\"", b"st=1\n", 0),
    ("printf 'a%'; echo \"st=$?\"", b"ast=1\n", 0),
    ("printf '%'; echo \"st=$?\"", b"st=1\n", 0),
    ("printf '[%5%]' x; echo \"st=$?\"", b"[st=1\n", 0),
    ("printf 'X%-%' x; echo \"st=$?\"", b"Xst=1\n", 0),
    ("printf '[%I'\"'\"'d]' 1234; echo \"st=$?\"", b"[st=1\n", 0),
    ("printf; echo \"st=$?\"", b"st=2\n", 0),
];

#[test]
fn printf_matches_bash_for_numbers_and_failures() {
    for (script, expected, code) in PRINTF_NUMBER_CASES {
        let out = sharky(script);
        assert_eq!(out.stdout, *expected, "{script}");
        assert_eq!(status(&out), *code, "{script}");
    }
}

#[test]
fn echo_and_printf_resolve_as_builtins() {
    let out = sharky("type echo; type printf");
    assert_eq!(
        stdout(&out),
        "echo is a shell builtin\nprintf is a shell builtin\n"
    );
    assert_eq!(status(&out), 0);

    let out = sharky("command -v echo printf");
    assert_eq!(stdout(&out), "echo\nprintf\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn builtins_print_in_order_inside_pipelines_and_substitutions() {
    let out = sharky("printf '%s\\n' one two | cat");
    assert_eq!(stdout(&out), "one\ntwo\n");

    let out = sharky("x=$(printf '%s-%s' a b); printf '[%s]' \"$x\"");
    assert_eq!(stdout(&out), "[a-b]");

    let out = sharky("echo -n prefix; printf '%s' suffix; echo end");
    assert_eq!(stdout(&out), "prefixsuffixend\n");
}

#[test]
fn printf_writes_to_a_variable_with_dash_v() {
    let out = sharky("printf -v value '%s/%s' a b; printf '%s' \"$value\"");
    assert_eq!(stdout(&out), "a/b");
    assert_eq!(status(&out), 0);

    let out = sharky("printf -v 9bad hi");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 2);
}
