use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

/// A scratch directory holding the files a script sources, removed when dropped.
struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(files: &[(&str, &str)]) -> Sandbox {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sharky-readonly-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// A plain assignment to a readonly variable is fatal to the input unit it runs
// in: bash reports it, skips the rest of that line and leaves the shell with
// status 1 when the failed line is the last one.

#[test]
fn assignment_to_readonly_aborts_the_script() {
    let out = sharky("readonly r=1; r=2; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// The value the failed assignment tried to store never lands.

#[test]
fn assignment_failure_keeps_the_previous_value() {
    let out = sharky("readonly r=1; ( r=2 ); printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
}

// Repeating the current value is still a write, so bash refuses it too.

#[test]
fn assignment_of_the_current_value_is_still_refused() {
    let out = sharky("readonly r=1; r=1; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// Bash 5.3.20 abandons only the input unit it is executing, not the rest of the
// script: everything after the failed assignment on its own line goes, and the
// next line runs.

#[test]
fn assignment_failure_skips_the_rest_of_the_line_and_runs_the_next() {
    let out = sharky("readonly q=1\nq=2\nprintf 'AFTER\\n'");
    assert_eq!(stdout(&out), "AFTER\n");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("q: readonly variable"), "{:?}", stderr(&out));
}

#[test]
fn assignment_failure_skips_the_commands_after_it_on_its_line() {
    let out = sharky("readonly q=1; q=2; printf SAME\nprintf 'NEXT\\n'");
    assert_eq!(stdout(&out), "NEXT\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_failure_leaves_status_one_for_the_next_line() {
    let out = sharky("readonly q=1\nq=2\nprintf 'st:%s\\n' \"$?\"");
    assert_eq!(stdout(&out), "st:1\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_failure_on_the_last_line_still_exits_one() {
    let out = sharky("readonly q=1\nq=2");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(stderr(&out).contains("q: readonly variable"), "{:?}", stderr(&out));
}

#[test]
fn assignment_failure_inside_a_group_skips_the_calling_line() {
    let out = sharky("readonly q=1\n{ q=2; printf IN; }; printf OUT\nprintf 'NEXT\\n'");
    assert_eq!(stdout(&out), "NEXT\n");
    assert_eq!(status(&out), 0);
}

// A compound command consumes the newlines inside it, so its failed line
// discards the whole compound and resumes after it.

#[test]
fn assignment_failure_inside_a_compound_command_skips_the_compound() {
    let out = sharky(
        "readonly q=1\nif true\nthen\nq=2\nprintf IN\nfi\nprintf 'OUT\\n'",
    );
    assert_eq!(stdout(&out), "OUT\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_failure_in_a_function_skips_the_calling_line() {
    let out = sharky(
        "readonly q=1\nf() { q=2; printf IN; }\nf; printf OUT\nprintf 'NEXT\\n'",
    );
    assert_eq!(stdout(&out), "NEXT\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_failure_in_a_function_leaves_status_one() {
    let out = sharky("readonly q=1\nf() { q=2; }\nf\nprintf 'f=%s\\n' \"$?\"");
    assert_eq!(stdout(&out), "f=1\n");
    assert_eq!(status(&out), 0);
}

// The aborted unit belongs to the shell process that runs it, so a pipeline
// member only loses its own subshell.

#[test]
fn assignment_failure_in_a_pipeline_leaves_the_parent_running() {
    let out = sharky("readonly q=1; q=2 | :; printf 'AFTER\\n'");
    assert_eq!(stdout(&out), "AFTER\n");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("q: readonly variable"), "{:?}", stderr(&out));
}

// `eval` runs its argument one line at a time, like a script.

#[test]
fn assignment_failure_in_eval_continues_with_the_next_evalled_line() {
    let out = sharky("readonly q=1; eval \"q=2\nprintf 'NEXT\\n'\"; printf 'OUT\\n'");
    assert_eq!(stdout(&out), "NEXT\nOUT\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn assignment_failure_in_eval_skips_the_rest_of_the_evalled_line() {
    let out = sharky("readonly q=1; eval 'q=2; printf SAME'; printf 'OUT\\n'");
    assert_eq!(stdout(&out), "OUT\n");
    assert_eq!(status(&out), 0);
}

// A sourced file resumes at its next line, and the sourcing line still runs.

#[test]
fn assignment_failure_in_a_sourced_file_leaves_the_rest_of_the_file() {
    let sandbox = Sandbox::new(&[(
        "f.sh",
        "readonly q=1\nq=2\nprintf 'IN\\n'\nprintf 'IN2\\n'\n",
    )]);
    let out = sharky(&format!("source {}\nprintf 'OUT\\n'", sandbox.path("f.sh")));
    assert_eq!(stdout(&out), "IN\nIN2\nOUT\n");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("q: readonly variable"), "{:?}", stderr(&out));
}

#[test]
fn assignment_failure_in_a_sourced_file_leaves_the_calling_line_running() {
    let sandbox = Sandbox::new(&[("f.sh", "readonly q=1\nq=2\nprintf 'IN\\n'\n")]);
    let out = sharky(&format!(
        "source {}; printf 'OUT\\n'",
        sandbox.path("f.sh")
    ));
    assert_eq!(stdout(&out), "IN\nOUT\n");
    assert_eq!(status(&out), 0);
}

// `unset` is a builtin error: it reports, returns 1 and the script goes on.

#[test]
fn unset_of_readonly_reports_and_continues() {
    let out = sharky("readonly r=1; unset r; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("unset: r: cannot unset: readonly variable"),
        "{:?}",
        stderr(&out)
    );
}

// `unset -f` names a function, so a readonly variable of the same name stays.

#[test]
fn unset_dash_f_leaves_a_readonly_variable() {
    let out = sharky("readonly r=1; unset -f r; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "0 1");
    assert_eq!(stderr(&out), "");
}

#[test]
fn unset_dash_v_reports_a_readonly_variable() {
    let out = sharky("readonly r=1; unset -v r; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert!(
        stderr(&out).contains("unset: r: cannot unset: readonly variable"),
        "{:?}",
        stderr(&out)
    );
}

// `declare`, `export` and `readonly` report the refused assignment with status
// 1 and keep the shell running.

#[test]
fn declare_assignment_reports_and_continues() {
    let out = sharky("readonly r=1; declare r=2; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

#[test]
fn export_assignment_reports_and_continues() {
    let out = sharky("readonly r=1; export r=2; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// Bash gives the export attribute to a name whose assignment it refused.

#[test]
fn export_assignment_keeps_the_export_attribute() {
    let out = sharky("readonly r=1; export r=2; declare -p r");
    assert_eq!(stdout(&out), "declare -rx r=\"1\"\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn readonly_reassignment_reports_and_continues() {
    let out = sharky("readonly r=1; readonly r=2; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
}

// `export -n` only drops the export attribute, so the variable stays readonly.

#[test]
fn export_dash_n_leaves_the_readonly_attribute() {
    let out = sharky("readonly r=1; export -n r; r=2; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// Arrays carry the attribute too, on the name.

#[test]
fn indexed_array_element_assignment_aborts() {
    let out = sharky("readonly A=(1 2); A[0]=9; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(stderr(&out).contains("A: readonly variable"), "{:?}", stderr(&out));
}

#[test]
fn readonly_array_append_aborts() {
    let out = sharky("readonly A=(1 2); A+=(3); printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn unset_of_a_readonly_array_element_reports_and_continues() {
    let out = sharky("readonly A=(1 2); unset 'A[0]'; printf '%s %s' \"${A[0]}\" \"${A[1]}\"");
    assert_eq!(stdout(&out), "1 2");
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("unset: A: cannot unset: readonly variable"),
        "{:?}",
        stderr(&out)
    );
}

#[test]
fn readonly_array_literal_cannot_be_replaced() {
    let out = sharky("readonly A=(1 2); A=(3 4); printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// The attribute is not function scoped: a readonly set inside a function
// applies to the global variable.

#[test]
fn readonly_set_inside_a_function_is_global() {
    let out = sharky("f() { readonly r=1; }; f; r=2; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// A local that becomes readonly is enforced while the function runs.

#[test]
fn local_readonly_is_enforced_inside_the_function() {
    let out = sharky("f() { local r=1; readonly r; r=2; printf in; }; f; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// The flag belongs to the local binding, so returning drops it with the frame.

#[test]
fn local_readonly_does_not_leak_to_the_outer_scope() {
    let out = sharky("f() { local r=1; readonly r; }; f; r=2; printf '%s' \"$r\"");
    assert_eq!(stdout(&out), "2");
    assert_eq!(stderr(&out), "");
    assert_eq!(status(&out), 0);
}

// A subshell gets a copy of the attribute, so the parent is unaffected.

#[test]
fn readonly_set_in_a_subshell_does_not_leak() {
    let out = sharky("( readonly r=1 ); r=2; printf '%s' \"$r\"");
    assert_eq!(stdout(&out), "2");
    assert_eq!(status(&out), 0);
}

// `local` cannot shadow a readonly global: bash reports it and the function
// keeps seeing the global value.

#[test]
fn local_cannot_shadow_a_readonly_global() {
    let out = sharky("readonly r=1; f() { local r=2; printf in; }; f; printf '%s' \"$r\"");
    assert_eq!(stdout(&out), "in1");
    assert_eq!(status(&out), 0);
    assert!(
        stderr(&out).contains("local: r: readonly variable"),
        "{:?}",
        stderr(&out)
    );
}

// `readonly -p` prints every readonly variable, and the bare form is the same
// request.

#[test]
fn readonly_dash_p_prints_readonly_variables() {
    let out = sharky("readonly r=1; readonly -p");
    assert_eq!(stdout(&out), "declare -r r=\"1\"\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn readonly_dash_p_prints_exported_and_array_attributes() {
    let out = sharky("readonly r=1; export r; readonly A=(1 2); readonly -p");
    assert_eq!(
        stdout(&out),
        "declare -ar A=([0]=\"1\" [1]=\"2\")\ndeclare -rx r=\"1\"\n"
    );
    assert_eq!(status(&out), 0);
}

#[test]
fn bare_readonly_prints_like_dash_p() {
    let out = sharky("readonly r=1; readonly");
    assert_eq!(stdout(&out), "declare -r r=\"1\"\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn readonly_dash_p_with_a_name_prints_nothing() {
    let out = sharky("x=1; readonly -p x");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 0);
}

// `declare -r` is the other spelling of the same attribute.

#[test]
fn declare_dash_r_marks_a_variable_readonly() {
    let out = sharky("declare -r q=5; q=6; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

#[test]
fn declare_dash_p_shows_the_readonly_flag() {
    let out = sharky("declare -rx y=3; declare -p y");
    assert_eq!(stdout(&out), "declare -rx y=\"3\"\n");
    assert_eq!(status(&out), 0);
}

#[test]
fn declare_dash_p_shows_a_readonly_array() {
    let out = sharky("readonly A=(1 2); declare -p A");
    assert_eq!(stdout(&out), "declare -ar A=([0]=\"1\" [1]=\"2\")\n");
    assert_eq!(status(&out), 0);
}

// A bare readonly declaration keeps the name without a value and prints
// without one.

#[test]
fn bare_readonly_declaration_prints_without_a_value() {
    let out = sharky("readonly x; declare -p x");
    assert_eq!(stdout(&out), "declare -r x\n");
    assert_eq!(status(&out), 0);
}

// The parameter assignment operators write, so they fail the same way a plain
// assignment does.

#[test]
fn parameter_assign_operator_aborts_the_script() {
    let out = sharky("readonly r; echo ${r:=9}; printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// Arithmetic assignments are builtin errors: status 1 and the script goes on.

#[test]
fn arithmetic_assignment_reports_status_one() {
    let out =
        sharky("readonly r=1; (( r=2 )); printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
}

#[test]
fn arithmetic_assignment_in_an_expansion_aborts() {
    let out = sharky("readonly r=1; echo $((r=2)); printf after");
    assert_eq!(stdout(&out), "");
    assert_eq!(status(&out), 1);
}

// A for loop cannot bind its variable, so it reports, runs no iteration and
// leaves the status at 1.

#[test]
fn for_loop_variable_reports_and_runs_no_iteration() {
    let out = sharky("readonly r=1; for r in a b; do printf it; done; printf '%s' \"$?\"");
    assert_eq!(stdout(&out), "1");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// `read` reports the refused write with status 1 and keeps reading the script.

#[test]
fn read_into_a_readonly_reports_status_one() {
    let out = sharky("readonly r=1; read r <<< hi; printf '%s %s' \"$?\" \"$r\"");
    assert_eq!(stdout(&out), "1 1");
    assert_eq!(status(&out), 0);
}

// A prefix assignment that fails still runs the command, and the command owns
// the status.

#[test]
fn prefix_assignment_reports_and_runs_the_command() {
    let out = sharky("readonly r=1; r=2 printf hi; printf '%s' \"$?\"");
    assert_eq!(stdout(&out), "hi0");
    assert_eq!(status(&out), 0);
    assert!(stderr(&out).contains("r: readonly variable"), "{:?}", stderr(&out));
}

// An interactive shell reports the refused assignment and keeps reading.

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
fn interactive_shell_survives_a_readonly_assignment() {
    let seen = run_interactive(
        &[
            "readonly r=1",
            "r=2",
            "printf 'AFTER-RO-%s' \"$r\"",
        ],
        "AFTER-RO-1",
    );
    assert!(
        seen.contains("AFTER-RO-1"),
        "shell died on a readonly assignment: {seen:?}"
    );
    assert!(seen.contains("readonly variable"), "{seen:?}");
}

#[test]
fn getopts_does_not_unset_readonly_optarg() {
    let out = sharky("readonly OPTARG=keep; getopts a opt -a; printf 'st=%s opt=%s arg=%s\\n' \"$?\" \"$opt\" \"$OPTARG\"; readonly -p | grep OPTARG");
    assert_eq!(status(&out), 0, "{:?}", stderr(&out));
    let out_text = stdout(&out);
    let lines: Vec<&str> = out_text.lines().collect();
    assert_eq!(lines.first().copied(), Some("st=0 opt=a arg=keep"), "{out:?}");
    assert!(
        lines.iter().skip(1).any(|line| line.contains("OPTARG")),
        "OPTARG vanished from readonly -p: {out:?}"
    );
    assert!(stderr(&out).contains("OPTARG: readonly variable"), "{:?}", stderr(&out));

    let out = sharky("readonly OPTARG=keep; getopts a: opt -a val; printf 'st=%s opt=%s arg=%s\\n' \"$?\" \"$opt\" \"$OPTARG\"");
    assert_eq!(status(&out), 0, "{:?}", stderr(&out));
    assert_eq!(stdout(&out), "st=0 opt=a arg=keep\n");
    assert!(stderr(&out).contains("OPTARG: readonly variable"), "{:?}", stderr(&out));

    let out = sharky("readonly OPTARG=keep; getopts :a opt -b; printf 'st=%s opt=%s arg=%s\\n' \"$?\" \"$opt\" \"$OPTARG\"");
    assert_eq!(status(&out), 0, "{:?}", stderr(&out));
    assert_eq!(stdout(&out), "st=0 opt=? arg=keep\n");
    assert!(stderr(&out).contains("OPTARG: readonly variable"), "{:?}", stderr(&out));
}

