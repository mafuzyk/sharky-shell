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
    history_session_with(None)
}

fn history_session_with(config_text: Option<&str>) -> HistorySession {
    let data = std::env::temp_dir().join(format!(
        "sharky-histexpand-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&data).unwrap();
    let config_dir = data.join("config");
    let config_env = match config_text {
        Some(text) => {
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::write(config_dir.join("config.sharky"), text).unwrap();
            config_dir.as_os_str().to_owned()
        }
        None => std::ffi::OsString::from(NO_CONFIG),
    };

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", config_env)
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
    assert!(
        out.contains("cmdline_url=echo%20%27%21%21%27"),
        "single quotes expanded: {out:?}"
    );
    assert!(out.contains("!!"), "single-quoted text missing: {out:?}");

    let out = session.run("echo \\!\\!");
    assert!(
        out.contains("cmdline_url=echo%20%5C%21%5C%21"),
        "backslash expanded: {out:?}"
    );
    assert!(out.contains("!!"), "escaped text missing: {out:?}");

    session.run("echo dqtarget");
    let out = session.run("echo \"!!\"");
    assert!(
        out.contains("cmdline_url=echo%20%22echo%20dqtarget%22"),
        "double quotes must expand: {out:?}"
    );

    let out = session.run("!doesnotexist");
    assert!(out.contains("event not found"), "missing error: {out:?}");
    let out = session.run("echo $?");
    assert!(out.contains('1'), "missing event status: {out:?}");

    session.run("set +H");
    let out = session.run("echo !!");
    assert!(
        out.contains("cmdline_url=echo%20%21%21"),
        "set +H did not disable: {out:?}"
    );

    session.run("set -H");
    let out = session.run("echo reenabled");
    assert!(out.contains("reenabled"), "echo reenabled output: {out:?}");
    let out = session.run("!!");
    assert!(
        out.contains("cmdline_url=echo%20reenabled"),
        "set -H did not re-enable: {out:?}"
    );

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

fn paste(writer: &mut std::fs::File, text: &str) {
    let _ = writer.write_all(format!("\u{1b}[200~{text}\u{1b}[201~").as_bytes());
}

#[test]
fn bracketed_paste_defers_execution_until_enter() {
    let mut session = history_session();
    paste(&mut session.writer, "printf 'PASTED-A'\nprintf 'PASTED-B'");
    let seen = read_until(session.master.as_raw_fd(), "PASTED-B", Duration::from_secs(5));
    assert!(seen.contains("PASTED-B"), "paste not rendered: {seen:?}");
    assert!(!seen.contains("133;C"), "pasted text ran early: {seen:?}");

    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("PASTED-A"), "first line missing: {out:?}");
    assert!(out.contains("PASTED-B"), "second line missing: {out:?}");
    assert_eq!(out.matches("133;C").count(), 1, "not one command: {out:?}");
    session.finish();
}

#[test]
fn bracketed_paste_inserts_at_the_cursor() {
    let mut session = history_session();
    let _ = session.writer.write_all(b"echo pre");
    read_until(session.master.as_raw_fd(), "echo pre", Duration::from_secs(5));
    paste(&mut session.writer, " mid");
    read_until(session.master.as_raw_fd(), "pre mid", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("pre mid"), "paste at cursor failed: {out:?}");
    session.finish();
}

#[test]
fn bracketed_paste_runs_a_compound_block_once() {
    let mut session = history_session();
    paste(
        &mut session.writer,
        "if true; then\nprintf 'IFDONE'\nfi",
    );
    read_until(session.master.as_raw_fd(), "fi", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("IFDONE"), "block did not run: {out:?}");
    assert!(!out.contains("syntax error"), "parse error: {out:?}");
    assert_eq!(out.matches("133;C").count(), 1, "not one block: {out:?}");
    session.finish();
}

#[test]
fn multiline_buffer_moves_by_line() {
    let mut session = history_session();
    paste(&mut session.writer, "echo one\necho two");
    read_until(session.master.as_raw_fd(), "echo two", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\x1b[A");
    let _ = session.writer.write_all(b"\x1b[F");
    let _ = session.writer.write_all(b"X");
    let _ = session.writer.write_all(b"\x1b[B");
    let _ = session.writer.write_all(b"Y");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("oneX"), "end of first line: {out:?}");
    assert!(out.contains("twoY"), "end of second line: {out:?}");
    session.finish();
}

#[test]
fn multiline_home_moves_to_the_line_start() {
    let mut session = history_session();
    paste(&mut session.writer, "echo one\necho two");
    read_until(session.master.as_raw_fd(), "echo two", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\x1b[A");
    let _ = session.writer.write_all(b"\x1b[H");
    let _ = session.writer.write_all(b" ");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("one"), "first line did not run: {out:?}");
    assert!(out.contains("two"), "second line did not run: {out:?}");
    session.finish();
}

#[test]
fn bracketed_paste_is_disabled_around_prompts() {
    let mut session = history_session();
    let seen = session.run("echo paste-toggle");
    assert!(seen.contains("\u{1b}[?2004h"), "paste never enabled: {seen:?}");
    assert!(seen.contains("\u{1b}[?2004l"), "paste never disabled: {seen:?}");
    session.finish();
}


#[test]
fn autosuggest_shows_a_ghost_and_right_accepts_it() {
    let mut session = history_session();
    session.run("echo suggested");

    let _ = session.writer.write_all(b"echo sug");
    let seen = read_until(
        session.master.as_raw_fd(),
        "gested",
        Duration::from_secs(5),
    );
    assert!(seen.contains("gested"), "no ghost text: {seen:?}");
    assert!(!seen.contains("133;C"), "ghost ran a command: {seen:?}");

    let _ = session.writer.write_all(b"\x1b[C");
    std::thread::sleep(Duration::from_millis(50));
    let _ = session.writer.write_all(b"\x03");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(!out.contains("133;C"), "accepting ran the command: {out:?}");

    let _ = session.writer.write_all(b"echo sug");
    read_until(session.master.as_raw_fd(), "gested", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\x1b[F");
    let out = read_until(
        session.master.as_raw_fd(),
        "echo suggested",
        Duration::from_secs(5),
    );
    assert!(out.contains("echo suggested"), "End did not accept: {out:?}");
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn autosuggest_off_shows_no_ghost() {
    let mut session = history_session_with(Some("tide autosuggest off\n"));
    session.run("echo suggested");
    let _ = session.writer.write_all(b"echo sug");
    let seen = read_until(
        session.master.as_raw_fd(),
        "echo sug",
        Duration::from_secs(5),
    );
    std::thread::sleep(Duration::from_millis(100));
    let extra = read_until(
        session.master.as_raw_fd(),
        "gested",
        Duration::from_millis(200),
    );
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
    assert!(!seen.contains("gested"), "ghost with autosuggest off: {seen:?}");
    assert!(extra.is_empty(), "ghost with autosuggest off: {extra:?}");
}

#[test]
fn autosuggest_is_silent_mid_line() {
    let mut session = history_session();
    session.run("echo suggested");
    let _ = session.writer.write_all(b"echo sug");
    read_until(
        session.master.as_raw_fd(),
        "gested",
        Duration::from_secs(5),
    );
    // Drain the ghost redraws already queued, then move the cursor and look
    // at the fresh redraw only.
    read_until(
        session.master.as_raw_fd(),
        "\u{0}",
        Duration::from_millis(100),
    );
    let _ = session.writer.write_all(b"\x1b[D");
    let extra = read_until(
        session.master.as_raw_fd(),
        "\u{0}",
        Duration::from_millis(300),
    );
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
    assert!(!extra.contains("gested"), "ghost mid-line: {extra:?}");
}

const HIGHLIGHT_CONFIG: &str = "coral sea #35B8C5
coral kelp #8CCFA8
coral danger #FF8F80
wave command @sea
wave builtin @kelp
wave string @kelp
wave variable @danger
wave error @danger
tide greeting off
";

#[test]
fn highlight_paints_command_strings_and_variables() {
    let mut session = history_session_with(Some(HIGHLIGHT_CONFIG));
    let _ = session.writer.write_all(b"echo \"$HOME\"");
    let seen = read_until(
        session.master.as_raw_fd(),
        "$HOME\u{1b}[0m\u{1b}[38;2;140;207;168m\"",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\u{1b}[38;2;53;184;197mecho\u{1b}[0m"),
        "command role missing: {seen:?}"
    );
    assert!(
        seen.contains("\u{1b}[38;2;255;143;128m$HOME\u{1b}[0m"),
        "variable role missing: {seen:?}"
    );
    assert!(
        seen.contains("\u{1b}[38;2;140;207;168m\""),
        "string role missing: {seen:?}"
    );
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn highlight_off_renders_plain_text() {
    let mut session = history_session_with(Some(&format!("{HIGHLIGHT_CONFIG}tide highlight off\n")));
    let _ = session.writer.write_all(b"echo \"$HOME\"");
    let seen = read_until(
        session.master.as_raw_fd(),
        "$HOME",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
    assert!(
        !seen.contains("\u{1b}[38;2;"),
        "highlight off emitted colour: {seen:?}"
    );
}

#[test]
fn anchor_rebinds_keys_to_actions() {
    let mut session = history_session_with(Some(
        "tide greeting off\nanchor ctrl+t kill_line\nanchor alt+l clear_screen\n",
    ));
    let _ = session.writer.write_all(b"echo killed");
    read_until(
        session.master.as_raw_fd(),
        "killed",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\x01");
    let _ = session.writer.write_all(b"\x14");
    let _ = session.writer.write_all(b"printf AFTERKILL\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=printf%20AFTERKILL\u{1b}\\"),
        "kill_line binding failed: {out:?}"
    );
    assert!(
        !out.contains("cmdline_url=printf%20AFTERKILLecho"),
        "killed line executed: {out:?}"
    );

    let _ = session.writer.write_all(b"\x1bl");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\u{1b}[2J",
        Duration::from_secs(5),
    );
    assert!(seen.contains("\u{1b}[2J"), "alt-l did not clear: {seen:?}");
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

fn completion_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sharky-complete-{}-{name}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn completion_menu_navigates_and_inserts() {
    let dir = completion_dir("menu");
    std::fs::write(dir.join("alpha.txt"), "ALPHA-CONTENT").unwrap();
    std::fs::write(dir.join("alpine.txt"), "ALPINE-CONTENT").unwrap();

    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat al");
    let _ = session.writer.write_all(b"\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "alpine.txt",
        Duration::from_secs(5),
    );
    assert!(seen.contains("alpha.txt"), "menu missing alpha: {seen:?}");
    assert!(seen.contains("alpine.txt"), "menu missing alpine: {seen:?}");

    let _ = session.writer.write_all(b"\t");
    let _ = session.writer.write_all(b"\r");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("ALPINE-CONTENT"),
        "selection did not run: {out:?}"
    );
    assert!(
        out.contains("cmdline_url=cat%20alpine.txt"),
        "wrong selection inserted: {out:?}"
    );
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn completion_inserts_a_single_candidate() {
    let dir = completion_dir("single");
    std::fs::write(dir.join("only.txt"), "ONLY-CONTENT").unwrap();

    let mut session = history_session();
    let cd_out = session.run(&format!("cd {}", dir.display()));
    assert!(cd_out.contains("133;D;0"), "cd failed: {cd_out:?}");
    let _ = session.writer.write_all(b"cat onl\t\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("ONLY-CONTENT"), "single completion: {out:?}");
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn completion_expands_variables() {
    let mut session = history_session();
    let _ = session.writer.write_all(b"echo $HOM");
    read_until(
        session.master.as_raw_fd(),
        "$HOM",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\t\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20%24HOME"),
        "variable not completed: {out:?}"
    );
    session.finish();
}

#[test]
fn completion_reads_git_branches_without_a_subprocess() {
    let dir = completion_dir("git");
    std::fs::create_dir_all(dir.join(".git/refs/heads")).unwrap();
    std::fs::write(dir.join(".git/refs/heads/main"), "0000").unwrap();
    std::fs::write(dir.join(".git/refs/heads/feature-x"), "0000").unwrap();

    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"git checkout ma\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "git checkout main",
        Duration::from_secs(5),
    );
    assert!(seen.contains("git checkout main"), "branch completion: {seen:?}");
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}


#[test]
fn fuzzy_file_search_inserts_a_match() {
    let dir = completion_dir("fuzzy");
    std::fs::write(dir.join("alpha.txt"), "ALPHA-CONTENT").unwrap();
    std::fs::write(dir.join("beta.txt"), "BETA-CONTENT").unwrap();

    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat \x14");
    let seen = read_until(session.master.as_raw_fd(), "fuzzy>", Duration::from_secs(5));
    assert!(seen.contains("beta.txt"), "fuzzy list missing files: {seen:?}");

    let _ = session.writer.write_all(b"bet");
    read_until(
        session.master.as_raw_fd(),
        "beta.txt",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\r");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        "BETA-CONTENT",
        Duration::from_secs(5),
    );
    assert!(out.contains("BETA-CONTENT"), "fuzzy choice did not run: {out:?}");
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fuzzy_file_search_cancel_keeps_the_buffer() {
    let dir = completion_dir("fuzzy-cancel");
    std::fs::write(dir.join("alpha.txt"), "ALPHA-CONTENT").unwrap();

    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat \x14");
    read_until(session.master.as_raw_fd(), "fuzzy>", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    let _ = session.writer.write_all(b"alpha.txt\r");
    let out = read_until(
        session.master.as_raw_fd(),
        "ALPHA-CONTENT",
        Duration::from_secs(5),
    );
    assert!(
        out.contains("ALPHA-CONTENT"),
        "cancel changed the buffer: {out:?}"
    );
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn completion_expands_tilde_paths() {
    let dir = std::env::temp_dir().join(format!("sharky-tilde-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("only.txt"), "TILDE-CONTENT").unwrap();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();

    let mut session = history_session();
    let _ = session
        .writer
        .write_all(format!("cat ~/{name}/onl\t\r").as_bytes());
    let out = read_until(
        session.master.as_raw_fd(),
        "TILDE-CONTENT",
        Duration::from_secs(5),
    );
    assert!(out.contains("TILDE-CONTENT"), "tilde completion: {out:?}");
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

fn decode_url(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn vi_execute(session: &mut HistorySession, initial: &str, keys: &str) -> String {
    let _ = session.writer.write_all(initial.as_bytes());
    std::thread::sleep(Duration::from_millis(40));
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(keys.as_bytes());
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    let Some(start) = out.find("cmdline_url=") else {
        return String::new();
    };
    let start = start + "cmdline_url=".len();
    let end = out[start..]
        .find("\u{1b}\\")
        .map(|offset| start + offset)
        .unwrap_or(out.len());
    decode_url(&out[start..end])
}

#[test]
fn vi_huge_counts_do_not_crash() {
    let mut session = history_session();
    session.run("set -o vi");
    let out = vi_execute(
        &mut session,
        "echo abc",
        "0l999999999999999999999l",
    );
    assert_eq!(out, "echo abc");
    session.finish();
}

#[test]
fn vi_motions_and_operators() {
    let mut session = history_session();
    session.run("set -o vi");

    let cases = [
        ("echo abc", "hx", "echo ac"),
        ("echo abc", "0lx", "eho abc"),
        ("echo abc", "0x", "cho abc"),
        ("echo abc", "$x", "echo ab"),
        ("echo one two", "0wx", "echo ne two"),
        ("echo one", "0wbx", "cho one"),
        ("echo one", "0ex", "ech one"),
        ("echo one two", "0dw", "one two"),
        ("echo abc", "0llD", "ec"),
        ("echo abc", "0cwX", "X abc"),
        ("echo abc", "0CX", "X"),
        ("echo", "dwp", "echo"),
        ("echo abc", "xu", "echo abc"),
        ("echo abc", "0rX", "Xcho abc"),
        ("a b c d", "03wx", "a b c "),
        ("echo abc", "\x7f", "echo ab"),
        ("echo abc", "ddiprintf DD", "printf DD"),
    ];

    for (initial, keys, expected) in cases {
        let command = vi_execute(&mut session, initial, keys);
        assert_eq!(command, expected, "initial={initial:?} keys={keys:?}");
    }

    let _ = session.writer.write_all(b"echo abc");
    std::thread::sleep(Duration::from_millis(40));
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"0d\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"x\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=cho%20abc"),
        "Esc did not cancel the pending operator: {out:?}"
    );

    session.finish();
}

#[test]
fn vi_history_and_mode_switches() {
    let mut session = history_session();
    session.run("set -o vi");
    session.run("echo first");
    session.run("echo second");

    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"k\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("cmdline_url=echo%20second"), "vi k: {out:?}");

    session.run("set +o vi");
    let _ = session.writer.write_all(b"echo ok\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20ok"),
        "set +o vi broke insert mode: {out:?}"
    );

    session.run("set -o vi");
    let command = vi_execute(&mut session, "echo ok", "x");
    assert_eq!(command, "echo o");
    session.finish();
}

#[test]
fn vi_normal_mode_hides_autosuggest() {
    let mut session = history_session();
    session.run("set -o vi");
    session.run("echo suggestion");

    let _ = session.writer.write_all(b"echo sug");
    read_until(
        session.master.as_raw_fd(),
        "gestion",
        Duration::from_secs(5),
    );
    read_until(
        session.master.as_raw_fd(),
        "\u{0}",
        Duration::from_millis(100),
    );
    let _ = session.writer.write_all(b"\x1b");
    let after = read_until(
        session.master.as_raw_fd(),
        "\u{0}",
        Duration::from_millis(300),
    );
    assert!(!after.contains("gestion"), "ghost in normal mode: {after:?}");

    let _ = session.writer.write_all(b"A\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20sug"),
        "normal mode changed buffer: {out:?}"
    );
    session.finish();
}

#[test]
fn a_full_width_line_does_not_move_the_cursor_up() {
    let dir = std::env::temp_dir().join(format!("sharky-width-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"> \"\ntide greeting off\n",
    )
    .unwrap();

    let pty = openpty(None, None).expect("pty");
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 20,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ copies a valid winsize into the pty.
    unsafe {
        libc::ioctl(pty.master.as_raw_fd(), libc::TIOCSWINSZ, &size);
    }

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
    read_until(fd, "> ", Duration::from_secs(5));

    // prompt (2 cols) + 18 characters fills the 20-column row exactly.
    let _ = writer.write_all(b"printf abcdefghijk");
    let seen = read_until(fd, "abcdefghijk", Duration::from_secs(5));
    assert!(
        !seen.contains("\x1b[1A"),
        "cursor moved above a full-width line: {seen:?}"
    );

    let _ = writer.write_all(b"\x03");
    let _ = read_until(fd, HISTORY_PROMPT_END, Duration::from_secs(5));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn redraw_keeps_the_prompt_on_screen() {
    let dir = std::env::temp_dir().join(format!("sharky-prompt-keep-{}", std::process::id()));
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"PROMPT> \"\ntide greeting off\n",
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
    read_until(fd, "PROMPT> ", Duration::from_secs(5));
    let _ = writer.write_all(b"x");
    let seen = read_until(fd, "x", Duration::from_secs(5));
    assert!(
        seen.contains("\r\u{1b}[8C\u{1b}[J"),
        "redraw did not clear after the prompt: {seen:?}"
    );
    assert!(
        !seen.contains("\r\u{1b}[Jx"),
        "redraw erased the prompt: {seen:?}"
    );

    let _ = writer.write_all(b"\x03");
    let _ = read_until(fd, HISTORY_PROMPT_END, Duration::from_secs(5));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}
