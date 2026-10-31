use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

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
fn interactive_shell_shows_a_prompt_and_runs_commands() {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let seen = read_until(fd, "❯", Duration::from_secs(5));
    assert!(seen.contains('❯'), "no prompt in {seen:?}");

    let _ = writer.write_all(b"printf hi\r");
    let seen = read_until(fd, "hi", Duration::from_secs(5));
    assert!(seen.contains("hi"), "command output missing in {seen:?}");

    let _ = writer.write_all(b"exit\r");
    let status = child.wait().expect("wait");
    assert!(status.success(), "exit status {status:?}");
}

#[test]
fn interactive_shell_keeps_running_after_a_failed_command() {
    let (mut child, master) = interactive();
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    read_until(fd, "❯", Duration::from_secs(5));
    let _ = writer.write_all(b"nosuchcommand_here\r");
    read_until(fd, "not found", Duration::from_secs(5));

    let _ = writer.write_all(b"printf after\r");
    let seen = read_until(fd, "after", Duration::from_secs(5));
    assert!(seen.contains("after"), "shell died after a failure: {seen:?}");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
}
