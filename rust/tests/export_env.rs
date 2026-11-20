//! Export attributes for scalar variables, plus the `SHLVL` and `_` special
//! variables.
//!
//! Every expectation here was measured against bash 5.3.20 with a cleared
//! environment: a plain assignment stays inside the shell, only `export` (or
//! an inherited variable, or a temporary `VAR=val cmd` prefix) reaches a
//! child, `SHLVL` starts at the inherited value plus one and is exported, and
//! `_` carries the last argument of the previous simple command.

use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-export-env-test";
const PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";

fn sharky_with(env: &[(&str, &str)], script: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
    command
        .args(["--no-config", "-c", script])
        .env_clear()
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("PATH", PATH);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("failed to run sharky")
}

fn sharky(script: &str) -> Output {
    sharky_with(&[], script)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn env_value(out: &Output, name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    stdout(out)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string))
}

// A plain assignment is a shell variable, not part of the child environment.
#[test]
fn plain_assignment_does_not_reach_a_child() {
    let out = sharky("PLAIN_ONLY=value; env");
    assert_eq!(env_value(&out, "PLAIN_ONLY"), None, "{out:?}");
}

#[test]
fn export_marks_a_variable_for_children() {
    let out = sharky("EXPORTED=value; export EXPORTED; env");
    assert_eq!(
        env_value(&out, "EXPORTED").as_deref(),
        Some("value"),
        "{out:?}"
    );
}

#[test]
fn export_without_a_value_keeps_the_current_value() {
    let out = sharky("MARKED=value; export MARKED; env");
    assert_eq!(
        env_value(&out, "MARKED").as_deref(),
        Some("value"),
        "{out:?}"
    );
}

#[test]
fn a_plain_assignment_keeps_an_existing_export_attribute() {
    let out = sharky("KEEP=one; export KEEP; KEEP=two; env");
    assert_eq!(env_value(&out, "KEEP").as_deref(), Some("two"), "{out:?}");
}

#[test]
fn inherited_variables_stay_exported() {
    let out = sharky_with(&[("INHERITED", "yes")], "env");
    assert_eq!(
        env_value(&out, "INHERITED").as_deref(),
        Some("yes"),
        "{out:?}"
    );
}

#[test]
fn export_n_clears_the_attribute_without_changing_the_value() {
    let out = sharky("DROPPED=value; export DROPPED; export -n DROPPED; env; declare -p DROPPED");
    assert_eq!(env_value(&out, "DROPPED"), None, "{out:?}");
    assert!(
        stdout(&out).contains("declare -- DROPPED=\"value\""),
        "{out:?}"
    );
}

#[test]
fn a_dropped_attribute_survives_a_plain_assignment() {
    let out = sharky("DROPPED=one; export DROPPED; export -n DROPPED; DROPPED=two; env");
    assert_eq!(env_value(&out, "DROPPED"), None, "{out:?}");
}

#[test]
fn a_prefix_assignment_reaches_only_that_command() {
    let out = sharky_with(
        &[],
        r#"PREFIX=1 sh -c 'printf "<%s>" "${PREFIX-UNSET}"'; printf '[%s]' "${PREFIX-UNSET}""#,
    );
    assert_eq!(stdout(&out), "<1>[UNSET]", "{out:?}");
}

#[test]
fn export_p_lists_only_exported_variables() {
    let out = sharky("PLAIN_ONE=1; PUBLIC_ONE=2; export PUBLIC_ONE; export -p");
    assert!(
        stdout(&out)
            .lines()
            .any(|line| line == "declare -x PUBLIC_ONE=\"2\""),
        "{out:?}"
    );
    assert!(
        !stdout(&out).lines().any(|line| line.contains("PLAIN_ONE")),
        "{out:?}"
    );
}

#[test]
fn export_p_lists_an_exported_name_without_a_value() {
    let out = sharky("export EMPTY_EXPORTED; export -p");
    assert!(
        stdout(&out)
            .lines()
            .any(|line| line == "declare -x EMPTY_EXPORTED"),
        "{out:?}"
    );
}

#[test]
fn declare_x_marks_an_existing_variable() {
    let out = sharky("DECLARED=value; declare -x DECLARED; env");
    assert_eq!(
        env_value(&out, "DECLARED").as_deref(),
        Some("value"),
        "{out:?}"
    );
}

#[test]
fn declare_x_with_a_value_sets_and_marks_it() {
    let out = sharky("declare -x DECLARED_WITH_VALUE=two; env");
    assert_eq!(
        env_value(&out, "DECLARED_WITH_VALUE").as_deref(),
        Some("two"),
        "{out:?}"
    );
}

#[test]
fn unset_removes_the_export_attribute() {
    let out = sharky("GONE=1; export GONE; unset GONE; export -p");
    assert!(
        !stdout(&out).lines().any(|line| line.contains("GONE")),
        "{out:?}"
    );
}

#[test]
fn getopts_state_does_not_leak_into_children() {
    let out = sharky("set -- -a; getopts 'a' o; env; echo");
    assert_eq!(env_value(&out, "OPTIND"), None, "{out:?}");
    assert_eq!(env_value(&out, "OPTARG"), None, "{out:?}");
}

