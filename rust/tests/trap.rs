//! Trap behaviour that only the shell can prove: signal delivery means the
//! shell really runs the stored action, not just that `trap` accepted it.
//!
//! Every expectation here was measured against bash 5.3 first. The cases where
//! sharky deliberately differs say so in a comment.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
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

/// Runs `source` from a script file, which is the path a real cleanup trap
/// takes; `-c` and a script file reach the trap code by different routes.
fn sharky_script(source: &str) -> Output {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "sharky-trap-test-{}-{}.sh",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, source).expect("write script");
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg(&path)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky");
    let _ = std::fs::remove_file(&path);
    out
}

#[test]
fn signal_trap_runs_after_the_command_that_took_it() {
    let out = sharky(&[
        "-c",
        "trap 'echo GOT' USR1; kill -USR1 $$; echo after",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "GOT\nafter\n");
}

#[test]
fn signal_trap_is_deferred_until_the_foreground_command_finishes() {
    // bash runs the action only once `sleep` has returned, so GOT precedes the
    // next command, never interrupts the running one.
    let out = sharky(&[
        "-c",
        "trap 'echo GOT' USR1; (sleep 0.3; kill -USR1 $$) & sleep 0.9; echo ONE; echo TWO; wait",
    ]);
    assert_eq!(out.status.code(), Some(0));
    let text = stdout(&out);
    let got = text.find("GOT").unwrap_or_else(|| panic!("no GOT in {text:?}"));
    let one = text.find("ONE").unwrap_or_else(|| panic!("no ONE in {text:?}"));
    assert!(got < one, "trap ran after the next command in {text:?}");
    assert!(text.contains("TWO"), "missing TWO in {text:?}");
}

#[test]
fn each_signal_runs_its_own_action() {
    let out = sharky(&[
        "-c",
        "trap 'echo U1' USR1; trap 'echo U2' USR2; kill -USR1 $$; kill -USR2 $$; echo after",
    ]);
    assert_eq!(stdout(&out), "U1\nU2\nafter\n");
}

#[test]
fn ignored_signal_runs_nothing() {
    let out = sharky(&["-c", "trap '' USR1; kill -USR1 $$; echo alive"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "alive\n");
}

#[test]
fn ignored_signal_is_inherited_by_a_child_process() {
    // An ignored disposition survives exec, so the child lives too.
    let out = sharky(&[
        "-c",
        "trap '' USR1; sh -c 'kill -USR1 $$; echo child-alive'",
    ]);
    assert_eq!(stdout(&out), "child-alive\n");
}

#[test]
fn caught_signal_is_reset_to_default_for_a_child_process() {
    // A caught disposition is reset by exec; the child dies and the parent
    // reports 128 + signal.
    let out = sharky(&[
        "-c",
        "trap 'echo T' USR1; sh -c 'kill -USR1 $$; echo child-alive' 2>/dev/null; echo st=$?",
    ]);
    assert_eq!(stdout(&out), "st=138\n");
}

#[test]
fn a_non_interactive_int_trap_continues_the_script() {
    // bash does not abandon a non-interactive script whose INT is trapped: the
    // action runs, `$?` is the status of the command that took the signal (the
    // successful `kill`), and the next command runs.
    let out = sharky(&["-c", "trap 'echo T st=$?' INT; kill -INT $$; echo after"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "T st=0\nafter\n");
}

#[test]
fn trap_p_lists_only_installed_names_and_rejects_unknown_ones() {
    let out = sharky(&["-c", "trap -p -- INT; echo st=$?"]);
    assert_eq!(stdout(&out), "st=0\n");
    let out = sharky(&["-c", "trap 'echo hi' INT; trap -p -- INT"]);
    assert_eq!(stdout(&out), "trap -- 'echo hi' SIGINT\n");
    let out = sharky(&["-c", "trap -p BOGUS; echo st=$?"]);
    assert_eq!(stdout(&out), "st=1\n");
}

#[test]
fn trap_dash_alone_is_a_usage_error() {
    let out = sharky(&["-c", "trap -; echo st=$?"]);
    assert_eq!(stdout(&out), "st=2\n");
    assert!(stderr(&out).contains("usage"), "unexpected stderr {:?}", stderr(&out));
}

#[test]
fn trap_action_can_change_its_own_trap() {
    let out = sharky(&[
        "-c",
        "trap 'trap \"echo T2\" USR1; echo T1' USR1; kill -USR1 $$; kill -USR1 $$; echo after",
    ]);
    assert_eq!(stdout(&out), "T1\nT2\nafter\n");
}

#[test]
fn resetting_a_trap_restores_the_default_action() {
    let out = sharky(&[
        "-c",
        "trap 'echo T' USR1; trap - USR1; kill -USR1 $$; echo unreachable",
    ]);
    assert_eq!(out.status.signal(), Some(10));
    assert_eq!(stdout(&out), "");
}

#[test]
fn trap_lists_installed_actions_in_bash_order() {
    // The EXIT action also runs when the shell leaves, after the listing.
    let out = sharky(&["-c", "trap 'echo hi' INT EXIT; trap"]);
    assert_eq!(
        stdout(&out),
        "trap -- 'echo hi' EXIT\ntrap -- 'echo hi' SIGINT\nhi\n"
    );
}

#[test]
fn trap_p_reports_only_the_named_signals() {
    let out = sharky(&["-c", "trap 'echo hi' INT; trap -p INT TERM; echo st=$?"]);
    assert_eq!(stdout(&out), "trap -- 'echo hi' SIGINT\nst=0\n");
}

#[test]
fn trap_reports_an_ignored_signal() {
    let out = sharky(&["-c", "trap '' INT; trap"]);
    assert_eq!(stdout(&out), "trap -- '' SIGINT\n");
}

#[test]
fn trap_accepts_numbers_and_any_spelling() {
    // 2 is INT, `sigusr1` is USR1, and both print in canonical form.
    let out = sharky(&["-c", "trap 'echo a' 2; trap 'echo b' sigusr1; trap"]);
    assert_eq!(
        stdout(&out),
        "trap -- 'echo a' SIGINT\ntrap -- 'echo b' SIGUSR1\n"
    );
}

#[test]
fn invalid_signal_is_reported_and_the_script_continues() {
    let out = sharky(&["-c", "trap 'echo x' FOO; echo st=$?"]);
    assert_eq!(stdout(&out), "st=1\n");
    let text = stderr(&out);
    assert!(
        text.contains("invalid signal specification"),
        "unexpected stderr {text:?}"
    );
    assert!(text.contains("FOO"), "signal name missing from {text:?}");
}

#[test]
fn debug_trap_is_rejected_rather_than_accepted_silently() {
    // bash implements DEBUG; sharky does not, so accepting it would promise
    // tracing that never runs.
    let out = sharky(&["-c", "trap 'echo d' DEBUG; echo st=$?"]);
    assert_eq!(stdout(&out), "st=1\n");
    let text = stderr(&out);
    assert_eq!(
        text.matches("invalid signal specification").count(),
        1,
        "unexpected stderr {text:?}"
    );
}

#[test]
fn trap_without_a_signal_is_a_usage_error() {
    let out = sharky(&["-c", "trap 'echo x'; echo st=$?"]);
    assert_eq!(stdout(&out), "st=2\n");
    assert!(
        stderr(&out).contains("usage"),
        "unexpected stderr {:?}",
        stderr(&out)
    );
}

#[test]
fn exit_trap_sees_the_exit_status() {
    let out = sharky(&["-c", "trap 'echo st=$?' EXIT; false"]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(stdout(&out), "st=1\n");
}

#[test]
fn exit_trap_runs_for_exit_n() {
    let out = sharky(&["-c", "trap 'echo bye' EXIT; exit 3"]);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(stdout(&out), "bye\n");
}

#[test]
fn exit_trap_can_change_the_status() {
    let out = sharky(&["-c", "trap 'echo bye; exit 5' EXIT; exit 3"]);
    assert_eq!(out.status.code(), Some(5));
    assert_eq!(stdout(&out), "bye\n");
}

#[test]
fn exit_trap_failure_does_not_change_the_status() {
    let out = sharky(&["-c", "trap 'false' EXIT; exit 3"]);
    assert_eq!(out.status.code(), Some(3));
}

#[test]
fn exit_trap_runs_once_on_a_normal_exit() {
    let out = sharky(&["-c", "trap 'echo bye' EXIT; echo hi"]);
    assert_eq!(stdout(&out), "hi\nbye\n");
}

#[test]
fn subshell_does_not_run_the_parents_exit_trap() {
    let out = sharky(&["-c", "trap 'echo P' EXIT; (echo insub); echo after"]);
    assert_eq!(stdout(&out), "insub\nafter\nP\n");
}

#[test]
fn subshell_lists_the_inherited_trap() {
    // bash prints the parent's action from `( trap )`, even though the signal
    // itself is back at its default action in the child.
    let out = sharky(&["-c", "trap 'echo T' USR1; (trap)"]);
    assert_eq!(stdout(&out), "trap -- 'echo T' SIGUSR1\n");
}

#[test]
fn subshell_resets_a_caught_signal_to_default() {
    // `sh` reports the subshell's pid, so the signal reaches the child: it dies
    // instead of running the parent's action, and the parent continues.
    let out = sharky(&[
        "-c",
        "trap 'echo T' USR1; (sh -c 'kill -USR1 $PPID'; echo insub) 2>/dev/null; echo after",
    ]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "after\n");
}

#[test]
fn function_defined_traps_apply_to_the_shell() {
    let out = sharky(&[
        "-c",
        "f() { trap 'echo T' USR1; }; f; kill -USR1 $$; echo after",
    ]);
    assert_eq!(stdout(&out), "T\nafter\n");
}

#[test]
fn trap_action_can_exit_the_shell() {
    let out = sharky(&[
        "-c",
        "trap 'echo T; exit 7' USR1; kill -USR1 $$; echo after",
    ]);
    assert_eq!(out.status.code(), Some(7));
    assert_eq!(stdout(&out), "T\n");
}

#[test]
fn trap_action_status_does_not_change_the_script_status() {
    let out = sharky(&["-c", "trap 'false' USR1; true; kill -USR1 $$; echo st=$?"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "st=0\n");
}

#[test]
fn trap_runs_from_a_script_file() {
    let out = sharky_script("trap 'echo GOT' USR1\nkill -USR1 $$\necho after\n");
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "GOT\nafter\n");
}

// ---------------------------------------------------------------------------
// Interactive cases: a pty, because the line editor only runs on a terminal.

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

#[test]
fn interactive_signal_trap_runs_and_the_shell_survives() {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let seen = read_until(fd, "❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    // The action and the markers are written so that the text typed into the
    // terminal never contains the text the shell prints when it runs them.
    let _ = writer.write_all(b"trap 'echo G-$(printf OT)' USR1\r");
    let _ = read_until(fd, "USR1", Duration::from_secs(5));
    let _ = writer.write_all(b"kill -USR1 $$\r");
    let seen = read_until(fd, "G-OT", Duration::from_secs(5));
    assert!(seen.contains("G-OT"), "trap did not run: {seen:?}");

    let _ = writer.write_all(b"printf 'ALI-%s\\n' VE\r");
    let seen = read_until(fd, "ALI-VE", Duration::from_secs(5));
    assert!(seen.contains("ALI-VE"), "shell died after the trap: {seen:?}");

    let _ = writer.write_all(b"exit\r");
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn interactive_ctrl_c_runs_the_int_trap_and_keeps_the_shell_alive() {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let seen = read_until(fd, "❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    let _ = writer.write_all(b"trap 'echo INT-$(printf TRAP)' INT\r");
    // Sync on the post-execution prompt: the trap builtin has run (and the INT
    // action is installed) by the time the shell prints a fresh prompt. The
    // echo of "printf TRAP" only proves the line was typed, not executed, and
    // a Ctrl-C sent at that point can beat trap installation under load.
    let seen = read_until(fd, "❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt after the trap command: {seen:?}");
    let _ = writer.write_all(b"\x03");
    let seen = read_until(fd, "INT-TRAP", Duration::from_secs(5));
    assert!(seen.contains("INT-TRAP"), "INT trap did not run: {seen:?}");

    let _ = writer.write_all(b"printf 'ALI-%s\\n' VE\r");
    let seen = read_until(fd, "ALI-VE", Duration::from_secs(5));
    assert!(seen.contains("ALI-VE"), "shell died on Ctrl-C: {seen:?}");

    let _ = writer.write_all(b"exit\r");
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn interactive_ctrl_c_still_cancels_the_line_without_a_trap() {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let seen = read_until(fd, "❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    let _ = writer.write_all(b"printf 'CANCEL-%s\\n' ME");
    let _ = read_until(fd, "CANCEL-%s", Duration::from_secs(5));
    let _ = writer.write_all(b"\x03");
    let _ = read_until(fd, "❯", Duration::from_secs(5));

    let _ = writer.write_all(b"printf 'ALI-%s\\n' VE\r");
    let seen = read_until(fd, "ALI-VE", Duration::from_secs(5));
    assert!(seen.contains("ALI-VE"), "shell died on Ctrl-C: {seen:?}");
    assert!(
        !seen.contains("CANCEL-ME"),
        "the cancelled line ran: {seen:?}"
    );

    let _ = writer.write_all(b"exit\r");
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit status {status:?}");
}
