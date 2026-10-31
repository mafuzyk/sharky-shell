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

#[test]
fn interactive_shell_persists_history_with_0600() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    let data = std::env::temp_dir().join(format!("sharky-history-{}", std::process::id()));
    std::fs::create_dir_all(&data).unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("XDG_DATA_HOME", &data)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    read_until(fd, "❯", Duration::from_secs(5));
    let _ = writer.write_all(b"printf HISTDONE\r");
    read_until(fd, "HISTDONE", Duration::from_secs(5));
    std::thread::sleep(Duration::from_millis(200));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();

    let path = Path::new(&data).join("sharky/history");
    let text = std::fs::read_to_string(&path).expect("history file");
    assert!(text.contains("printf HISTDONE"), "history={text:?}");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    std::fs::remove_dir_all(&data).ok();
    assert_eq!(mode, 0o600);
}

#[test]
fn starship_can_provide_the_prompt() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("sharky-starship-{}", std::process::id()));
    let config = dir.join("config");
    let bin = dir.join("bin");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(config.join("config.sharky"), "tide starship on\n").unwrap();

    let fake = bin.join("starship");
    std::fs::write(&fake, "#!/bin/sh\nprintf 'STARSHIP-PROMPT> '\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let seen = read_until(fd, "STARSHIP-PROMPT>", Duration::from_secs(5));
    std::fs::remove_dir_all(&dir).ok();
    assert!(seen.contains("STARSHIP-PROMPT>"), "no starship prompt in {seen:?}");

    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
}

#[test]
fn a_multi_line_prompt_is_drawn_once_per_read_line() {
    let dir = std::env::temp_dir().join(format!("sharky-multiline-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"TOP\\nBOTTOM> \"\ntide greeting off\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    let mut seen = read_until(fd, "BOTTOM> ", Duration::from_secs(5));
    for expected in ["a", "ab", "abc"] {
        let last = expected.chars().last().unwrap().to_string();
        let _ = writer.write_all(last.as_bytes());
        seen.push_str(&read_until(fd, expected, Duration::from_secs(5)));
    }

    let _ = writer.write_all(b"\r");
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    let count = seen.matches("TOP").count();
    assert_eq!(count, 1, "prompt redrawn {count} times: {seen:?}");
}

#[test]
fn wake_runs_commands_before_the_first_prompt() {
    let dir = std::env::temp_dir().join(format!("sharky-wake-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"PROMPT> \"\ntide greeting off\nwake \"printf WAKE-UP\"\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let seen = read_until(fd, "PROMPT> ", Duration::from_secs(5));
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    let wake = seen.find("WAKE-UP").expect("wake output");
    let prompt = seen.find("PROMPT>").expect("prompt");
    assert!(wake < prompt, "wake output after prompt: {seen:?}");
    assert_eq!(seen.matches("WAKE-UP").count(), 1);
}

#[test]
fn abbreviations_expand_on_space_and_enter() {
    let dir = std::env::temp_dir().join(format!("sharky-abbr-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\nshoal hi \"printf yes > $HOME/marker\"\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .env("HOME", &dir)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let marker = dir.join("marker");

    read_until(fd, "P> ", Duration::from_secs(5));
    let _ = writer.write_all(b"hi \r");
    read_until(fd, "P> ", Duration::from_secs(5));
    assert_eq!(std::fs::read_to_string(&marker).unwrap_or_default(), "yes");

    std::fs::remove_file(&marker).ok();
    let _ = writer.write_all(b"hi\r");
    read_until(fd, "P> ", Duration::from_secs(5));
    assert_eq!(std::fs::read_to_string(&marker).unwrap_or_default(), "yes");

    std::fs::remove_file(&marker).ok();
    let _ = writer.write_all(b"echo hi \r");
    read_until(fd, "P> ", Duration::from_secs(5));
    assert!(!marker.exists(), "argument expanded as an abbreviation");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn ctrl_r_searches_history_and_accepts_on_enter() {
    let dir = std::env::temp_dir().join(format!("sharky-search-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("history"),
        "printf first > $HOME/one\nprintf second > $HOME/two\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("SHARKY_HISTORY", dir.join("history"))
        .env("TERM", "xterm-256color")
        .env("HOME", &dir)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    read_until(fd, "P> ", Duration::from_secs(5));
    let _ = writer.write_all(b"\x12");
    let _ = writer.write_all(b"second");
    let _ = writer.write_all(b"\r");
    assert!(wait_for(&dir.join("two"), Duration::from_secs(5)), "search did not run the match");

    let second = std::fs::read_to_string(dir.join("two")).unwrap_or_default();
    let first = std::fs::read_to_string(dir.join("one")).unwrap_or_default();
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(second, "second");
    assert_eq!(first, "");
}

#[test]
fn ctrl_r_cycles_to_older_matches_and_cancels_with_ctrl_g() {
    let dir = std::env::temp_dir().join(format!("sharky-search-cycle-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("history"),
        "printf one > $HOME/one\nprintf two > $HOME/two\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("SHARKY_HISTORY", dir.join("history"))
        .env("TERM", "xterm-256color")
        .env("HOME", &dir)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    read_until(fd, "P> ", Duration::from_secs(5));
    let _ = writer.write_all(b"\x12\x12\r");
    assert!(wait_for(&dir.join("one"), Duration::from_secs(5)), "cycle did not run the older match");

    let one = std::fs::read_to_string(dir.join("one")).unwrap_or_default();
    let two = std::fs::read_to_string(dir.join("two")).unwrap_or_default();

    let _ = writer.write_all(b"\x12\x07");
    std::thread::sleep(Duration::from_millis(100));
    let _ = writer.write_all(b"printf canceled > $HOME/canceled\r");
    assert!(wait_for(&dir.join("canceled"), Duration::from_secs(5)), "cancel did not return to editing");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    assert_eq!(one, "one");
    assert_eq!(two, "");
}

fn wait_for(path: &std::path::Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    path.exists()
}
