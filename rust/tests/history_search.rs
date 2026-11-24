//! H5: the search surface of the `history` builtin.
//!
//! The builtin only exists in an interactive shell, so every test drives a real
//! session over a PTY. Command output is redirected to a file instead of being
//! scraped from the terminal, because these tests assert exact bytes: entry
//! order, timestamp lines and NUL separators.
//!
//! Each test seeds the history file directly, so fixtures stay independent of
//! what the probe commands record themselves. Probe commands split their query
//! across quotes (`"al""p"`) so the recorded probe line never matches the query
//! it searched for.
//!
//! The file-level invariants of `history delete --exact` (legacy bytes, torn
//! tails, undecodable lines) are checked against the module directly as well, so
//! a failure points at the rewrite instead of the terminal.

#[path = "../src/history.rs"]
#[allow(dead_code)] // the module is exercised through a subset of its surface
mod history;

use history::History;

use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

const NO_CONFIG: &str = "/nonexistent/sharky-history-search";

/// The shell prints this after every prompt, so it marks the end of a command.
const MARK_INPUT: &str = "\x1b]133;B\x1b\\";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-history-search-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// One v2 record with no cwd and no paths.
fn v2(command: &str, when: i64, hits: u64) -> String {
    format!("h2\t{when}\t\t0\t{command}\t{hits}")
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
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };
            if count > 0 {
                collected.push_str(&String::from_utf8_lossy(&buffer[..count as usize]));
                if collected.contains(needle) {
                    return collected;
                }
            }
        }
    }

    panic!("timed out waiting for {needle:?}; saw {collected:?}");
}

struct Session {
    child: Child,
    master: OwnedFd,
    writer: std::fs::File,
    dir: PathBuf,
    history: PathBuf,
}

