//! `return` scope and errexit inside command substitution.
//!
//! Two context boundaries bash honours and the reference shell must match:
//!
//! * `return` leaves a function or the sourced file it runs in, never the
//!   script that sourced it. Outside both it is an error that reports status 2
//!   and lets the current input unit continue.
//! * With `set -e` and without `shopt -s inherit_errexit`, errexit is disabled
//!   inside `$(...)`; the substitution keeps only its own final status, which
//!   the enclosing assignment then reports.

use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(files: &[(&str, &str)]) -> Sandbox {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sharky-return-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        Sandbox { dir }
    }

    fn sharky(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .arg("--no-config")
            .args(args)
            .current_dir(&self.dir)
            .env("SHARKY_CONFIG", NO_CONFIG)
            .output()
            .expect("failed to run sharky")
    }

    fn run(&self, script: &str) -> Output {
        self.sharky(&["-c", script])
    }

    /// Runs a script file from the sandbox, the way bash treats a script named
    /// on its command line: not a sourced file, so `return` is an error there.
    fn run_file(&self, name: &str) -> Output {
        let path = self.dir.join(name);
        self.sharky(&[path.to_str().expect("utf-8 sandbox path")])
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

// `return` in a sourced file ends that file and reports its code to the
// sourcing context, which keeps running.

#[test]
fn return_in_a_sourced_file_reports_to_the_sourcing_shell() {
    let sandbox = Sandbox::new(&[("lib.sh", "echo in-lib\nreturn 5\necho never\n")]);
    let out = sandbox.run("source ./lib.sh; echo \"st=$? survived\"");
    assert_eq!(stdout(&out), "in-lib\nst=5 survived\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn dot_in_a_function_returns_only_from_the_sourced_file() {
    let sandbox = Sandbox::new(&[("lib2.sh", "echo inner\nreturn 3\n")]);
    let out = sandbox.run("f() { . ./lib2.sh; echo \"fn st=$?\"; }; f; echo \"outer st=$? survived\"");
    assert_eq!(stdout(&out), "inner\nfn st=3\nouter st=0 survived\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_still_exits_on_a_failing_source() {
    let sandbox = Sandbox::new(&[("lib3.sh", "return 7\n")]);
    let out = sandbox.run("set -e; . ./lib3.sh; echo not-reached");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 7);
}

#[test]
fn return_outside_a_function_or_source_is_an_error_that_continues() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("return 9; echo \"st=$? survived\"");
    assert_eq!(stdout(&out), "st=2 survived\n");
    assert_eq!(status(&out), 0);
    let err = stderr(&out);
    assert!(
        err.contains("return") && err.contains("function or sourced script"),
        "unexpected diagnostic: {err:?}"
    );
}

#[test]
fn return_in_a_script_file_is_an_error_that_continues() {
    let sandbox = Sandbox::new(&[("top.sh", "return 9\necho survived\n")]);
    let out = sandbox.run_file("top.sh");
    assert_eq!(stdout(&out), "survived\n");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("return"), "missing diagnostic: {:?}", stderr(&out));
}

#[test]
fn return_at_top_level_of_a_subshell_reports_status_two() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("(return 5); echo \"st=$? survived\"");
    assert_eq!(stdout(&out), "st=2 survived\n");
    assert_eq!(status(&out), 0);
}

// errexit is off inside `$(...)` unless `inherit_errexit` is enabled, so a
// failing command there does not end the substitution.

#[test]
fn errexit_is_disabled_inside_command_substitution() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("set -e; x=$(false; echo inner); echo \"st=$? x=$x survived\"");
    assert_eq!(stdout(&out), "st=0 x=inner survived\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_is_disabled_in_nested_command_substitution() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("set -e; y=$(x=$(false); echo mid); echo \"st=$? y=$y survived\"");
    assert_eq!(stdout(&out), "st=0 y=mid survived\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn a_failed_substitution_still_fails_the_assignment_under_errexit() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("set -e; x=$(false); echo survived");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn a_guarded_substitution_does_not_trigger_errexit() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("set -e; x=$(false; echo inner) || echo caught");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 0);
}

#[test]
fn errexit_still_applies_to_the_other_contexts() {
    let sandbox = Sandbox::new(&[]);
    let cases: &[(&str, i32)] = &[
        ("set -e; if (false); then echo yes; else echo no; fi; echo survived", 0),
        ("set -e; ! false; echo survived", 0),
        ("set -e; false | true; echo survived", 0),
        ("set -eo pipefail; false | true; echo survived", 1),
        ("set -e; (exit 2); echo survived", 2),
    ];
    for (script, want) in cases {
        let out = sandbox.run(script);
        assert_eq!(status(&out), *want, "wrong status for {script:?}");
        assert!(
            !stdout(&out).contains("survived") || *want == 0,
            "errexit did not stop {script:?}: {:?}",
            stdout(&out)
        );
    }
}
