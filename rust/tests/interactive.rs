use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

mod common;

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

    let _ = writer.write_all(b"printf 'OUT-%s' 42\r");
    let seen = read_until(fd, "OUT-42", Duration::from_secs(5));
    assert!(
        seen.contains("OUT-42"),
        "command output missing in {seen:?}"
    );

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

    let _ = writer.write_all(b"printf 'AFTER-%s' 99\r");
    let seen = read_until(fd, "AFTER-99", Duration::from_secs(5));
    assert!(
        seen.contains("AFTER-99"),
        "shell died after a failure: {seen:?}"
    );

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
    let sh = common::resolve("sh");
    std::fs::write(&fake, format!("#!{}\nprintf 'STARSHIP-PROMPT> '\n", sh.display())).unwrap();
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

const DAY_MICROS: i64 = 86_400_000_000;

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as i64)
        .unwrap_or(0)
}

/// One v2 history record, or the bare command for a legacy record.
fn v2_history(command: &str, when: Option<i64>, hits: u64) -> String {
    match when {
        Some(when) => format!("h2\t{when}\t\t0\t{command}\t{hits}"),
        None => command.to_string(),
    }
}

struct CtrlRSession {
    child: std::process::Child,
    master: std::os::fd::OwnedFd,
    writer: std::fs::File,
    dir: std::path::PathBuf,
}