// `SHLVL` starts one above the inherited value (or at 1 when unset) and is
// exported. The final command of a `-c` string is exec'd without a fork by
// bash and therefore exports one less, which the login acceptance observes as
// SHLVL=0.
#[test]
fn shlvl_defaults_to_one_when_unset() {
    let out = sharky("printf '%s' \"$SHLVL\"");
    assert_eq!(stdout(&out), "1", "{out:?}");
}

#[test]
fn shlvl_increments_an_inherited_value() {
    let out = sharky_with(&[("SHLVL", "7")], "printf '%s' \"$SHLVL\"");
    assert_eq!(stdout(&out), "8", "{out:?}");
}

#[test]
fn shlvl_is_exported_to_a_child() {
    let out = sharky("env; echo");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("1"), "{out:?}");
}

#[test]
fn shlvl_final_command_exports_one_less() {
    let out = sharky("env");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("0"), "{out:?}");
}

#[test]
fn shlvl_assignment_is_exported_unchanged_to_a_forked_child() {
    let out = sharky("SHLVL=9; env; echo");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("9"), "{out:?}");
}

#[test]
fn shlvl_final_command_adjusts_an_assigned_value() {
    let out = sharky("SHLVL=9; env");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("8"), "{out:?}");
}

#[test]
fn shlvl_final_command_never_exports_a_negative_level() {
    let out = sharky("SHLVL=0; env");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("0"), "{out:?}");
    let out = sharky("SHLVL=-3; env");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("0"), "{out:?}");
}

// `_` is the last argument of the previous simple command. Pipelines and
// subshells do not update it in the parent. A child environment gets the
// resolved path of the command instead.
#[test]
fn underscore_tracks_the_last_argument() {
    let out = sharky("echo one two; printf '[%s]' \"$_\"");
    assert_eq!(stdout(&out), "one two\n[two]", "{out:?}");
}

#[test]
fn underscore_is_empty_after_an_assignment_only_command() {
    let out = sharky("x=1; printf '[%s]' \"$_\"");
    assert_eq!(stdout(&out), "[]", "{out:?}");
}

#[test]
fn underscore_keeps_the_previous_value_after_a_pipeline() {
    let out = sharky("echo one two; echo a | cat; printf '[%s]' \"$_\"");
    assert_eq!(stdout(&out), "one two\na\n[two]", "{out:?}");
}

#[test]
fn underscore_keeps_the_command_word_after_a_failed_lookup() {
    let out = sharky("sharky_missing_command 2>/dev/null; printf '[%s]' \"$_\"");
    assert_eq!(stdout(&out), "[sharky_missing_command]", "{out:?}");
}

#[test]
fn underscore_in_a_child_environment_is_the_resolved_command_path() {
    let ambient_path = std::env::var("PATH").unwrap_or_default();
    let path = if ambient_path.is_empty() {
        PATH.to_string()
    } else {
        ambient_path
    };

    // Resolve `env` the same way the shell does: the first PATH entry that
    // holds a regular, executable file named `env` wins.
    let expected = path
        .split(':')
        .filter(|entry| !entry.is_empty())
        .map(std::path::PathBuf::from)
        .map(|dir| dir.join("env"))
        .find(|candidate| {
            std::fs::metadata(candidate)
                .map(|meta| {
                    let mode = std::os::unix::fs::PermissionsExt::mode(&meta.permissions());
                    meta.is_file() && mode & 0o111 != 0
                })
                .unwrap_or(false)
        })
        .expect("no executable `env` found in PATH");
    let expected = expected.to_str().expect("PATH entry is not valid UTF-8");

    let out = sharky_with(&[("PATH", &path)], "env; echo");
    assert_eq!(
        env_value(&out, "_").as_deref(),
        Some(expected),
        "expected _ to be the PATH-resolved env ({expected}); {out:?}"
    );
}

#[test]
fn underscore_keeps_an_inherited_value_until_a_command_runs() {
    let out = sharky_with(&[("_", "/given/path")], "printf '[%s]' \"$_\"");
    assert_eq!(stdout(&out), "[/given/path]", "{out:?}");
}

#[test]
fn underscore_is_never_exported() {
    let out = sharky("echo one two; export -p");
    assert!(
        !stdout(&out)
            .lines()
            .any(|line| line.starts_with("declare -x _=")),
        "{out:?}"
    );
}

// The explicit `exec` builtin exports one shell level less and does not rewrite
// `_`, except inside a subshell where bash keeps the normal export value.
#[test]
fn shlvl_exec_exports_one_less_without_underscore() {
    let out = sharky("exec env");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("0"), "{out:?}");
    assert_eq!(env_value(&out, "_"), None, "{out:?}");
}

#[test]
fn shlvl_exec_inside_a_subshell_keeps_the_level() {
    let out = sharky("( exec env )");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("1"), "{out:?}");
}

#[test]
fn shlvl_process_substitution_exports_one_less() {
    let out = sharky("cat <(env)");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("0"), "{out:?}");
}

#[test]
fn shlvl_process_substitution_inside_a_pipeline_keeps_the_level() {
    let out = sharky("cat <(env) | cat");
    assert_eq!(env_value(&out, "SHLVL").as_deref(), Some("1"), "{out:?}");
}
