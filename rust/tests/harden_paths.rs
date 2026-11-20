//! Targeted regression tests for the temporary-path and process-substitution
//! hardening: unpredictable heredoc/FIFO names, refusal to follow a symlink at
//! a FIFO path, and a bounded readiness wait that cannot hang the shell.

mod common;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

/// A fresh, isolated scratch directory under the approved temp area.
fn isolated_dir(tag: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let dir = PathBuf::from("/tmp/opencode").join(format!(
        "sharky-harden-{}-{tag}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Spawn sharky with no config and an isolated HOME/XDG/history/TMPDIR.
fn spawn_sharky(tmp: &Path, fifo: bool, args: &[&str]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharky"));
    cmd.arg("--no-config")
        .args(args)
        .env("HOME", tmp)
        .env("XDG_CONFIG_HOME", tmp.join("xdg"))
        .env("SHARKY_HISTORY", tmp.join("history"))
        .env("SHARKY_CONFIG", "/dev/null")
        .env("PATH", common::tool_path())
        .env("TMPDIR", tmp)
        .env("LC_ALL", "C.UTF-8");
    if fifo {
        cmd.env("SHARKY_PROCESS_SUBST_FIFO", "1");
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn sharky")
}

fn output_text(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn error_text(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Each child of `pid`, read from /proc.
fn children_of(pid: i32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect()
}

fn comm_of(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// A here-document must not fall back to the old, predictable counter names:
/// pre-creating every one of them (as symlinks to a victim) used to exhaust all
/// retries and break the here-document. Random suffixes avoid the traps.
#[test]
fn heredoc_avoids_precreated_counter_names() {
    let dir = isolated_dir("heredoc");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let victim = dir.join("victim");
    std::fs::write(&victim, "original\n").unwrap();

    let mut child = spawn_sharky(&tmp, false, &[]);
    let pid = child.id() as i32;

    // The old scheme tried sharky-here-{pid}-{0..15}. Occupy them all.
    for counter in 0..16 {
        let trap = tmp.join(format!("sharky-here-{pid}-{counter}"));
        std::os::unix::fs::symlink(&victim, &trap).unwrap();
    }

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"cat <<EOF\npayload\nEOF\n")
        .unwrap();
    drop(child.stdin.take());

    let out = child.wait_with_output().unwrap();
    assert_eq!(
        output_text(&out),
        "payload\n",
        "heredoc failed with pre-created counter names: {}",
        error_text(&out)
    );
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "original\n",
        "a pre-created symlink redirected the here-document write"
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// In FIFO mode every process substitution gets a distinct, unpredictable
/// suffix. The old code reused `-0` for every substitution in a process.
#[test]
fn psub_fifo_names_are_unpredictable() {
    let dir = isolated_dir("psub-names");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    let child = spawn_sharky(&tmp, true, &["-c", "echo <(true) <(true) <(true) <(true)"]);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sharky failed: {}", error_text(&out));

    let stdout = output_text(&out);
    let paths: Vec<&str> = stdout.split_whitespace().collect();
    assert_eq!(paths.len(), 4, "expected four FIFO paths: {:?}", paths);

    let prefix = format!("{}/sharky-psub-", tmp.display());
    let mut suffixes = HashSet::new();
    for path in paths {
        assert!(path.starts_with(&prefix), "unexpected FIFO path {path:?}");
        let suffix = path.rsplit('-').next().unwrap();
        assert!(
            suffix.len() >= 16 && suffix.bytes().all(|b| b.is_ascii_hexdigit()),
            "suffix {suffix:?} is not a random hex token"
        );
        assert_ne!(suffix, "0", "fell back to the old counter name");
        assert!(suffixes.insert(suffix), "duplicate FIFO suffix {suffix:?}");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Random names and O_NOFOLLOW must not disturb the normal FIFO path: the
/// child still opens the FIFO and its output is delivered.
#[test]
fn psub_fifo_delivers_output_normally() {
    let dir = isolated_dir("psub-normal");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    let child = spawn_sharky(&tmp, true, &["-c", "cat <(echo fifo-psub-ok)"]);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "sharky failed: {}", error_text(&out));
    assert_eq!(output_text(&out), "fifo-psub-ok\n");
    std::fs::remove_dir_all(&dir).ok();
}

/// A process-substitution child stopped before it signals readiness must not
/// freeze the shell. The wrapped sleep ignores the FIFO path argument, so the
/// child stays in open_fifo_end for the whole sleep; stopping it there used to
/// leave the parent blocked forever in the bare readiness `read` (wchan
/// `anon_pipe_read`). The bounded poll wait lets the shell finish instead.
#[test]
fn stopped_psub_child_does_not_hang_shell() {
    let dir = isolated_dir("psub-stop");
    let tmp = dir.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();

    let mut child = spawn_sharky(
        &tmp,
        true,
        &["-c", "sh -c 'sleep 2' <(echo hi); echo AFTER"],
    );
    let pid = child.id() as i32;

    // Wait for the forked psub child (it never execs, so comm is still
    // "sharky") and stop it before it can write the readiness byte.
    let search_deadline = Instant::now() + Duration::from_secs(1);
    let mut stopped = None;
    while Instant::now() < search_deadline {
        for kid in children_of(pid) {
            if comm_of(kid) == "sharky" {
                kill(Pid::from_raw(kid), Signal::SIGSTOP).ok();
                stopped = Some(kid);
                break;
            }
        }
        if stopped.is_some() {
            break;
        }
        sleep(Duration::from_millis(10));
    }
    assert!(
        stopped.is_some(),
        "never found a psub child to stop; test cannot exercise the hang"
    );

    // Poll the shell process itself: the stopped child keeps the stdout pipe
    // open, so waiting for pipe EOF would be misleading.
    let exit_deadline = Instant::now() + Duration::from_secs(7);
    let mut exited = false;
    while Instant::now() < exit_deadline {
        if child.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        sleep(Duration::from_millis(20));
    }

    // Reap only the child we created and stopped.
    if let Some(kid) = stopped {
        kill(Pid::from_raw(kid), Signal::SIGKILL).ok();
    }
    if !exited {
        child.kill().ok();
    }
    let status = child.wait().unwrap();

    assert!(exited, "shell hung indefinitely on a stopped psub child");
    assert!(status.success(), "shell exited with {status}");
    std::fs::remove_dir_all(&dir).ok();
}