fn ctrl_r_session(lines: &[String]) -> CtrlRSession {
    let dir = std::env::temp_dir().join(format!(
        "sharky-ctrlr-rank-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "wave prompt \"P> \"\ntide greeting off\n",
    )
    .unwrap();
    let mut history = String::new();
    for line in lines {
        history.push_str(line);
        history.push('\n');
    }
    std::fs::write(dir.join("history"), history).unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("SHARKY_HISTORY", dir.join("history"))
        .env("XDG_DATA_HOME", dir.join("xdg"))
        .env("TERM", "xterm-256color")
        .env("HOME", &dir)
        .spawn()
        .expect("spawn sharky");

    let writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    read_until(pty.master.as_raw_fd(), "P> ", Duration::from_secs(5));
    CtrlRSession {
        child,
        master: pty.master,
        writer,
        dir,
    }
}

impl CtrlRSession {
    fn send(&mut self, bytes: &str) {
        let _ = self.writer.write_all(bytes.as_bytes());
    }

    fn read(&self, needle: &str) -> String {
        read_until(self.master.as_raw_fd(), needle, Duration::from_secs(5))
    }

    fn finish(mut self) {
        let _ = self.writer.write_all(b"exit\r");
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

#[test]
fn ranks_frecency_but_pins_the_newest_prefix_match() {
    let now = now_micros();
    // File order puts the newest prefix match first and the highest score last,
    // so a plain newest-first scan would show the opposite of the ranking.
    let mut session = ctrl_r_session(&[
        v2_history("deploy nightly", Some(now), 1),
        v2_history("deploy release --slow", Some(now - 60 * DAY_MICROS), 40),
        v2_history("run deploy now", Some(now - 60 * DAY_MICROS), 99),
    ]);

    session.send("\x12deploy");
    let seen = session.read("'deploy': deploy nightly");
    assert!(
        seen.contains("'deploy': deploy nightly"),
        "the newest prefix match was not pinned first: {seen:?}"
    );

    session.send("\x12");
    let seen = session.read("'deploy': run deploy now");
    assert!(
        seen.contains("'deploy': run deploy now"),
        "the highest score did not follow the pin: {seen:?}"
    );

    session.send("\x12");
    let seen = session.read("'deploy': deploy release --slow");
    assert!(
        seen.contains("'deploy': deploy release --slow"),
        "the lowest score was not last: {seen:?}"
    );

    // One more Ctrl-R at the end stays on the last match, so Down walks back one.
    session.send("\x12");
    std::thread::sleep(Duration::from_millis(100));
    session.send("\x1b[B");
    let seen = session.read("'deploy': run deploy now");
    assert!(
        seen.contains("'deploy': run deploy now"),
        "paging did not clamp at the last ranked match: {seen:?}"
    );

    session.send("\x07");
    session.finish();
}

#[test]
fn ctrl_r_prefers_frequency_over_recency_on_a_non_prefix_query() {
    let now = now_micros();
    let mut session = ctrl_r_session(&[
        v2_history("git log --all", Some(now - 60 * DAY_MICROS), 10),
        v2_history("other log", Some(now), 1),
    ]);

    session.send("\x12log");
    let seen = session.read("'log': git log --all");
    assert!(
        seen.contains("'log': git log --all"),
        "the frequent older command did not outrank the newer one: {seen:?}"
    );

    session.send("\x12");
    let seen = session.read("'log': other log");
    assert!(
        seen.contains("'log': other log"),
        "the newer, less frequent match is missing from the ranking: {seen:?}"
    );

    session.send("\x07");
    session.finish();
}

#[test]
fn ctrl_r_keeps_chronology_for_an_empty_query() {
    let now = now_micros();
    let mut session = ctrl_r_session(&[
        v2_history("printf second > $HOME/two", Some(now - 60 * DAY_MICROS), 99),
        v2_history("printf first > $HOME/one", Some(now), 1),
    ]);

    session.send("\x12");
    let seen = session.read("(reverse-i-search)");
    assert!(
        seen.contains("(reverse-i-search)"),
        "no search line: {seen:?}"
    );

    session.send("\r");
    assert!(
        wait_for(&session.dir.join("one"), Duration::from_secs(5)),
        "an empty query did not pick the newest command"
    );
    let one = std::fs::read_to_string(session.dir.join("one")).unwrap_or_default();
    let two = std::fs::read_to_string(session.dir.join("two")).unwrap_or_default();
    session.finish();

    assert_eq!(one, "first");
    assert_eq!(
        two, "",
        "an empty query ranked by frecency instead of chronology"
    );
}

#[test]
fn ctrl_r_ranks_a_legacy_record_as_old() {
    let now = now_micros();
    let mut session = ctrl_r_session(&[
        v2_history("recent log", Some(now), 1),
        v2_history("run legacy log", None, 1),
    ]);

    session.send("\x12log");
    let seen = session.read("'log': recent log");
    assert!(
        seen.contains("'log': recent log"),
        "the timestamped record did not outrank the legacy one: {seen:?}"
    );

    session.send("\x12");
    let seen = session.read("'log': run legacy log");
    assert!(
        seen.contains("'log': run legacy log"),
        "the legacy record is missing from the ranking: {seen:?}"
    );

    session.send("\x07");
    session.finish();
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
    home: std::path::PathBuf,
}

fn history_session() -> HistorySession {
    history_session_with(None)
}

fn history_session_with(config_text: Option<&str>) -> HistorySession {
    history_session_with_env(config_text, &[])
}

fn history_session_with_env(
    config_text: Option<&str>,
    extra_env: &[(&str, &str)],
) -> HistorySession {
    let data = std::env::temp_dir().join(format!(
        "sharky-histexpand-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&data).unwrap();
    let home = data.join("home");
    std::fs::create_dir_all(&home).unwrap();
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
    // A fixed width keeps every ghost assertion independent of the checkout path.
    // The prompt carries the working directory, so a longer path leaves less room on
    // the row, and an unset size leaves the editor on its 80-column fallback, where a
    // suggestion that no longer fits is dropped on purpose.
    set_terminal_width(pty.master.as_raw_fd(), 120);
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sharky"));
    cmd.stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", config_env)
        .env("TERM", "xterm-256color")
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &data);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let child = cmd.spawn().expect("spawn sharky");

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
        home,
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
    let out = session.run("printf 'STATUS-%s' \"$?\"");
    assert!(out.contains("STATUS-1"), "missing event status: {out:?}");

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
fn history_expansion_leaves_a_literal_bang_alone() {
    let mut session = history_session();
    session.run("echo seed");

    let out = session.run("echo \"quoted!\"");
    assert!(
        out.contains("cmdline_url=echo%20%22quoted%21%22"),
        "bang expanded inside double quotes: {out:?}"
    );
    assert!(out.contains("quoted!"), "quoted text lost: {out:?}");

    let out = session.run("echo foo!; echo after");
    assert!(
        out.contains("cmdline_url=echo%20foo%21"),
        "bang expanded before the semicolon: {out:?}"
    );
    assert!(out.contains("after"), "rest of the line did not run: {out:?}");

    let out = session.run("echo a!,b");
    assert!(out.contains("event not found"), "no event error: {out:?}");

    session.finish();
}

#[test]
fn interactive_exec_failure_keeps_the_shell_alive() {
    let mut session = history_session();
    let out = session.run("exec /nonexistent/sharky");
    assert!(out.contains("exec"), "no exec error in {out:?}");
    assert!(out.contains("133;D;127"), "unexpected failure status: {out:?}");
    let out = session.run("echo after");
    assert!(out.contains("after"), "shell died after failed exec: {out:?}");
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
    let _ = session.writer.write_all(b"\x1b[D\x1b[D\x1b[D");
    paste(&mut session.writer, "X");
    read_until(session.master.as_raw_fd(), "Xpre", Duration::from_secs(5));
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("Xpre"), "paste at cursor failed: {out:?}");
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
    assert!(
        out.contains("cmdline_url=%20echo%20one%0Aecho%20two"),
        "Home did not move to the line start: {out:?}"
    );
    assert!(out.contains("one"), "first line did not run: {out:?}");
    assert!(out.contains("two"), "second line did not run: {out:?}");
    session.finish();
}

#[test]
fn bracketed_paste_inserts_control_bytes_verbatim() {
    let mut session = history_session();
    paste(
        &mut session.writer,
        "printf '%s' 'a\u{3}b\u{1b}[Ac'",
    );
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("a%03b%1B%5BAc"),
        "paste dropped control bytes: {out:?}"
    );
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
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20suggested"),
        "Right did not accept the ghost: {out:?}"
    );

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
fn autosuggest_uses_a_visible_fallback_when_theme_omits_suggestion_style() {
    let config = "coral foam #E4F5F4\nwave prompt \"P> \"\nwave command @foam\ntide greeting off\n";
    let mut session = history_session_with(Some(config));
    session.run("echo suggested");
    let _ = session.writer.write_all(b"echo sug");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\x1b[2mgested\x1b[0m",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\x1b[2mgested\x1b[0m"),
        "ghost is not visually distinguished without a suggestion style: {seen:?}"
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
fn suggests_a_token_prefix_before_the_cursor() {
    let mut session = history_session();
    session.run("printf 'GHOSTTOKEN-%s' ran");

    // The token under the cursor is the only part matched, so the ghost completes
    // it and drops the rest of the line after it.
    let _ = session.writer.write_all(b"print");
    let seen = read_until(
        session.master.as_raw_fd(),
        "f 'GHOSTTOKEN-%s' ran",
        Duration::from_secs(5),
    );
    assert!(seen.contains("f 'GHOSTTOKEN-%s' ran"), "no token ghost: {seen:?}");

    // Right accepts the suffix, so the completed line really runs.
    let _ = session.writer.write_all(b"\x1b[C\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("GHOSTTOKEN-ran"), "token ghost did not run: {out:?}");

    // Mid-line, after words the history record never had, the same token still
    // drives the ghost.
    let _ = session.writer.write_all(b"x = print");
    let seen = read_until(
        session.master.as_raw_fd(),
        "f 'GHOSTTOKEN-%s' ran",
        Duration::from_secs(5),
    );
    assert!(seen.contains("f 'GHOSTTOKEN-%s' ran"), "no mid-line ghost: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn skips_a_suggestion_whose_path_is_gone() {
    let mut session = history_session();
    let dir = session.data.join("work-7788");
    std::fs::create_dir_all(&dir).unwrap();
    // Quoted: the session directory carries parentheses in its name.
    session.run(&format!("cd '{}'", dir.display()));
    let out = session.run("pwd");
    assert!(out.contains("work-7788"), "cd did not land: {out:?}");
    session.run("touch stale.txt stale.yml");
    session.run("cat stale.yml");
    session.run("cat stale.txt");
    // Removed here, not by `rm`: a removal the shell runs is itself recorded after
    // it took effect, so that record names no paths and stays suggestible, which
    // would put `.txt` back in the ghost for a longer prefix.
    std::fs::remove_file(dir.join("stale.txt")).unwrap();

    // `cat stale.txt` is newer, but the file it named is gone; the search has to
    // keep walking back to `cat stale.yml`.
    let _ = session.writer.write_all(b"cat stale");
    let seen = read_until(session.master.as_raw_fd(), ".yml", Duration::from_secs(5));
    assert!(seen.contains(".yml"), "no fallback ghost: {seen:?}");
    assert!(!seen.contains(".txt"), "stale path suggested anyway: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn suggests_a_command_that_named_a_missing_path() {
    let mut session = history_session();
    // Nothing exists at the path, so the record names no paths at all; the command
    // stays suggestible instead of being hidden for naming something absent.
    session.run("cat GHOSTMISSING");

    let _ = session.writer.write_all(b"cat GHOSTMIS");
    let seen = read_until(session.master.as_raw_fd(), "SING", Duration::from_secs(5));
    assert!(seen.contains("SING"), "missing path hid the command: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn prefers_case_sensitive_over_case_insensitive() {
    let mut session = history_session();
    session.run("echo CASETHING");
    session.run("echo caseOther");

    // The newer entry only matches case-insensitively; the exact-cased older one
    // wins. The redraws typed before the last character are drained first, so the
    // assertion only sees what the folded case would have shown.
    let _ = session.writer.write_all(b"echo CAS");
    read_until(
        session.master.as_raw_fd(),
        "\u{0}",
        Duration::from_millis(100),
    );
    let _ = session.writer.write_all(b"E");
    let seen = read_until(session.master.as_raw_fd(), "THING", Duration::from_secs(5));
    assert!(seen.contains("THING"), "no exact-case ghost: {seen:?}");
    assert!(!seen.contains("Other"), "folded-case ghost won: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn suggests_the_newest_match_first() {
    let mut session = history_session();
    session.run("echo alpha GHOSTOLD");
    session.run("echo alpha GHOSTNEW");

    let _ = session.writer.write_all(b"echo alpha GH");
    let seen = read_until(session.master.as_raw_fd(), "OSTNEW", Duration::from_secs(5));
    assert!(seen.contains("OSTNEW"), "newest match did not win: {seen:?}");
    assert!(!seen.contains("OSTOLD"), "old match won: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn suggests_a_legacy_record_without_paths() {
    let mut session = history_session();
    let file = session.data.join("sharky/history");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "echo LEGACYWITNESS\n").unwrap();
    session.run("true");

    let _ = session.writer.write_all(b"echo LEG");
    let seen = read_until(session.master.as_raw_fd(), "ACYWITNESS", Duration::from_secs(5));
    assert!(seen.contains("ACYWITNESS"), "legacy record hidden: {seen:?}");

    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
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
coral arg #112233
wave command @sea
wave builtin @kelp
wave string @kelp
wave variable @danger
wave argument @arg
wave error @danger
tide greeting off
";

#[test]
fn highlight_paints_command_strings_and_variables() {
    let mut session = history_session_with(Some(HIGHLIGHT_CONFIG));
    let _ = session.writer.write_all(b"ls \"$HOME\"");
    let seen = read_until(
        session.master.as_raw_fd(),
        "$HOME\u{1b}[0m\u{1b}[38;2;140;207;168m\"",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\u{1b}[38;2;53;184;197mls\u{1b}[0m"),
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

    let _ = session.writer.write_all(b" hello");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\u{1b}[38;2;17;34;51m hello\u{1b}[0m",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\u{1b}[38;2;17;34;51m hello\u{1b}[0m"),
        "argument role missing: {seen:?}"
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

const OVERWRITE_CONFIG: &str = "coral danger #FF8F80
wave overwrite @danger
tide greeting off
";

const OVERWRITE_DANGER: &str = "\u{1b}[38;2;255;143;128m";

// Inserts a redirect line without pressing Enter, captures the highlighted
// redraw, then cancels. The target is a literal absolute path to a regular
// file created only when `exists` is true. It lives in a private directory so
// that intermediate prefixes of the typed path cannot collide with unrelated
// files in the temp directory. `deliver` picks typing vs. paste; `extra_env`
// exercises colour gates such as NO_COLOR.
fn overwrite_case<F>(
    exists: bool,
    build: F,
    extra_env: &[(&str, &str)],
    deliver: fn(&mut std::fs::File, &str),
) -> (String, String)
where
    F: Fn(&str) -> String,
{
    static DIRSEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let mut session = history_session_with_env(Some(OVERWRITE_CONFIG), extra_env);
    let dir = std::env::temp_dir().join(format!(
        "sharky-overwrite-{}-{}",
        std::process::id(),
        DIRSEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("target").to_str().unwrap().to_string();
    if exists {
        std::fs::write(&target, b"old").unwrap();
    }
    let command = build(&target);
    let fd = session.master.as_raw_fd();
    deliver(&mut session.writer, &command);
    let seen = read_until(fd, "\u{0}", Duration::from_millis(500));
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(fd, HISTORY_PROMPT_END, Duration::from_secs(5));
    session.finish();
    std::fs::remove_file(&target).ok();
    std::fs::remove_dir(&dir).ok();
    (seen, target)
}

fn type_line(writer: &mut std::fs::File, text: &str) {
    let _ = writer.write_all(text.as_bytes());
}

fn overwrite_render<F>(exists: bool, build: F) -> (String, String)
where
    F: Fn(&str) -> String,
{
    overwrite_case(exists, build, &[], type_line)
}

fn overwrite_paste<F>(exists: bool, build: F) -> (String, String)
where
    F: Fn(&str) -> String,
{
    overwrite_case(exists, build, &[], paste)
}

fn overwrite_no_color<F>(exists: bool, build: F) -> (String, String)
where
    F: Fn(&str) -> String,
{
    overwrite_case(exists, build, &[("NO_COLOR", "1")], type_line)
}

#[test]
fn overwrite_warning_paints_an_existing_redirect_target() {
    for redirect in [">", "2>", "&>"] {
        let (seen, target) =
            overwrite_render(true, |target| format!("echo hi {redirect} {target}"));
        let painted = format!("{OVERWRITE_DANGER}{target}");
        assert!(
            seen.contains(&painted),
            "{redirect} did not paint the existing target: {seen:?}"
        );
    }
}

#[test]
fn overwrite_warning_ignores_non_overwrite_redirects() {
    let (append, _) = overwrite_render(true, |t| format!("echo hi >> {t}"));
    assert!(
        !append.contains(OVERWRITE_DANGER),
        "append painted a target: {append:?}"
    );

    let (input, _) = overwrite_render(true, |t| format!("echo hi < {t}"));
    assert!(
        !input.contains(OVERWRITE_DANGER),
        "input painted a target: {input:?}"
    );

    let (dup, _) = overwrite_render(true, |_| "echo hi 2>&1".to_string());
    assert!(
        !dup.contains(OVERWRITE_DANGER),
        "fd duplication painted a target: {dup:?}"
    );

    let (missing, _) = overwrite_render(false, |t| format!("echo hi > {t}"));
    assert!(
        !missing.contains(OVERWRITE_DANGER),
        "nonexistent target was painted: {missing:?}"
    );
}

#[test]
fn overwrite_warning_ignores_escaped_operator() {
    let (seen, _) = overwrite_render(true, |t| format!("echo hi \\> {t}"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "escaped '>' painted a target: {seen:?}"
    );
}

#[test]
fn overwrite_warning_ignores_read_write_redirect() {
    let (seen, _) = overwrite_render(true, |t| format!("echo hi <> {t}"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "'<>' painted a target: {seen:?}"
    );
}

#[test]
fn overwrite_warning_ignores_test_comparison() {
    let (seen, _) = overwrite_render(true, |t| format!("[[ a > {t} ]]"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "'[[ ... > ... ]]' painted a target: {seen:?}"
    );
}

#[test]
fn overwrite_warning_ignores_arithmetic_comparison() {
    let (seen, _) = overwrite_render(true, |t| format!("(( 0 > {t} ))"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "'(( ... > ... ))' painted a target: {seen:?}"
    );
}

#[test]
fn overwrite_warning_ignores_heredoc_body() {
    let (seen, _) = overwrite_paste(true, |t| format!("cat <<EOF\n> {t}\nEOF"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "heredoc body painted a target: {seen:?}"
    );
}

#[test]
fn overwrite_warning_respects_no_color() {
    let (seen, _) = overwrite_no_color(true, |t| format!("echo hi > {t}"));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "NO_COLOR still painted a target: {seen:?}"
    );
}

// v1 covers unquoted static literals only; simple quoted literals are a
// separate assessment and must not be highlighted yet.
#[test]
fn overwrite_warning_leaves_quoted_literals_for_a_later_slice() {
    let (seen, _) = overwrite_render(true, |t| format!("echo hi > \"{t}\""));
    assert!(
        !seen.contains(OVERWRITE_DANGER),
        "quoted target painted beyond v1 scope: {seen:?}"
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

fn set_terminal_width(fd: std::os::fd::RawFd, columns: u16) {
    let size = libc::winsize {
        ws_row: 24,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe {
        libc::ioctl(fd, libc::TIOCSWINSZ, &size);
    }
}

#[test]
fn completion_menu_stays_closed_in_a_narrow_terminal() {
    let dir = completion_dir("narrow");
    std::fs::write(dir.join("alpha.txt"), "ALPHA").unwrap();
    std::fs::write(dir.join("alpine.txt"), "ALPINE").unwrap();
    let mut session = history_session();
    set_terminal_width(session.master.as_raw_fd(), 10);
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat al\t");
    std::thread::sleep(Duration::from_millis(150));
    let seen = read_until(session.master.as_raw_fd(), "alp", Duration::from_millis(200));
    assert!(!seen.contains("alpha.txt"), "menu opened in 10 columns: {seen:?}");
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
fn completion_menu_keeps_the_selection_visible() {
    let dir = completion_dir("window");
    for index in 0..10 {
        std::fs::write(dir.join(format!("aa{index}.txt")), "X").unwrap();
    }
    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat aa");
    let _ = session.writer.write_all(b"\t\t\t\t\t\t\t\t\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "> aa8.txt",
        Duration::from_secs(5),
    );
    assert!(seen.contains("> aa8.txt"), "selection left the window: {seen:?}");
    let _ = session.writer.write_all(b"\x1b");
    let _ = read_until(
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
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn completion_escapes_file_names() {
    let dir = completion_dir("escape");
    let marker = dir.join("pwned");
    let name = "zz;touch pwned";
    std::fs::write(dir.join(name), "SAFE").unwrap();

    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat zz");
    let _ = session.writer.write_all(b"\t");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("SAFE"), "file was not read: {out:?}");
    assert!(!marker.exists(), "completion ran the file name: {out:?}");
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fuzzy_files_restores_the_screen_on_escape() {
    let mut session = history_session();
    session.run("printf SCREEN_MARKER");
    let _ = session.writer.write_all(b"\x14");
    let seen = read_until(session.master.as_raw_fd(), "fuzzy>", Duration::from_secs(5));
    assert!(seen.contains("fuzzy>"), "pager did not open: {seen:?}");
    let _ = session.writer.write_all(b"\x1b");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\x1b[?1049l",
        Duration::from_secs(5),
    );
    assert!(seen.contains("\x1b[?1049l"), "screen not restored: {seen:?}");
    session.finish();
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
    session.run("SHARKY_COMPLETION_TEST_TARGET=value");
    let _ = session
        .writer
        .write_all(b"echo $SHARKY_COMPLETION_TEST_TAR");
    read_until(
        session.master.as_raw_fd(),
        "$SHARKY_COMPLETION_TEST_TAR",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\t\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20%24SHARKY_COMPLETION_TEST_TARGET"),
        "variable not completed: {out:?}"
    );
    session.finish();
}

#[test]
fn completion_completes_the_last_segment_of_a_repeated_dollar_word() {
    let mut session = history_session();
    session.run("SHARKY_COMPLETION_TEST_TARGET=value");
    let _ = session
        .writer
        .write_all(b"echo $$SHARKY_COMPLETION_TEST_TAR\t\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20%24%24SHARKY_COMPLETION_TEST_TARGET"),
        "repeated-dollar word not completed: {out:?}"
    );
    session.finish();
}

#[test]
fn completing_an_exact_repeated_dollar_word_does_not_panic() {
    let mut session = history_session();
    session.run("SHARKY_COMPLETION_TEST_TARGET=value");
    let _ = session
        .writer
        .write_all(b"echo $$SHARKY_COMPLETION_TEST_TARGET\t\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(!out.contains("panicked"), "completion panicked: {out:?}");
    assert!(
        out.contains("cmdline_url=echo%20%24%24SHARKY_COMPLETION_TEST_TARGET"),
        "exact repeated-dollar word changed: {out:?}"
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
    // The picker draws its prompt and its list in one frame, and the pty may hand
    // that frame over in pieces: wait for the entry the assertion is about.
    let seen = read_until(session.master.as_raw_fd(), "beta.txt", Duration::from_secs(5));
    assert!(
        seen.contains("fuzzy>") && seen.contains("beta.txt"),
        "fuzzy list missing files: {seen:?}"
    );

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
    let mut session = history_session();
    let dir = session
        .home
        .join(format!("sharky-tilde-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("only.txt"), "TILDE-CONTENT").unwrap();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();

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

#[test]
fn completion_includes_builtins_without_a_path() {
    let mut session = history_session();
    session.run("PATH=/nonexistent");
    let _ = session.writer.write_all(b"rel");
    let _ = session.writer.write_all(b"\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "reload",
        Duration::from_secs(5),
    );
    assert!(seen.contains("reload"), "builtin not completed: {seen:?}");
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn completion_appends_a_slash_to_directories() {
    let mut session = history_session();
    let dir = session
        .home
        .join(format!("sharky-dironly-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let prefix = &name[.."sharky-dironly".len()];
    let _ = session
        .writer
        .write_all(format!("ls ~/{prefix}\t").as_bytes());
    let seen = read_until(
        session.master.as_raw_fd(),
        &name,
        Duration::from_secs(5),
    );
    assert!(seen.contains(&format!("{name}/")), "missing slash: {seen:?}");
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
fn completion_escapes_path_executables() {
    use std::os::unix::fs::PermissionsExt;

    let mut session = history_session();
    let dir = completion_dir("inject");
    let marker = dir.join("pwned");
    let name = "zz;touch pwned";
    let path = dir.join(name);
    let sh = common::resolve("sh");
    std::fs::write(&path, format!("#!{}\nprintf SAFE\n", sh.display())).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    session.run(&format!("cd {}", dir.display()));
    session.run(&format!("PATH={}", dir.display()));
    let _ = session.writer.write_all(b"zz");
    let _ = session.writer.write_all(b"\t");
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("SAFE"), "escaped command did not run: {out:?}");
    assert!(
        !marker.exists(),
        "command completion injected shell code: {out:?}"
    );
    session.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn completion_offers_exec_without_a_path() {
    let mut session = history_session();
    session.run("PATH=/nonexistent");
    let _ = session.writer.write_all(b"ex");
    let _ = session.writer.write_all(b"\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "exec",
        Duration::from_secs(5),
    );
    assert!(seen.contains("exec"), "exec missing from completion: {seen:?}");
    let _ = session.writer.write_all(b"\x03");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    session.finish();
}

#[test]
fn paste_content_ending_in_escape_keeps_the_end_marker() {
    let mut session = history_session();
    let _ = session.writer.write_all(b"\x1b[200~echo END\x1b\x1b[201~");
    std::thread::sleep(Duration::from_millis(60));
    let _ = session.writer.write_all(b"\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("END"), "paste never finished: {out:?}");
    session.finish();
}

#[test]
fn completion_menu_closes_on_resize() {
    let dir = completion_dir("resize");
    for index in 0..4 {
        std::fs::write(dir.join(format!("aa{index}.txt")), "X").unwrap();
    }
    let mut session = history_session();
    session.run(&format!("cd {}", dir.display()));
    let _ = session.writer.write_all(b"cat aa\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "aa0.txt",
        Duration::from_secs(5),
    );
    assert!(seen.contains("aa0.txt"), "menu did not open: {seen:?}");

    set_terminal_width(session.master.as_raw_fd(), 10);
    let _ = session.writer.write_all(b"\t");
    let seen = read_until(
        session.master.as_raw_fd(),
        "aa0.txt",
        Duration::from_millis(500),
    );
    assert!(
        !seen.contains("aa0.txt"),
        "menu survived the resize: {seen:?}"
    );
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
fn process_substitution_keeps_the_terminal_foreground() {
    use std::os::unix::process::CommandExt;

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
    command
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp");
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    read_until(fd, "❯", Duration::from_secs(5));

    let _ = writer.write_all(b"true <(sleep 0.15)\r");
    std::thread::sleep(Duration::from_millis(400));
    let foreground = unsafe { libc::tcgetpgrp(fd) };
    assert_eq!(
        foreground as u32,
        child.id(),
        "process substitution took the terminal"
    );

    let _ = writer.write_all(b"printf 'INPUT-%s' OK\r");
    let seen = read_until(fd, "INPUT-OK", Duration::from_secs(5));
    assert!(seen.contains("INPUT-OK"), "prompt stopped reading: {seen:?}");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
}

#[test]
fn idle_reaps_finished_substitutions() {
    let mut session = history_session();
    session.run("true <(sleep 0.1)");
    std::thread::sleep(Duration::from_millis(900));
    let shell = session.child.id();
    let children = std::fs::read_to_string(format!("/proc/{shell}/task/{shell}/children"))
        .unwrap_or_default();
    for pid in children.split_whitespace() {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let state = stat
            .rsplit(')')
            .next()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .unwrap_or("");
        assert_ne!(state, "Z", "child {pid} is still a zombie while idle");
    }
    session.finish();
}

#[test]
fn sigterm_in_fuzzy_restores_the_screen() {
    let mut session = history_session();
    let _ = session.writer.write_all(b"\x14");
    let seen = read_until(session.master.as_raw_fd(), "fuzzy>", Duration::from_secs(5));
    assert!(seen.contains("fuzzy>"), "pager did not open: {seen:?}");
    let pid = nix::unistd::Pid::from_raw(session.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("signal");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\x1b[?1049l",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\x1b[?1049l"),
        "SIGTERM left the alternate screen: {seen:?}"
    );
    let _ = session.child.wait();
    std::fs::remove_dir_all(&session.data).ok();
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
fn sigterm_restores_the_terminal() {
    let mut session = history_session();
    let pid = nix::unistd::Pid::from_raw(session.child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).expect("signal");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\x1b[?2004l",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\x1b[?2004l"),
        "SIGTERM left paste mode on: {seen:?}"
    );
    let status = session.child.wait().expect("wait");
    assert!(!status.success());
    std::fs::remove_dir_all(&session.data).ok();
}

#[test]
fn undo_is_reachable_from_ctrl_underscore() {
    let mut session = history_session();
    let _ = session.writer.write_all(b"echo abc");
    std::thread::sleep(Duration::from_millis(60));
    let _ = session.writer.write_all(b"\x7f");
    std::thread::sleep(Duration::from_millis(60));
    let _ = session.writer.write_all(b"\x1f\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=echo%20abc"),
        "Ctrl-_ did not undo: {out:?}"
    );
    session.finish();
}

#[test]
fn vi_escape_cancels_a_pending_operator_without_moving() {
    let mut session = history_session();
    session.run("set -o vi");
    let _ = session.writer.write_all(b"echo abc");
    std::thread::sleep(Duration::from_millis(40));
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"0lld");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(80));
    let _ = session.writer.write_all(b"x\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=eco%20abc"),
        "Esc moved the cursor or kept the operator: {out:?}"
    );
    session.finish();
}

#[test]
fn vi_count_deletes_all_requested_lines() {
    let mut session = history_session();
    session.run("set -o vi");
    paste(&mut session.writer, "one\ntwo\nthree");
    read_until(
        session.master.as_raw_fd(),
        "three",
        Duration::from_secs(5),
    );
    let _ = session.writer.write_all(b"\x1b");
    std::thread::sleep(Duration::from_millis(60));
    let _ = session.writer.write_all(b"\x1b[A\x1b[A0");
    let _ = session.writer.write_all(b"2dd\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(
        out.contains("cmdline_url=three"),
        "2dd did not remove two lines: {out:?}"
    );
    session.finish();
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
        ("echo one two", "02ex", "echo on two"),
        ("echo abc", "0x3p", "ceeeho abc"),
        ("echo abc", "0x3P", "eeecho abc"),
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
    let _ = writer.write_all(b"X");
    let seen = read_until(fd, "X", Duration::from_secs(5));
    assert!(
        !seen.contains("\x1b[1A"),
        "cursor moved above after wrapping: {seen:?}"
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

struct SharedSession {
    child: std::process::Child,
    master: std::os::fd::OwnedFd,
    writer: std::fs::File,
}

fn shared_fixture(name: &str) -> std::path::PathBuf {
    let dir = std::path::Path::new("/tmp/opencode").join(format!(
        "sharky-shared-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

fn shared_session(dir: &std::path::Path, args: &[&str]) -> SharedSession {
    shared_session_with_startup(dir, args).0
}

fn shared_session_with_startup(dir: &std::path::Path, args: &[&str]) -> (SharedSession, String) {
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
        .env("HOME", dir.join("home"))
        .env("SHARKY_HISTORY", dir.join("history"))
        .spawn()
        .expect("spawn sharky");

    let writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let session = SharedSession {
        child,
        master: pty.master,
        writer,
    };
    let startup = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    (session, startup)
}

impl SharedSession {
    fn run(&mut self, line: &str) -> String {
        let fd = self.master.as_raw_fd();
        let _ = self.writer.write_all(format!("{line}\r").as_bytes());
        read_until(fd, HISTORY_PROMPT_END, Duration::from_secs(5))
    }

    fn finish(mut self) {
        let _ = self.writer.write_all(b"exit\r");
        let _ = self.child.wait();
    }
}

/// Writes one command into the shared file behind a running shell's back, the way
/// another terminal would.
fn append_foreign(dir: &std::path::Path, command: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dir.join("history"))
        .unwrap();
    writeln!(file, "{command}").unwrap();
}

#[test]
fn two_shells_see_each_others_entries_without_merge() {
    let dir = shared_fixture("merge-visibility");
    let mut a = shared_session(&dir, &[]);
    let mut b = shared_session(&dir, &[]);

    a.run("printf from-sibling > $HOME/from_sibling");
    assert!(wait_for(&dir.join("home/from_sibling"), Duration::from_secs(5)));

    // The view is a snapshot taken at the prompt, so a command a sibling wrote
    // afterwards is not in it yet. The next prompt refreshes it, and an empty line
    // is enough to reach one.
    b.run("history > $HOME/before");
    let before = std::fs::read_to_string(dir.join("home/before")).unwrap();
    assert!(
        !before.contains("from-sibling"),
        "entry arrived without a prompt refresh: {before:?}"
    );

    b.run("");
    b.run("history > $HOME/after");
    let after = std::fs::read_to_string(dir.join("home/after")).unwrap();
    assert!(
        after.contains("printf from-sibling > $HOME/from_sibling"),
        "the next prompt did not refresh the sibling entry: {after:?}"
    );

    // `history merge` is the forced form of the same path, so it changes nothing.
    b.run("history merge");
    b.run("history > $HOME/merged");
    let merged = std::fs::read_to_string(dir.join("home/merged")).unwrap();
    assert_eq!(
        merged
            .matches("printf from-sibling > $HOME/from_sibling")
            .count(),
        1,
        "merge duplicated or dropped the sibling entry: {merged:?}"
    );

    a.finish();
    b.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_foreign_append_is_visible_at_the_next_prompt() {
    let dir = shared_fixture("refresh-visible");
    let mut a = shared_session(&dir, &[]);
    a.run("history -c");
    a.run("printf mine");

    append_foreign(&dir, "printf ghost-foreign");

    // No `history merge`, no command: an empty line reaches the next primary prompt
    // and that prompt has already refreshed.
    a.run("");
    a.run("history > $HOME/list");
    let list = std::fs::read_to_string(dir.join("home/list")).unwrap();
    assert!(
        list.contains("printf ghost-foreign"),
        "the prompt missed a foreign append: {list:?}"
    );

    // The editor sees it too: a prefix gets the rest from the ghost.
    let _ = a.writer.write_all(b"printf gh");
    let seen = read_until(a.master.as_raw_fd(), "ost-foreign", Duration::from_secs(5));
    assert!(
        seen.contains("ost-foreign"),
        "no ghost from a refreshed entry: {seen:?}"
    );
    let _ = a.writer.write_all(b"\x03");
    let _ = read_until(
        a.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );

    a.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn expansion_ignores_a_command_a_refresh_brought_in() {
    let dir = shared_fixture("refresh-expansion");
    let mut a = shared_session(&dir, &[]);
    a.run("history -c");
    a.run("printf local-base");

    // The foreign command is the newest record in the file and the newest entry in
    // the list, so a guard that looked at the whole view would expand to it.
    append_foreign(&dir, "printf foreign-base");
    a.run("");

    let out = a.run("!!");
    assert!(
        out.contains("printf local-base"),
        "!! did not expand the local command: {out:?}"
    );
    assert!(
        !out.contains("foreign-base"),
        "!! expanded a command another session ran: {out:?}"
    );

    // The entry really is in the view: the expansion guard is what ignored it.
    a.run("history > $HOME/list");
    let list = std::fs::read_to_string(dir.join("home/list")).unwrap();
    assert!(
        list.contains("printf foreign-base"),
        "the guard test needs the refreshed entry in the view: {list:?}"
    );

    a.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_continuation_line_does_not_refresh() {
    let dir = shared_fixture("refresh-continuation");
    let mut a = shared_session(&dir, &[]);
    a.run("printf setup");

    // From here on every read of the shared file fails with a message on this same
    // terminal, so a refresh is impossible to miss.
    std::fs::remove_file(dir.join("history")).unwrap();
    std::fs::create_dir(dir.join("history")).unwrap();

    // An unterminated quote asks for more input instead of running anything; the
    // line that draws the continuation prompt is where a refresh would happen.
    let _ = a.writer.write_all(b"echo \"open\r");
    let seen = read_until(a.master.as_raw_fd(), "... ", Duration::from_secs(5));
    assert!(seen.contains("... "), "no continuation prompt: {seen:?}");
    assert!(
        !seen.contains("history"),
        "a continuation line refreshed the file: {seen:?}"
    );

    // Closing the command reaches a primary prompt, where the refresh runs and the
    // refused path is reported instead of the shell going quiet about it.
    let _ = a.writer.write_all(b"\"\r");
    let out = read_until(
        a.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    assert!(out.contains("open"), "the continued command did not run: {out:?}");
    assert!(
        out.contains("not a regular file"),
        "the primary prompt did not refresh the broken path: {out:?}"
    );

    a.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn stale_shell_exit_does_not_clobber_live_history() {
    let dir = shared_fixture("stale-exit");
    let mut a = shared_session(&dir, &[]);
    let b = shared_session(&dir, &[]);

    a.run("printf survivor > $HOME/survivor");
    assert!(wait_for(&dir.join("home/survivor"), Duration::from_secs(5)));

    // B never observed A's entry and exits with its stale in-memory view.
    b.finish();

    let text = std::fs::read_to_string(dir.join("history")).unwrap();
    assert!(
        text.contains("printf survivor > $HOME/survivor"),
        "stale exit clobbered the file: {text:?}"
    );

    let mut c = shared_session(&dir, &[]);
    c.run("history > $HOME/fresh");
    let fresh = std::fs::read_to_string(dir.join("home/fresh")).unwrap();
    assert!(
        fresh.contains("printf survivor > $HOME/survivor"),
        "fresh shell lost the entry: {fresh:?}"
    );

    a.finish();
    c.finish();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn command_is_persisted_after_abrupt_termination() {
    let dir = shared_fixture("abrupt");
    let mut a = shared_session(&dir, &[]);

    a.run("printf abrupt-cmd > $HOME/abrupt");
    assert!(wait_for(&dir.join("home/abrupt"), Duration::from_secs(5)));

    a.child.kill().expect("kill shell");
    let _ = a.child.wait();

    let text = std::fs::read_to_string(dir.join("history")).unwrap();
    assert!(
        text.contains("printf abrupt-cmd > $HOME/abrupt"),
        "command lost on SIGKILL: {text:?}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn private_mode_starts_empty_and_leaves_history_bytes_untouched() {
    let dir = shared_fixture("private");
    let seed = "seeded command\n";
    std::fs::write(dir.join("history"), seed).unwrap();
    let before = std::fs::read(dir.join("history")).unwrap();

    let mut a = shared_session(&dir, &["--private"]);
    a.run("history > $HOME/private-list");
    let list = std::fs::read_to_string(dir.join("home/private-list")).unwrap();
    assert!(list.is_empty(), "private shell saw prior history: {list:?}");

    let out = a.run("history merge");
    assert!(
        out.contains("can't merge history in private mode"),
        "private merge error not surfaced: {out:?}"
    );

    a.run("printf ran-private > $HOME/private-ran");
    assert!(wait_for(&dir.join("home/private-ran"), Duration::from_secs(5)));
    a.finish();

    assert_eq!(
        std::fs::read(dir.join("history")).unwrap(),
        before,
        "private mode wrote history bytes"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn refused_history_file_warns_and_clear_never_reports_success() {
    use std::os::unix::fs::symlink;

    let dir = shared_fixture("refused-load");
    let target = dir.join("target");
    std::fs::write(&target, "seeded command\n").unwrap();
    let before = std::fs::read(&target).unwrap();
    symlink(&target, dir.join("history")).unwrap();

    let (mut a, startup) = shared_session_with_startup(&dir, &[]);
    assert!(
        startup.contains("refusing history path"),
        "no startup load diagnostic: {startup:?}"
    );

    a.run("history > $HOME/list");
    let list = std::fs::read_to_string(dir.join("home/list")).unwrap();
    assert!(list.is_empty(), "refused file was read: {list:?}");

    let cleared = a.run("history -c");
    assert!(
        cleared.contains("refusing history path"),
        "no clear diagnostic: {cleared:?}"
    );
    assert!(
        cleared.contains("\x1b]133;D;1\x1b\\"),
        "failed clear reported success: {cleared:?}"
    );

    let merged = a.run("history merge");
    assert!(
        merged.contains("refusing history path"),
        "merge hid the load failure: {merged:?}"
    );

    a.finish();
    assert_eq!(
        std::fs::read(&target).unwrap(),
        before,
        "refused file was modified"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn fish_like_prefix_history_and_word_by_word_autosuggest() {
    let mut session = history_session();
    session.run("history -c");
    session.run("echo MARK_ONE MARK_TWO MARK_THREE");
    session.run("echo MARK_ONE");
    session.run("printf 'UNRELATED\\n'");

    // Ctrl+Right (\x1b[1;5C) at end of buffer accepts one suggested word at a time.
    let _ = session
        .writer
        .write_all(b"echo MARK_\x1b[1;5C\r");
    let out = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    let executed = out
        .split_once("cmdline_url=")
        .map(|(_, after)| after)
        .unwrap_or(out.as_str());
    assert!(executed.contains("MARK_ONE"), "word accept missing in {out:?}");
    assert!(!executed.contains("MARK_TWO"), "accepted whole line instead of one word: {out:?}");

    // Up with prefix filters history and Down walks back to the draft.
    let _ = session
        .writer
        .write_all(b"echo MARK_O\x1b[A\x1b[A\x1b[B\x1b[BK\r");
    let out = read_until(
        session.master.as_raw_fd(),
        "MARK_OK",
        Duration::from_secs(5),
    );
    assert!(out.contains("MARK_OK"), "draft was not restored cleanly: {out:?}");

    session.finish();
}

#[test]
fn fish_like_autocd_shopt_and_did_you_mean() {
    let mut session = history_session();
    session.run("mkdir -p $HOME/workdir && cd $HOME");

    session.run("workdir");
    let pwd_out = session.run("pwd");
    assert!(
        pwd_out.contains("workdir"),
        "autocd did not change directory: {pwd_out:?}"
    );

    let query = session.run("shopt autocd");
    assert!(query.contains("autocd\ton"), "unexpected shopt autocd query: {query:?}");

    session.run("cd $HOME && shopt -u autocd");
    let disabled = session.run("workdir");
    assert!(
        disabled.contains("command not found: workdir"),
        "shopt -u autocd did not disable autocd: {disabled:?}"
    );

    let suggestion = session.run("echp hello");
    assert!(
        suggestion.contains("did you mean 'echo'"),
        "missing did-you-mean suggestion: {suggestion:?}"
    );

    let non_interactive = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", "echp hello"])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("run non-interactive");
    let stderr = String::from_utf8_lossy(&non_interactive.stderr);
    assert!(
        !stderr.contains("did you mean"),
        "non-interactive shell should not emit interactive typo hint: {stderr:?}"
    );

    session.finish();
}

#[test]
fn fish_like_error_highlight_and_prompt_tokens() {
    let config = format!(
        "{HIGHLIGHT_CONFIG}wave prompt \"{{cwd_short}} {{git}} {{duration}}{{symbol}} \"\nwave prompt.git @kelp\nwave prompt.error @danger\n"
    );
    let mut session = history_session_with(Some(&config));
    let repo = session.home.join("deep/project");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/fish-ux\n").unwrap();

    let entered = session.run("cd $HOME/deep/project");
    assert!(entered.contains("fish-ux"), "git branch missing from prompt: {entered:?}");
    assert!(entered.contains("~/d/project"), "cwd_short missing from prompt: {entered:?}");

    let failed = session.run("false");
    assert!(
        failed.contains("\u{1b}[38;2;255;143;128m❯\u{1b}[0m"),
        "error-colored prompt symbol missing on failure: {failed:?}"
    );

    let _ = session.writer.write_all(b"nosuchcmd_xy");
    let seen = read_until(
        session.master.as_raw_fd(),
        "\u{1b}[38;2;255;143;128mnosuchcmd_xy\u{1b}[0m",
        Duration::from_secs(5),
    );
    assert!(
        seen.contains("\u{1b}[38;2;255;143;128mnosuchcmd_xy\u{1b}[0m"),
        "unknown command not highlighted with @error: {seen:?}"
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
fn fish_like_multi_position_abbr_and_cd_tab_completion() {
    let config = "wave prompt \"P> \"\ntide greeting off\nshoal markit \"printf MULTI > $HOME/multi\"\n";
    let mut session = history_session_with(Some(config));
    let marker = session.home.join("multi");

    session.run("true && markit");
    assert_eq!(std::fs::read_to_string(&marker).unwrap_or_default(), "MULTI");

    std::fs::remove_file(&marker).ok();
    session.run("echo 'markit'");
    assert!(
        !marker.exists(),
        "quoted abbreviation should not expand"
    );

    let dir = session.home.join("OnlyDir");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(session.home.join("onlyfile.txt"), b"x").unwrap();

    session.run("cd $HOME");
    let _ = session.writer.write_all(b"cd only\t\r");
    let _ = read_until(
        session.master.as_raw_fd(),
        HISTORY_PROMPT_END,
        Duration::from_secs(5),
    );
    let pwd_out = session.run("pwd");
    assert!(
        pwd_out.contains("OnlyDir"),
        "cd Tab completion did not enter OnlyDir: {pwd_out:?}"
    );

    session.finish();
}

fn strip_ansi_csi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\x07' {
                            break;
                        }
                        if next == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        out.push(ch);
    }
    out
}

#[test]
fn omp_can_provide_the_prompt() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("sharky-omp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    let bin = dir.join("bin");
    let log = dir.join("omp-args.log");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "tide greeting off\ntide prompt omp\ntide omp_config \"/tmp/demo.omp.json\"\n",
    )
    .unwrap();

    let fake = bin.join("oh-my-posh");
    let sh = common::resolve("sh");
    std::fs::write(
        &fake,
        format!(
            "#!{}\nprintf '%s\\n' \"$*\" > \"{}\"\nprintf 'OMP-PROMPT> '\n",
            sh.display(),
            log.display()
        ),
    )
    .unwrap();
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
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let seen = read_until(fd, "OMP-PROMPT>", Duration::from_secs(5));
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();

    let recorded = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(seen.contains("OMP-PROMPT>"), "no omp prompt in {seen:?}");
    assert!(recorded.contains("print primary"), "args: {recorded:?}");
    assert!(recorded.contains("--shell=universal"), "args: {recorded:?}");
    assert!(recorded.contains("--escape=false"), "args: {recorded:?}");
    assert!(
        recorded.contains("--config /tmp/demo.omp.json"),
        "args: {recorded:?}"
    );

    // Fallback: when `oh-my-posh` fails (exits 1), sharky falls back to its native prompt.
    std::fs::write(&fake, format!("#!{}\nexit 1\n", sh.display())).unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let pty2 = openpty(None, None).expect("pty");
    let stdin2 = unsafe { Stdio::from_raw_fd(pty2.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout2 = unsafe { Stdio::from_raw_fd(pty2.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr2 = unsafe { Stdio::from_raw_fd(pty2.slave.try_clone().unwrap().into_raw_fd()) };
    let mut child2 = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin2)
        .stdout(stdout2)
        .stderr(stderr2)
        .env("SHARKY_CONFIG", &config)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .spawn()
        .expect("spawn sharky fallback");

    let fd2 = pty2.master.as_raw_fd();
    let seen2 = read_until(fd2, "❯", Duration::from_secs(5));
    let mut writer2 =
        unsafe { std::fs::File::from_raw_fd(pty2.master.try_clone().unwrap().into_raw_fd()) };
    let _ = writer2.write_all(b"exit\r");
    let _ = child2.wait();
    std::fs::remove_dir_all(&dir).ok();
    assert!(seen2.contains('❯'), "fallback native prompt missing: {seen2:?}");
}

#[test]
fn neolambda_preset_renders_two_lines_and_redraws_cleanly() {
    let dir = std::env::temp_dir().join(format!("sharky-neolambda-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    let work = dir.join("work");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(config.join("config.sharky"), "tide greeting off\ntide autosuggest off\n")
        .unwrap();
    std::fs::write(config.join("theme.sharky"), "wave preset neolambda\n").unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .current_dir(&work)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", "/home/julia")
        .env("USER", "julia")
        .env("HOSTNAME", "jiggle-station")
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    let mut seen = read_until(fd, "λ ", Duration::from_secs(5));
    for expected in ["a", "ab", "abc"] {
        let last = expected.chars().last().unwrap().to_string();
        let _ = writer.write_all(last.as_bytes());
        seen.push_str(&read_until(fd, expected, Duration::from_secs(5)));
    }
    let _ = writer.write_all(b"\r");
    std::thread::sleep(Duration::from_millis(80));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    let plain = strip_ansi_csi(&seen);
    let expected_header = format!("╭─julia at jiggle-station in {}", work.display());
    assert!(plain.contains(&expected_header), "plain output: {plain:?}");
    assert!(plain.contains("╰─λ "), "plain output: {plain:?}");
    assert_eq!(
        seen.matches("╭─").count(),
        1,
        "line 1 redrawn more than once: {seen:?}"
    );
}

#[test]
fn right_prompt_renders_and_hides_when_input_grows() {
    let dir = std::env::temp_dir().join(format!("sharky-rprompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("config.sharky"),
        "tide greeting off\ntide autosuggest off\nwave prompt \"> \"\nwave rprompt \"RPROMPT\"\n",
    )
    .unwrap();

    let size = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 40,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&size, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    let initial = read_until(fd, "RPROMPT", Duration::from_secs(5));
    assert!(initial.contains("RPROMPT"), "initial rprompt missing: {initial:?}");

    // 40 cols total, prompt `> ` = 2 cols, `RPROMPT` = 7 cols + 2 col margin.
    // Sending 31 chars reaches col 33 (> 31), then 32nd char `'Z'` redraws without `RPROMPT`.
    let fill = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let _ = writer.write_all(fill.as_bytes());
    let _ = read_until(fd, fill, Duration::from_secs(5));
    let _ = writer.write_all(b"Z");
    let overflow_chunk = read_until(fd, "Z", Duration::from_secs(5));
    assert!(overflow_chunk.contains('Z'), "overflow redraw missing: {overflow_chunk:?}");
    assert!(
        !overflow_chunk.contains("RPROMPT"),
        "rprompt should hide when input grows past margin: {overflow_chunk:?}"
    );

    // Deleting 4 chars drops back to col 30 (`30 + 2 + 7 = 39 <= 40`), so `RPROMPT` returns.
    let _ = writer.write_all(b"\x7f\x7f\x7f\x7f");
    let restored = read_until(fd, "RPROMPT", Duration::from_secs(5));
    assert!(
        restored.contains("RPROMPT"),
        "rprompt should return after backspace frees room: {restored:?}"
    );

    let _ = writer.write_all(b"\x03exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn declarative_two_line_fill_prompt_aligns_to_terminal_width() {
    let dir = std::env::temp_dir().join(format!("sharky-decl-fill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    let home = dir.join("home");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(config.join("config.sharky"), "tide greeting off\ntide autosuggest off\n")
        .unwrap();
    std::fs::write(
        config.join("theme.sharky"),
        r#"
coral blue #5BCFFA
coral pink #F5ABB9
coral ink  #1D151D
wave prompt "{@blue:\u256D\u2500}{@ink on @blue: {os_icon} }{@pink reverse:\uE0B0}{@ink on @pink: {cwd_home} }{fill}{@ink on @blue: {shell} {mem} }\n{@blue:\u2570\u2500}{symbol} "
wave prompt.os.glyph "LNX"
wave prompt.symbol.glyph ">>"
wave prompt.symbol "@blue bold"
"#,
    )
    .unwrap();

    let size = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&size, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .current_dir(&home)
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", &home)
        .spawn()
        .expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    let mut seen = read_until(fd, ">> ", Duration::from_secs(5));
    for expected in ["a", "ab", "abc"] {
        let last = expected.chars().last().unwrap().to_string();
        let _ = writer.write_all(last.as_bytes());
        seen.push_str(&read_until(fd, expected, Duration::from_secs(5)));
    }
    let _ = writer.write_all(b"\r");
    std::thread::sleep(Duration::from_millis(80));
    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();

    let plain = strip_ansi_csi(&seen);
    assert!(plain.contains("╭─"), "missing line 1 frame: {plain:?}");
    assert!(plain.contains("LNX"), "missing custom os glyph: {plain:?}");
    assert!(plain.contains("sharky"), "missing shell segment: {plain:?}");
    assert!(plain.contains("╰─>> "), "missing line 2 prompt: {plain:?}");
    assert!(seen.contains("\x1b[7;38;2;245;171;185m\u{E0B0}\x1b[0m"), "{seen:?}");
    assert_eq!(seen.matches("╭─").count(), 1, "line 1 redrawn: {seen:?}");
}

#[test]
fn prompt_fill_redraws_on_sigwinch_and_drops_right_when_narrow() {
    use std::os::unix::process::CommandExt;

    let dir = std::env::temp_dir().join(format!("sharky-winch-fill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let config = dir.join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.sharky"), "tide greeting off\n").unwrap();
    std::fs::write(
        config.join("theme.sharky"),
        "wave prompt \"LEFT{fill}RIGHT\\n> \"\n",
    )
    .unwrap();

    let size = nix::pty::Winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = openpty(&size, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
    command
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", &config)
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp");
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn sharky");

    let fd = pty.master.as_raw_fd();
    let mut writer =
        unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };

    let initial = read_until(fd, "> ", Duration::from_secs(5));
    let expected_100 = format!("LEFT{}RIGHT", " ".repeat(91));
    assert!(
        initial.contains(&expected_100),
        "initial 100-col fill missing: {initial:?}"
    );

    let _ = writer.write_all(b"echo ok");
    let _ = read_until(fd, "echo ok", Duration::from_secs(5));

    // Shrink 100 -> 60: wrapped top row reflowed into 2 rows, so redraw must move up 2 rows.
    set_terminal_width(fd, 60);
    let expected_60 = format!("LEFT{}RIGHT", " ".repeat(51));
    let resized_60 = read_until(fd, "echo ok", Duration::from_secs(5));
    assert!(
        resized_60.contains(&expected_60),
        "60-col fill missing after SIGWINCH: {resized_60:?}"
    );
    assert!(
        resized_60.contains("\x1b[2A\r\x1b[J"),
        "expected move up 2 reflowed rows before redraw: {resized_60:?}"
    );

    // Shrink 60 -> 8: narrower than LEFT(4) + 1 + RIGHT(5) = 10 -> omit RIGHT cleanly.
    set_terminal_width(fd, 8);
    let resized_8 = read_until(fd, "echo ok", Duration::from_secs(5));
    let plain_8 = strip_ansi_csi(&resized_8);
    assert!(
        plain_8.contains("LEFT\r\n> "),
        "narrow 8-col prompt should omit RIGHT cleanly: {plain_8:?}"
    );
    assert!(
        !resized_8.contains("RIGHT"),
        "narrow 8-col prompt should not print RIGHT: {resized_8:?}"
    );

    // Grow 8 -> 40: RIGHT should come back with 31 fill spaces, and typed buffer survives.
    set_terminal_width(fd, 40);
    let expected_40 = format!("LEFT{}RIGHT", " ".repeat(31));
    let resized_40 = read_until(fd, "echo ok", Duration::from_secs(5));
    assert!(
        resized_40.contains(&expected_40),
        "40-col fill did not restore RIGHT: {resized_40:?}"
    );

    let _ = writer.write_all(b"\r");
    let ran = read_until(fd, "ok", Duration::from_secs(5));
    assert!(ran.contains("ok"), "command buffer did not execute intact: {ran:?}");

    let _ = writer.write_all(b"exit\r");
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}
