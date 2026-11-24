//! Stability tests for recursion guards, IGNOREEOF, stopped-jobs exit protection,
//! and huponexit.

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

const NO_CONFIG: &str = "/nonexistent/sharky-stability-test";

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-stability-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
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

fn spawn_pty(args: &[&str], home: &std::path::Path) -> (Child, std::os::fd::OwnedFd) {
    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", home)
        .spawn()
        .expect("spawn sharky");

    (child, pty.master)
}

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("sharky did not exit cleanly");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn circular_source_fails_without_crashing() {
    let dir = scratch("source-cycle");
    let a = dir.join("a.sh");
    let b = dir.join("b.sh");
    std::fs::write(&a, format!(". '{}'\n", b.display())).unwrap();
    std::fs::write(&b, format!(". '{}'\n", a.display())).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", &format!(". '{}'", a.display())])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    assert_ne!(out.status.code(), Some(0));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("maximum source depth"), "{err}");
}

#[test]
fn funcnest_stops_infinite_recursion_cleanly() {
    let custom = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "FUNCNEST=5; f() { f; }; f"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .unwrap();
    assert_ne!(custom.status.code(), Some(0));
    let err = String::from_utf8_lossy(&custom.stderr);
    assert!(err.contains("maximum function nesting level exceeded (5)"), "{err}");
}

#[test]
fn shopt_huponexit_can_be_toggled_and_queried() {
    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "shopt -q huponexit && exit 9; shopt -s huponexit; shopt -q huponexit && printf ON"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ON");
}

#[test]
fn ignoreeof_blocks_first_two_ctrl_d_and_exits_on_third() {
    let home = scratch("ignoreeof");
    let (mut child, master) = spawn_pty(&[], &home);
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let _ = read_until(fd, "❯", Duration::from_secs(5));
    writer.write_all(b"set -o ignoreeof; IGNOREEOF=2\r").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    writer.write_all(b"\x04").unwrap();
    let warn1 = read_until(fd, "Use \"exit\" to leave the shell.", Duration::from_secs(3));
    assert!(warn1.contains("Use \"exit\" to leave the shell."), "{warn1:?}");
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    writer.write_all(b"\x04").unwrap();
    let warn2 = read_until(fd, "Use \"exit\" to leave the shell.", Duration::from_secs(3));
    assert!(warn2.contains("Use \"exit\" to leave the shell."), "{warn2:?}");
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    // 3rd Ctrl-D exits
    writer.write_all(b"\x04").unwrap();
    let status = wait_for_exit(&mut child);
    let _ = std::fs::remove_dir_all(&home);
    assert!(status.success(), "{status:?}");
}

#[test]
fn stopped_job_warns_on_first_exit_and_leaves_on_second() {
    let home = scratch("stopped-job");
    let (mut child, master) = spawn_pty(&[], &home);
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let _ = read_until(fd, "❯", Duration::from_secs(5));
    writer.write_all(b"sleep 30 &\r").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));
    writer.write_all(b"kill -STOP %1\r").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    writer.write_all(b"exit\r").unwrap();
    let warn = read_until(fd, "There are stopped jobs.", Duration::from_secs(3));
    assert!(warn.contains("There are stopped jobs."), "{warn:?}");
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    writer.write_all(b"exit\r").unwrap();
    let status = wait_for_exit(&mut child);
    let _ = std::fs::remove_dir_all(&home);
    assert_eq!(status.code(), Some(1));
}
