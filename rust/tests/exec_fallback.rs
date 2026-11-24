//! PATH lookup and `execve` failure parity: effective execute permission and
//! the ENOEXEC shell-script fallback.
//!
//! Every expectation was measured against bash 5.3.20 first. Bash tests PATH
//! candidates with the effective IDs, remembers an existing regular file it may
//! not execute for a 126 report, and runs an ENOEXEC file as a shell script
//! with its own interpreter, forwarding the arguments and the exit status.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-exec-fallback";
const SYSTEM_PATH: &str = "/run/current-system/sw/bin:/usr/bin:/bin";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-exec-fallback-{}-{name}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes an executable-looking file with an explicit mode.
fn write_mode(path: &Path, mode: u32, body: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Runs sharky with `path` as `PATH` and `dir` as the working directory.
fn sharky(home: &Path, dir: &Path, path: &str, script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", script])
        .current_dir(dir)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("HOME", home)
        .env("PATH", path)
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

// bash: `probe` with mode 0605 is a regular file with an execute bit, but the
// owning user's class has none, so the search skips it and finds the real
// command later in PATH. The blocked copy must never run.
#[test]
fn path_search_skips_a_candidate_without_effective_execute_permission() {
    if unsafe { nix::libc::geteuid() } == 0 {
        return;
    }
    let home = scratch("xok");
    let blocked = home.join("blocked");
    let real = home.join("real");
    write_mode(&blocked.join("probe"), 0o605, b"echo BLOCKED\n");
    write_mode(&real.join("probe"), 0o755, b"echo REAL\n");

    let path = format!(
        "{}:{}:{SYSTEM_PATH}",
        blocked.display(),
        real.display()
    );
    let out = sharky(&home, &home, &path, "probe");
    assert_eq!(stdout(&out), "REAL\n", "{out:?}");
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));

    std::fs::remove_dir_all(&home).ok();
}

// bash: an existing regular file that cannot be executed for this user is
// remembered, so a search that finds nothing runnable reports 126
// (Permission denied), not 127.
#[test]
fn a_lone_candidate_without_execute_permission_reports_126() {
    if unsafe { nix::libc::geteuid() } == 0 {
        return;
    }
    let home = scratch("denied");
    write_mode(&home.join("only/probe"), 0o644, b"echo BAD\n");

    let path = format!("{}/only:{SYSTEM_PATH}", home.display());
    let out = sharky(&home, &home, &path, "probe; echo \"s=$?\"");
    assert_eq!(stdout(&out), "s=126\n", "{out:?}");
    assert!(
        !stdout(&out).contains("BAD"),
        "a non-executable candidate must not run: {out:?}"
    );

    std::fs::remove_dir_all(&home).ok();
}

// bash: a candidate with a stray execute bit for another class is skipped the
// same way, and reports 126 when no runnable candidate exists.
#[test]
fn a_lone_candidate_with_a_stray_execute_bit_reports_126() {
    if unsafe { nix::libc::geteuid() } == 0 {
        return;
    }
    let home = scratch("stray");
    write_mode(&home.join("only/probe"), 0o605, b"echo BAD\n");

    let path = format!("{}/only:{SYSTEM_PATH}", home.display());
    let out = sharky(&home, &home, &path, "probe; echo \"s=$?\"");
    assert_eq!(stdout(&out), "s=126\n", "{out:?}");
    assert!(!stdout(&out).contains("BAD"), "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

// bash: directories and dangling symlinks in PATH are ignored completely.
#[test]
fn directory_and_dangling_path_candidates_are_not_found() {
    let home = scratch("dirs");
    std::fs::create_dir_all(home.join("d1/probe")).unwrap();
    std::fs::create_dir_all(home.join("d2")).unwrap();
    std::os::unix::fs::symlink("/nonexistent/sharky-probe", home.join("d2/probe")).unwrap();

    let path = format!("{}/d1:{}/d2:{SYSTEM_PATH}", home.display(), home.display());
    let out = sharky(&home, &home, &path, "probe; echo \"s=$?\"");
    assert_eq!(stdout(&out), "s=127\n", "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

// bash runs a file the kernel rejects with ENOEXEC through its own interpreter:
// `$0` is the path that was resolved, the arguments are forwarded, and the
// script's exit status is the command's status.
#[test]
fn an_enoexec_file_runs_as_a_shell_script() {
    let home = scratch("enoexec");
    let bin = home.join("bin");
    write_mode(
        &bin.join("plain"),
        0o755,
        b"printf 'zero=%s\\n' \"$0\"\nprintf 'args=[%s]\\n' \"$*\"\necho ran\nexit 7\n",
    );

    let path = format!("{}:{SYSTEM_PATH}", bin.display());
    let out = sharky(&home, &home, &path, "plain a 'b c'; echo \"s=$?\"");
    assert_eq!(
        stdout(&out),
        format!(
            "zero={}\nargs=[a b c]\nran\ns=7\n",
            bin.join("plain").display()
        ),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(status(&out), 0);
    assert_eq!(stderr(&out), "", "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

// bash also falls back for a file named by a path, and `$0` keeps the spelling
// that was invoked.
#[test]
fn an_enoexec_file_run_by_path_also_falls_back() {
    let home = scratch("enoexec-path");
    write_mode(
        &home.join("plain"),
        0o755,
        b"printf 'zero=%s args=[%s]\\n' \"$0\" \"$*\"\nexit 3\n",
    );

    let out = sharky(&home, &home, SYSTEM_PATH, "./plain x; echo \"s=$?\"");
    assert_eq!(stdout(&out), "zero=./plain args=[x]\ns=3\n", "{out:?}");

    std::fs::remove_dir_all(&home).ok();
}

// bash does not treat every ENOEXEC file as a script: a NUL byte or the ELF
// magic marks a binary, so it reports 126 and runs nothing.
#[test]
fn a_binary_file_with_a_bad_format_reports_126_without_running_it() {
    let home = scratch("binary");
    let bin = home.join("bin");
    write_mode(
        &bin.join("junk"),
        0o755,
        b"\x7fELF\x01\x02\x00\x00echo BAD\n",
    );
    // The magic alone is enough: bash rejects it even without a NUL byte.
    write_mode(&bin.join("elfjunk"), 0o755, b"\x7fELFjunk\necho BAD\n");

    let path = format!("{}:{SYSTEM_PATH}", bin.display());
    for name in ["junk", "elfjunk"] {
        let out = sharky(&home, &home, &path, &format!("{name}; echo \"s=$?\""));
        assert_eq!(stdout(&out), "s=126\n", "{name}: {out:?}");
        assert!(!stdout(&out).contains("BAD"), "{name}: {out:?}");
    }

    std::fs::remove_dir_all(&home).ok();
}

// The `exec` builtin replaces the shell, so the fallback script runs with the
// caller's environment and its exit status becomes the shell's.
#[test]
fn exec_builtin_falls_back_to_a_shell_script_for_enoexec() {
    let home = scratch("exec-builtin");
    let script = home.join("plain");
    write_mode(
        &script,
        0o755,
        b"printf 'zero=%s args=[%s]\\n' \"$0\" \"$*\"\nexit 4\n",
    );

    let invocation = format!("exec '{}' a b; echo unreached", script.display());
    let out = sharky(&home, &home, SYSTEM_PATH, &invocation);
    assert_eq!(
        stdout(&out),
        format!("zero={} args=[a b]\n", script.display()),
        "stderr: {}",
        stderr(&out)
    );
    assert_eq!(status(&out), 4);

    std::fs::remove_dir_all(&home).ok();
}
