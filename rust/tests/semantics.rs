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
