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

const MARK_PROMPT: &str = "\x1b]133;A\x1b\\";
const MARK_INPUT: &str = "\x1b]133;B\x1b\\";

#[test]
fn koral_marks_wrap_commands_and_report_the_status() {
    let dir = std::env::temp_dir().join(format!("sharky-marks-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
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

    let start = read_until(fd, MARK_INPUT, Duration::from_secs(5));
    assert!(start.contains(MARK_PROMPT), "no prompt start: {start:?}");

    let _ = writer.write_all(b"printf hi\r");
    let out = read_until(fd, MARK_INPUT, Duration::from_secs(5));
    assert!(out.contains("\x1b]133;C;cmdline_url=printf%20hi\x1b\\"), "no C: {out:?}");
    assert!(out.contains("\x1b]133;D;0\x1b\\"), "no D;0: {out:?}");
    assert!(out.contains(MARK_PROMPT), "no next prompt: {out:?}");

    let _ = writer.write_all(b"\r");
    let empty = read_until(fd, MARK_INPUT, Duration::from_secs(5));
    assert!(!empty.contains("\x1b]133;C;"), "empty line emitted C: {empty:?}");
    assert!(!empty.contains("\x1b]133;D;"), "empty line emitted D: {empty:?}");

    let _ = writer.write_all(b"false\r");
    let failed = read_until(fd, MARK_INPUT, Duration::from_secs(5));
    assert!(failed.contains("\x1b]133;D;1\x1b\\"), "no D;1: {failed:?}");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn koral_marks_keep_multi_line_commands_together() {
    let dir = std::env::temp_dir().join(format!("sharky-marks-multi-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
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

    read_until(fd, MARK_INPUT, Duration::from_secs(5));
    let _ = writer.write_all(b"if true; then\r");
    read_until(fd, "... ", Duration::from_secs(5));
    let _ = writer.write_all(b"echo hi\r");
    read_until(fd, "... ", Duration::from_secs(5));
    let _ = writer.write_all(b"fi\r");
    let out = read_until(fd, MARK_INPUT, Duration::from_secs(5));

    assert_eq!(out.matches("\x1b]133;C;").count(), 1, "expected one C: {out:?}");
    assert!(
        out.contains("cmdline_url=if%20true%3B%20then%0Aecho%20hi%0Afi"),
        "wrong cmdline: {out:?}"
    );
    assert!(out.contains("\x1b]133;D;0\x1b\\"), "no D;0: {out:?}");

    let _ = writer.write_all(b"echo >\r");
    let broken = read_until(fd, MARK_INPUT, Duration::from_secs(5));
    assert!(broken.contains("\x1b]133;D;2\x1b\\"), "no D;2: {broken:?}");
    assert!(!broken.contains("\x1b]133;C;"), "parse error emitted C: {broken:?}");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn koral_osc7_encodes_the_working_directory() {
    let dir = std::env::temp_dir().join(format!("sharky osc é-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .current_dir(&dir)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("TERM", "xterm-256color")
        .env("HOME", &dir)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let seen = read_until(fd, MARK_INPUT, Duration::from_secs(5));

    let mut writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    assert!(seen.contains("\x1b]7;file://"), "no OSC 7: {seen:?}");
    assert!(seen.contains("%20"), "space not encoded: {seen:?}");
    assert!(seen.contains("%C3%A9"), "non-ASCII not encoded: {seen:?}");
}

struct HistorySession {
    child: std::process::Child,
    master: std::os::fd::OwnedFd,
    writer: std::fs::File,
    data: std::path::PathBuf,
}

fn history_session() -> HistorySession {
    let data = std::env::temp_dir().join(format!(
        "sharky-histexpand-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&data).unwrap();

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
        .env("XDG_DATA_HOME", &data)
        .spawn()
        .expect("spawn sharky");

    let writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    read_until(
        pty.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    HistorySession {
        child,
        master: pty.master,
        writer,
        data,
    }
}

const HISTORY_PROMPT_END: &str = "\u{1b}]133;B\u{1b}\\";

impl HistorySession {
    fn run(&mut self, line: &str) -> String {
        let fd = self.master.as_raw_fd();
        let _ = self.writer.write_all(format!("{line}\r").as_bytes());
        read_until(fd, HISTORY_PROMPT_END, Duration::from_secs(5))
    }

    fn finish(mut self) {
        let _ = self.writer.write_all(b"exit\r");
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.data).ok();
    }
}

#[test]
fn history_expansion_covers_events_and_arguments() {
    let mut session = history_session();
    session.run("history -c");
    session.run("printf 'BASE'");
    let out = session.run("!!");
    assert!(out.contains("printf 'BASE'"), "no echoed expansion in {out:?}");
    assert!(out.contains("BASE"), "past command did not run: {out:?}");

    let out = session.run("!2");
    assert!(out.contains("printf 'BASE'"), "!n failed: {out:?}");

    session.run("printf 'A'");
    session.run("printf 'B'");
    let out = session.run("echo !-2");
    assert!(out.contains("printf A"), "!-n failed: {out:?}");

    session.run("printf '%s' lastarg");
    let out = session.run("echo !$");
    assert!(out.contains("lastarg"), "!$ failed: {out:?}");

    session.run("echo a b");
    let out = session.run("printf '<%s>' !*");
    assert!(out.contains("<a><b>"), "!* failed: {out:?}");

    session.run("echo first second");
    let out = session.run("printf '<%s>' !^");
    assert!(out.contains("<first>"), "!^ failed: {out:?}");

    session.run("echo PREFIXMARK");
    let out = session.run("!echo P");
    assert!(out.contains("echo PREFIXMARK"), "!prefix failed: {out:?}");
    assert!(out.contains("PREFIXMARK"), "!prefix did not run: {out:?}");

    session.run("printf 'SUBB'");
    let out = session.run("echo !?SUBB?");
    assert!(out.contains("printf SUBB"), "!?sub? failed: {out:?}");

    session.run("echo old");
    let out = session.run("^old^new^");
    assert!(out.contains("echo new"), "quick substitution failed: {out:?}");
    assert!(out.contains("new"), "quick substitution did not run: {out:?}");

    session.finish();
}

#[test]
fn history_expansion_respects_quotes_and_reports_missing_events() {
    let mut session = history_session();
    session.run("echo quoted");

    let out = session.run("echo '!!'");
    assert!(!out.contains("echo quoted"), "single quotes expanded: {out:?}");
    assert!(out.contains("!!"), "single-quoted text missing: {out:?}");

    let out = session.run("echo \\!\\!");
    assert!(!out.contains("echo quoted"), "backslash expanded: {out:?}");
    assert!(out.contains("!!"), "escaped text missing: {out:?}");

    session.run("echo dqtarget");
    let out = session.run("echo \"!!\"");
    assert!(out.contains("echo dqtarget"), "double quotes must expand: {out:?}");

    let out = session.run("!doesnotexist");
    assert!(out.contains("event not found"), "missing error: {out:?}");
    let out = session.run("echo $?");
    assert!(out.contains('1'), "missing event status: {out:?}");

    session.run("set +H");
    let out = session.run("echo !!");
    assert!(!out.contains("echo quoted"), "set +H did not disable: {out:?}");

    session.run("set -H");
    let out = session.run("echo reenabled");
    assert!(out.contains("reenabled"), "echo reenabled output: {out:?}");
    let out = session.run("!!");
    assert!(out.contains("echo reenabled"), "set -H did not re-enable: {out:?}");

    session.finish();
}

#[test]
fn interactive_exec_failure_keeps_the_shell_alive() {
    let mut session = history_session();
    let out = session.run("exec /nonexistent/sharky");
    assert!(out.contains("exec"), "no exec error in {out:?}");
    let out = session.run("echo after");
    assert!(out.contains("after"), "shell died after failed exec: {out:?}");
    let out = session.run("echo $?");
    assert!(out.contains('1'), "unexpected failure status: {out:?}");
    session.finish();
}
