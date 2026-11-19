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
    assert_eq!(status(&out), 1);
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
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("v: boom"), "{stderr:?}");
}

#[test]
fn empty_error_word_uses_the_null_parameter_message() {
    let out = sharky(r#"unset v; printf '[%s]' "${v?}"; printf after"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("v: parameter null or not set"), "{stderr:?}");
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
    assert_eq!(status(&out), 1);
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
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("boom"), "{stderr:?}");
}

#[test]
fn expansion_error_in_case_runs_no_arm() {
    let out = sharky(r#"case ${SHARKY_UNSET_F?boom} in *) [[ -n x ]] ;; esac"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("boom"), "{stderr:?}");
}

#[test]
fn expansion_error_in_a_test_keeps_the_command_failure() {
    let out = sharky(r#"[[ -z ${SHARKY_UNSET_F?boom} ]]"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(String::from_utf8_lossy(&out.stderr).contains("boom"));
}

#[test]
fn nounset_in_a_for_word_list_runs_no_iteration() {
    let out = sharky(r#"set -u; for x in a$SHARKY_UNSET_F b; do [[ -n "$x" ]]; done"#);
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
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

