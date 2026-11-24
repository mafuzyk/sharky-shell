use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(source: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", source])
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

#[test]
fn declare_inside_function_is_local() {
    let out = sharky(
        r#"z=outer; f() { declare z=inner; printf 'in=%s\n' "$z"; }; f; printf 'out=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=inner\nout=outer\n");
}

#[test]
fn declare_inside_function_does_not_leak_after_return() {
    let out = sharky(r#"f() { declare z=inner; }; f; printf 'z=%s\n' "${z-UNSET}""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "z=UNSET\n");
}

#[test]
fn typeset_inside_function_is_local() {
    let out = sharky(
        r#"z=outer; f() { typeset z=inner; printf 'in=%s\n' "$z"; }; f; printf 'out=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=inner\nout=outer\n");
}

#[test]
fn declare_g_forces_global_from_a_function() {
    let out = sharky(r#"f() { declare -g z=inner; }; f; printf 'z=%s\n' "$z""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "z=inner\n");
}

#[test]
fn declare_g_writes_through_a_shadowing_local() {
    let out = sharky(r#"z=0; f() { local z=1; declare -g z=2; }; f; printf 'global=%s\n' "$z""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "global=2\n");
}

#[test]
fn declare_g_write_leaves_the_shadowing_local_visible() {
    // Bash keeps the function on its local; only the global changes.
    let out = sharky(
        r#"z=0; f() { local z=1; declare -g z=2; printf 'in=%s\n' "$z"; }; f; printf 'global=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=1\nglobal=2\n");
}

#[test]
fn declare_g_creates_the_global_behind_a_shadowing_local() {
    let out = sharky(
        r#"f() { local z=1; declare -g z=2; }; printf 'before=%s\n' "${z-UNSET}"; f; declare -p z"#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "before=UNSET\ndeclare -- z=\"2\"\n");
}

#[test]
fn declare_g_without_a_value_keeps_the_global_behind_a_local() {
    let out = sharky(
        r#"z=0; f() { local z=1; declare -g z; printf 'in=%s\n' "$z"; }; f; printf 'global=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=1\nglobal=0\n");
}

#[test]
fn declare_g_array_writes_through_a_shadowing_local() {
    let out = sharky(
        r#"f() { local -a a=(1); declare -g a=(2 3); printf 'in=%s\n' "${a[*]}"; }; f; declare -p a"#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=1\ndeclare -a a=([0]=\"2\" [1]=\"3\")\n");
}

#[test]
fn declare_g_array_conversion_writes_through_a_shadowing_local() {
    let out = sharky(r#"f() { local a=1; declare -g a=(2 3); }; f; declare -p a"#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "declare -a a=([0]=\"2\" [1]=\"3\")\n");
}

#[test]
fn declare_g_assoc_writes_through_a_shadowing_local() {
    let out = sharky(
        r#"f() { local -A m=([k]=1); declare -gA m=([k]=2); printf 'in=%s\n' "${m[k]}"; }; f; printf 'global=%s\n' "${m[k]-UNSET}""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "in=1\nglobal=2\n");
}

#[test]
fn declare_g_export_writes_through_a_shadowing_local() {
    let out = sharky(r#"f() { local x=1; declare -gx x=2; }; f; declare -p x"#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "declare -x x=\"2\"\n");
}

#[test]
fn declare_g_reaches_the_global_past_an_outer_local() {
    let out = sharky(
        r#"z=0; outer() { local z=1; inner() { declare -g z=9; printf 'inner=%s\n' "$z"; }; inner; printf 'outer=%s\n' "$z"; }; outer; printf 'global=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "inner=1\nouter=1\nglobal=9\n");
}

#[test]
fn declare_g_reaches_the_global_past_two_shadowing_locals() {
    let out = sharky(
        r#"z=0; outer() { local z=1; inner() { local z=5; declare -g z=9; printf 'inner=%s\n' "$z"; }; inner; printf 'outer=%s\n' "$z"; }; outer; printf 'global=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "inner=5\nouter=1\nglobal=9\n");
}

#[test]
fn top_level_declare_stays_global() {
    let out = sharky(r#"declare z=top; printf 'z=%s\n' "$z""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "z=top\n");
}

#[test]
fn nested_function_declare_does_not_override_outer_local() {
    let out = sharky(
        r#"z=outer; outer() { declare z=outerlocal; inner() { declare z=innerlocal; printf 'inner=%s\n' "$z"; }; inner; printf 'after=%s\n' "$z"; }; outer; printf 'top=%s\n' "$z""#,
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "inner=innerlocal\nafter=outerlocal\ntop=outer\n");
}

#[test]
fn declare_f_lists_all_functions_sorted() {
    let out = sharky(r#"zfn() { :; }; afn() { :; }; mfn() { :; }; declare -F; echo "st=$?""#);
    assert_eq!(status(&out), 0);
    assert_eq!(
        stdout(&out),
        "declare -f afn\ndeclare -f mfn\ndeclare -f zfn\nst=0\n"
    );
}

#[test]
fn declare_f_reports_known_names_and_unknown_names() {
    let out = sharky(r#"f() { :; }; g() { :; }; declare -F f nope g; echo "st=$?"; declare -F f; echo "st=$?""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "f\ng\nst=1\nf\nst=0\n");
}

#[test]
fn declare_f_unknown_name_alone_fails() {
    let out = sharky(r#"declare -F nosuchfn; echo "st=$?""#);
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "st=1\n");
}

#[test]
fn declare_p_still_prints_existing_variables_and_fails_for_unknown() {
    let out = sharky("declare -p PATH >/dev/null; echo \"path_st=$?\"; declare -p nosuchvar 2>/dev/null; echo \"missing_st=$?\"");
    assert_eq!(status(&out), 0);
    assert_eq!(stdout(&out), "path_st=0\nmissing_st=1\n");
}