fn session(name: &str, seed: &[String]) -> Session {
    let dir = scratch(name);
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let history = dir.join("history");
    let mut text = String::new();
    for line in seed {
        text.push_str(line);
        text.push('\n');
    }
    std::fs::write(&history, text).unwrap();

    let pty = openpty(None, None).expect("pty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("SHARKY_HISTORY", &history)
        .env("HOME", &home)
        .env("XDG_DATA_HOME", &dir)
        .env("TERM", "xterm-256color")
        .spawn()
        .expect("spawn sharky");

    let writer = unsafe { std::fs::File::from_raw_fd(pty.master.try_clone().unwrap().into_raw_fd()) };
    read_until(pty.master.as_raw_fd(), MARK_INPUT, Duration::from_secs(10));

    Session {
        child,
        master: pty.master,
        writer,
        dir,
        history,
    }
}

impl Session {
    fn send(&mut self, line: &str) {
        let _ = self.writer.write_all(format!("{line}\r").as_bytes());
    }

    fn wait_prompt(&mut self) {
        read_until(self.master.as_raw_fd(), MARK_INPUT, Duration::from_secs(10));
    }

    fn run(&mut self, line: &str) {
        self.send(line);
        self.wait_prompt();
    }

    /// Runs `command`, captures stdout into a fresh file and returns its bytes.
    fn capture(&mut self, name: &str, command: &str) -> Vec<u8> {
        let path = self.dir.join(name);
        self.run(&format!("{command} > {}", path.display()));
        std::fs::read(&path).unwrap()
    }

    fn finish(mut self) {
        self.send("exit");
        let _ = self.child.wait();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// On-disk records, split on the record separator only.
fn records(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut records: Vec<String> = text.split('\n').map(str::to_string).collect();
    if records.last().is_some_and(String::is_empty) {
        records.pop();
    }
    records
}

#[test]
fn history_prefix_filters_newest_first() {
    let mut session = session(
        "prefix",
        &[
            v2("alpha one", 1_700_000_000_000_000, 1),
            v2("beta two", 1_700_000_060_000_000, 1),
            v2("alphabet soup", 1_700_000_120_000_000, 1),
        ],
    );

    let out = session.capture("out", r#"history --prefix "al""p""#);
    assert_eq!(text(&out), "alphabet soup\nalpha one\n");
    session.finish();
}

#[test]
fn history_show_time_prints_a_timestamp() {
    let mut session = session(
        "show-time",
        &[
            v2("alpha one", 1_700_000_000_000_000, 1),
            v2("gamma ALPHA three", 1_700_000_060_000_000, 1),
        ],
    );

    let out = session.capture("out", "history --show-time");
    assert_eq!(
        text(&out),
        "# 2023-11-14 22:14:20\ngamma ALPHA three\n\
         # 2023-11-14 22:13:20\nalpha one\n"
    );
    session.finish();
}

#[test]
fn history_show_time_leaves_legacy_records_untimed() {
    let mut session = session(
        "legacy-time",
        &["old legacy".to_string(), v2("new timed", 1_700_000_000_000_000, 1)],
    );

    let out = session.capture("out", "history --show-time");
    assert_eq!(
        text(&out),
        "# 2023-11-14 22:13:20\nnew timed\nold legacy\n",
        "a legacy record must not invent a timestamp"
    );
    session.finish();
}

#[test]
fn history_delete_rewrites_only_the_file() {
    // The extra trailing field on `keep two` stands in for a field a newer writer
    // added: this reader must preserve a line it does not fully understand.
    let keep = format!("{}\tfuture-field", v2("keep two", 1_700_000_120_000_000, 3));
    let mut session = session(
        "delete-file",
        &[
            "legacy one".to_string(),
            v2("remove me", 1_700_000_060_000_000, 7),
            keep.clone(),
        ],
    );
    let before = std::fs::metadata(&session.history).unwrap();

    session.run(r#"history delete --exact "remove me""#);

    let after = std::fs::metadata(&session.history).unwrap();
    assert_eq!(
        after.mode() & 0o777,
        0o600,
        "the rewritten file must stay private"
    );
    assert_ne!(
        (before.dev(), before.ino()),
        (after.dev(), after.ino()),
        "delete must rewrite the file, not truncate it in place"
    );
    let remainder: Vec<String> = records(&session.history)
        .into_iter()
        .filter(|line| !line.contains("history delete"))
        .collect();
    assert_eq!(
        remainder,
        vec!["legacy one".to_string(), keep],
        "legacy bytes, the v2 hits field and unknown fields must survive verbatim"
    );
    let leftover: Vec<_> = std::fs::read_dir(&session.dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(leftover.is_empty(), "temporary file left behind: {leftover:?}");

    let out = session.capture("out", r#"history --exact "remove me""#);
    assert!(out.is_empty(), "memory still lists a deleted entry: {out:?}");
    session.finish();
}

#[test]
fn history_delete_removes_only_exact_matches() {
    let mut session = session(
        "delete-exact",
        &[
            v2("remove me", 1_700_000_000_000_000, 1),
            v2("remove me too", 1_700_000_060_000_000, 1),
            v2("keep three", 1_700_000_120_000_000, 1),
        ],
    );

    session.run(r#"history delete --exact "remove me""#);

    let commands: Vec<String> = records(&session.history)
        .into_iter()
        .filter(|line| !line.contains("history delete"))
        .map(|line| sharky_decode(&line))
        .collect();
    assert_eq!(commands, ["remove me too", "keep three"]);
    session.finish();
}

/// The command a fixture record decodes to. Fixtures contain no escapes, so this
/// only has to pick the command field of a v2 line.
fn sharky_decode(line: &str) -> String {
    if !line.starts_with("h2\t") {
        return line.to_string();
    }
    let fields: Vec<&str> = line.split('\t').collect();
    let npaths: usize = fields[3].parse().unwrap();
    fields[4 + npaths].to_string()
}

#[test]
fn history_delete_requires_exact() {
    let mut session = session(
        "delete-mode",
        &[v2("keep me", 1_700_000_000_000_000, 1)],
    );

    session.run("history delete keep");
    assert_eq!(
        records(&session.history).len(),
        2,
        "a delete without --exact must not touch the history"
    );
    session.finish();
}

#[test]
fn history_search_subcommand_searches_instead_of_deleting() {
    let mut session = session(
        "search-subcommand",
        &[
            v2("delete this", 1_700_000_000_000_000, 1),
            v2("keep this", 1_700_000_060_000_000, 1),
        ],
    );

    // After `search`, the word `delete` is a pattern, not the delete subcommand.
    let out = session.capture("out", r#"history search "dele""te this""#);
    assert_eq!(text(&out), "delete this\n");
    assert_eq!(
        records(&session.history).len(),
        3,
        "a search must not delete anything"
    );
    session.finish();
}

#[test]
fn history_default_contains_is_case_insensitive() {
    let mut session = session(
        "contains",
        &[
            v2("alpha one", 1_700_000_000_000_000, 1),
            v2("gamma ALPHA three", 1_700_000_060_000_000, 1),
            v2("unrelated", 1_700_000_120_000_000, 1),
        ],
    );

    let out = session.capture("out", r#"history "AL""PHA""#);
    assert_eq!(text(&out), "gamma ALPHA three\nalpha one\n");
    session.finish();
}

#[test]
fn history_exact_and_case_sensitive_narrow_the_match() {
    let mut session = session(
        "exact-cs",
        &[
            v2("Task one", 1_700_000_000_000_000, 1),
            v2("task one", 1_700_000_060_000_000, 1),
        ],
    );

    let out = session.capture(
        "insensitive",
        r#"history --exact "ta""sk one""#,
    );
    assert_eq!(text(&out), "task one\nTask one\n");

    let out = session.capture(
        "sensitive",
        r#"history --exact --case-sensitive "ta""sk one""#,
    );
    assert_eq!(text(&out), "task one\n");
    session.finish();
}

#[test]
fn history_max_zero_returns_nothing() {
    let mut session = session(
        "max-zero",
        &[
            v2("one", 1_700_000_000_000_000, 1),
            v2("two", 1_700_000_060_000_000, 1),
        ],
    );

    let out = session.capture("one", "history --max 1");
    assert_eq!(text(&out), "two\n");

    let out = session.capture("out", "history --max 0");
    assert!(out.is_empty(), "--max 0 must print nothing, got {out:?}");
    session.finish();
}

#[test]
fn history_reverse_prints_oldest_first() {
    let mut session = session(
        "reverse",
        &[
            v2("one", 1_700_000_000_000_000, 1),
            v2("two", 1_700_000_060_000_000, 1),
            v2("three", 1_700_000_120_000_000, 1),
        ],
    );

    let out = session.capture("out", "history --reverse");
    assert_eq!(text(&out), "one\ntwo\nthree\n");
    session.finish();
}

#[test]
fn history_null_separates_entries() {
    let mut session = session(
        "null",
        &[
            v2("one", 1_700_000_000_000_000, 1),
            v2("two", 1_700_000_060_000_000, 1),
        ],
    );

    let out = session.capture("out", "history --null");
    assert_eq!(out, b"two\0one\0");
    session.finish();
}

#[test]
fn delete_keeps_a_sibling_append_made_while_it_waited() {
    let mut session = session(
        "delete-concurrent",
        &[
            v2("keep me", 1_700_000_000_000_000, 1),
            v2("remove me", 1_700_000_060_000_000, 1),
        ],
    );

    // Holding the directory lock proves delete reads the file only after it gets
    // the same lock: an unlocked read-modify-write would rewrite while we hold it.
    let lock_file = std::fs::File::open(&session.dir).unwrap();
    let lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, err)| err)
        .expect("lock history directory");

    session.send(r#"history delete --exact "remove me""#);
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        records(&session.history)
            .iter()
            .any(|line| line.contains("remove me")),
        "delete rewrote the file without the directory lock"
    );

    let mut sibling = std::fs::read_to_string(&session.history).unwrap();
    sibling.push_str(&v2("sibling entry", 1_700_000_180_000_000, 1));
    sibling.push('\n');
    std::fs::write(&session.history, sibling).unwrap();
    drop(lock);

    session.wait_prompt();

    let commands: Vec<String> = records(&session.history)
        .into_iter()
        .filter(|line| !line.contains("history delete"))
        .map(|line| sharky_decode(&line))
        .collect();
    assert_eq!(commands, ["keep me", "sibling entry"]);
    session.finish();
}

fn file_scratch(name: &str) -> PathBuf {
    let dir = scratch(name);
    dir.join("history")
}

#[test]
fn records_expose_when_and_origin() {
    let path = file_scratch("records");
    std::fs::write(
        &path,
        format!("legacy here\n{}\n", v2("timed here", 1_700_000_000_000_000, 4)),
    )
    .unwrap();
    let mut history = History::try_load(path, 100).unwrap();

    let records = history.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].command, "legacy here");
    assert_eq!(
        records[0].when, None,
        "a legacy record has no timestamp to show"
    );
    assert_eq!(records[1].command, "timed here");
    assert_eq!(records[1].when, Some(1_700_000_000_000_000));

    history.try_add("typed here").unwrap();
    let records = history.records();
    assert_eq!(records.last().unwrap().command, "typed here");
    assert!(
        records.last().unwrap().when.is_some(),
        "a command this session ran is timed"
    );
}

#[test]
fn delete_returns_zero_and_leaves_the_file_alone_when_nothing_matches() {
    let path = file_scratch("delete-miss");
    std::fs::write(&path, "keep\nremove\ntorn tail").unwrap();
    let before = std::fs::metadata(&path).unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();

    let removed = history.try_delete_exact("absent").unwrap();

    assert_eq!(removed, 0);
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!((before.ino(), before.len()), (after.ino(), after.len()));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"keep\nremove\ntorn tail",
        "a miss must not rewrite the torn tail away"
    );
}

#[test]
fn delete_drops_the_torn_tail_and_keeps_complete_lines() {
    let path = file_scratch("delete-torn");
    std::fs::write(&path, "keep\nremove\ntorn").unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();

    assert_eq!(history.try_delete_exact("remove").unwrap(), 1);

    assert_eq!(std::fs::read(&path).unwrap(), b"keep\n");
    assert_eq!(history.entries(), ["keep"]);
}

#[test]
fn delete_keeps_undecodable_lines_and_removes_every_duplicate() {
    let path = file_scratch("delete-bytes");
    std::fs::write(&path, "keep\nremove\n").unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();
    // An undecodable line must survive a rewrite untouched; two legacy records of
    // the same command are both removed, because the file has no dedup.
    std::fs::write(&path, b"keep\n\xff\nremove\nremove\n").unwrap();

    assert_eq!(history.try_delete_exact("remove").unwrap(), 2);

    assert_eq!(std::fs::read(&path).unwrap(), b"keep\n\xff\n");
}

#[test]
fn private_delete_removes_from_memory() {
    let mut history = History::private(100);
    history.try_add("one").unwrap();
    history.try_add("two").unwrap();

    assert_eq!(history.try_delete_exact("one").unwrap(), 0);

    assert_eq!(history.entries(), ["two"]);
}
