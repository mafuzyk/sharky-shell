#[path = "../src/history.rs"]
#[allow(dead_code)] // this target exercises a subset of the module
mod history;

#[path = "../src/histexpand.rs"]
#[allow(dead_code)] // this target exercises the expansion guard only
mod histexpand;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use history::History;

fn scratch(name: &str) -> PathBuf {
    let dir =
        Path::new("/tmp/opencode").join(format!("sharky-history-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// Splits on the record separator only, so a `\r` before the newline stays part of
/// the record exactly as `parse` sees it.
fn records(path: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut records: Vec<String> = text.split('\n').map(str::to_string).collect();
    if records.last().is_some_and(String::is_empty) {
        records.pop();
    }
    records
}

/// The command each on-disk record decodes to, in file order.
fn commands(path: &Path) -> Vec<String> {
    records(path)
        .iter()
        .map(|line| history::decode(line).command)
        .collect()
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// Permission failures are not reachable for root, so those fixtures skip there.
fn running_as_root() -> bool {
    std::fs::metadata(".")
        .map(|metadata| metadata.uid() == 0)
        .unwrap_or(false)
}

#[test]
fn appended_command_is_durable_without_flush() {
    let path = scratch("durable").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();

    history.try_add("ls -la").unwrap();

    assert_eq!(commands(&path), ["ls -la"]);
    assert_eq!(records(&path).len(), 1);
}

#[test]
fn history_file_is_private_at_creation() {
    let path = scratch("mode").join("nested/history");
    let mut history = History::try_load(path.clone(), 100).unwrap();

    history.try_add("secret command").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn escape_roundtrips_newlines_and_backslashes() {
    let path = scratch("escape").join("history");
    let command = "echo a\nb\\c";
    let mut history = History::try_load(path.clone(), 100).unwrap();

    history.try_add(command).unwrap();

    assert_eq!(records(&path).len(), 1, "a record never spans two lines");
    assert_eq!(commands(&path), [command]);
    assert_eq!(History::try_load(path, 100).unwrap().entries(), [command]);
}

#[test]
fn independent_instances_both_append_and_survive() {
    let path = scratch("two-writers").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();
    // Production starts each session at a random vacuum offset; pin it so no
    // compaction rewrites the file mid-test and reorders these records.
    a.set_vacuum_offset(0);
    b.set_vacuum_offset(0);

    a.try_add("from a").unwrap();
    b.try_add("from b").unwrap();

    assert_eq!(commands(&path), ["from a", "from b"]);
    assert_eq!(
        History::try_load(path, 100).unwrap().entries(),
        ["from a", "from b"]
    );
}

#[test]
fn readers_ignore_an_incomplete_tail() {
    let path = scratch("torn-tail").join("history");
    std::fs::write(&path, "first\nsecond\npar").unwrap();

    let history = History::try_load(path, 100).unwrap();

    assert_eq!(history.entries(), ["first", "second"]);
}

#[test]
fn readers_survive_a_partial_utf8_tail() {
    let path = scratch("torn-utf8").join("history");
    std::fs::write(&path, b"first\n\xe2\x82").unwrap();

    let history = History::try_load(path, 100).unwrap();

    assert_eq!(history.entries(), ["first"]);
}

#[test]
fn append_discards_an_incomplete_tail() {
    let path = scratch("torn-append").join("history");
    std::fs::write(&path, "first\ntor").unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();
    assert_eq!(history.entries(), ["first"]);

    history.try_add("second").unwrap();

    assert_eq!(commands(&path), ["first", "second"]);
    assert_eq!(
        History::try_load(path, 100).unwrap().entries(),
        ["first", "second"]
    );
}

#[test]
fn legacy_newline_terminated_records_still_load() {
    let path = scratch("legacy").join("history");
    std::fs::write(&path, "echo a\\nb\ncafé\n\nlast\n").unwrap();

    let history = History::try_load(path, 100).unwrap();

    assert_eq!(history.entries(), ["echo a\nb", "café", "last"]);
}

#[test]
fn a_session_sees_a_foreign_command_without_merge() {
    let path = scratch("refresh").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();
    assert!(b.entries().is_empty());

    a.try_add("from sibling").unwrap();
    assert!(b.entries().is_empty(), "reading needs a refresh");

    b.refresh().unwrap();

    assert_eq!(b.entries(), ["from sibling"]);
    assert_eq!(
        b.expansion_entries(),
        Vec::<String>::new(),
        "a sibling command is never expandable"
    );
}

#[test]
fn refresh_keeps_local_failed_entries() {
    if running_as_root() {
        return;
    }
    let dir = scratch("refresh-local");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("kept").unwrap();

    chmod(&path, 0o400);
    assert!(history.try_add("local only").is_err());
    chmod(&path, 0o600);

    let mut other = History::try_load(path, 100).unwrap();
    other.try_add("later").unwrap();

    history.refresh().unwrap();

    assert_eq!(history.entries(), ["kept", "later", "local only"]);
    assert_eq!(
        history.expansion_entries(),
        ["kept", "local only"],
        "a failed append is still this session's own command"
    );
}

#[test]
fn refresh_keeps_the_order_of_local_entries() {
    let path = scratch("refresh-order").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();

    b.try_add("mine one").unwrap();
    b.try_add("mine two").unwrap();
    a.try_add("sibling").unwrap();

    for _ in 0..3 {
        b.refresh().unwrap();
        assert_eq!(b.entries(), ["mine one", "mine two", "sibling"]);
    }
}

#[test]
fn refresh_drops_the_entries_a_clear_removed_and_keeps_local_ones() {
    let path = scratch("refresh-clear").join("history");
    std::fs::write(&path, "loaded\n").unwrap();
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();
    b.try_add("mine").unwrap();
    assert_eq!(b.entries(), ["loaded", "mine"]);

    a.try_clear().unwrap();
    b.refresh().unwrap();

    assert_eq!(b.entries(), ["mine"], "a cleared file hides foreign entries");
}

#[test]
fn expansion_ignores_refreshed_entries() {
    let path = scratch("refresh-expansion").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();

    a.try_add("zsh foreign").unwrap();
    a.try_add("zsh newest").unwrap();
    b.refresh().unwrap();

    // Everything is visible to the editor, the listing and the picker...
    assert_eq!(b.entries(), ["zsh foreign", "zsh newest"]);
    // ...but nothing a sibling ran may answer an event.
    assert!(b.expansion_entries().is_empty(), "refreshed entry is expandable");
    assert_eq!(
        histexpand::expand("!!", &b.expansion_entries()),
        Err("!!: event not found".to_string())
    );

    b.try_add("printf own").unwrap();

    assert_eq!(b.expansion_entries(), ["printf own"]);
    assert_eq!(
        histexpand::expand("!!", &b.expansion_entries()),
        Ok("printf own".to_string())
    );
    assert_eq!(
        histexpand::expand("!1", &b.expansion_entries()),
        Ok("printf own".to_string()),
        "!n numbers the expansion list"
    );
    assert_eq!(
        histexpand::expand("!zsh", &b.expansion_entries()),
        Err("!zsh: event not found".to_string()),
        "a prefix may not reach a refreshed entry"
    );
    assert_eq!(
        histexpand::expand("!?newest?", &b.expansion_entries()),
        Err("!?newest?: event not found".to_string()),
        "a needle may not reach a refreshed entry"
    );
    // The full view holds three entries, so a count that reached it would resolve
    // here; the expansion list holds one.
    assert_eq!(
        histexpand::expand("!-2", &b.expansion_entries()),
        Err("!-2: event not found".to_string()),
        "counting back stays inside the expansion list"
    );
}

#[test]
fn refresh_reads_nothing_when_the_file_is_unchanged() {
    if running_as_root() {
        return;
    }
    let dir = scratch("refresh-noop");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("one").unwrap();

    // The file stops being readable without moving, so any read or lock would
    // fail. An unchanged fingerprint has to return before touching it.
    chmod(&path, 0o000);
    let result = history.refresh();
    chmod(&path, 0o600);

    assert_eq!(result, Ok(()), "refresh read an unchanged file");
    assert_eq!(history.entries(), ["one"]);
}

#[test]
fn refresh_in_private_mode_keeps_memory_and_touches_nothing() {
    let dir = scratch("refresh-private");
    let mut history = History::private(100);
    history.try_add("mine").unwrap();

    assert_eq!(history.refresh(), Ok(()));
    assert_eq!(history.entries(), ["mine"]);
    assert_eq!(
        history.merge(),
        Err("can't merge history in private mode".to_string())
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // An empty path is the same private mode, and also returns before any syscall.
    let mut empty_path = History::try_load(PathBuf::new(), 100).unwrap();
    empty_path.try_add("mine").unwrap();
    assert_eq!(empty_path.refresh(), Ok(()));
    assert_eq!(empty_path.entries(), ["mine"]);
}

#[test]
fn merge_is_idempotent_and_forced() {
    let path = scratch("merge-idempotent").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();
    a.try_add("first").unwrap();
    a.try_add("second").unwrap();
    b.try_add("mine").unwrap();

    b.merge().unwrap();
    assert_eq!(b.entries(), ["first", "second", "mine"]);

    b.merge().unwrap();
    assert_eq!(b.entries(), ["first", "second", "mine"]);
    assert_eq!(
        b.expansion_entries(),
        ["mine"],
        "merging twice does not promote a sibling command"
    );
}

#[test]
fn refresh_after_a_failed_read_keeps_the_view_and_retries_on_a_change() {
    let dir = scratch("refresh-failed");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("kept").unwrap();

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    assert!(history.refresh().is_err(), "a directory is not history");
    assert_eq!(history.entries(), ["kept"], "a failed refresh drops nothing");

    // A second prompt sees the same broken path: the fingerprint was taken, so
    // the failure is reported once instead of on every prompt.
    assert_eq!(history.refresh(), Ok(()));

    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, "fresh\n").unwrap();
    history.refresh().unwrap();
    // Disk order first, then the entry only memory has, exactly as merge orders them.
    assert_eq!(history.entries(), ["fresh", "kept"]);
}

#[test]
fn a_deleted_history_file_keeps_local_entries_only() {
    let path = scratch("refresh-deleted").join("history");
    std::fs::write(&path, "loaded\n").unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("mine").unwrap();

    std::fs::remove_file(&path).unwrap();
    history.refresh().unwrap();

    assert_eq!(history.entries(), ["mine"]);
    assert_eq!(history.expansion_entries(), ["mine"]);
}

#[test]
fn a_local_repeat_of_a_refreshed_command_becomes_expandable() {
    let path = scratch("refresh-repeat").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();
    a.try_add("make test").unwrap();
    b.refresh().unwrap();
    assert!(b.expansion_entries().is_empty());

    // Typing here what the sibling just ran makes it this session's own command,
    // even though the view already ends with it and nothing new is written.
    b.try_add("make test").unwrap();

    assert_eq!(b.entries(), ["make test"]);
    assert_eq!(b.expansion_entries(), ["make test"]);
    assert_eq!(records(&path).len(), 1, "a duplicate still writes nothing");
}

#[test]
fn refresh_needs_the_shared_view_to_survive_a_vacuum() {
    let path = scratch("refresh-vacuum").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    // Pin the trigger so the compaction lands on the 25th append.
    history.set_vacuum_offset(0);
    for index in 0..25 {
        history.try_add(&format!("command {index}")).unwrap();
    }

    history.refresh().unwrap();

    assert_eq!(history.entries().len(), 25, "a vacuum is not a new command");
}

#[test]
fn stale_view_waits_for_merge_and_then_follows_disk_order() {
    let path = scratch("merge").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();

    a.try_add("first").unwrap();
    assert!(b.entries().is_empty());

    a.try_add("second").unwrap();
    b.try_add("mine").unwrap();
    assert_eq!(b.entries(), ["mine"]);

    b.merge().unwrap();

    assert_eq!(b.entries(), ["first", "second", "mine"]);
}

#[test]
fn merge_keeps_local_only_entries_after_the_disk_entries() {
    if running_as_root() {
        return;
    }
    let dir = scratch("merge-local");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("kept").unwrap();

    chmod(&path, 0o400);
    assert!(history.try_add("local only").is_err());
    chmod(&path, 0o600);

    let mut other = History::try_load(path, 100).unwrap();
    other.try_add("later").unwrap();

    history.merge().unwrap();

    assert_eq!(history.entries(), ["kept", "later", "local only"]);
}

#[test]
fn merge_reports_a_refused_path() {
    let path = scratch("merge-refused").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("kept").unwrap();

    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();

    assert!(history.merge().is_err());
    assert_eq!(history.entries(), ["kept"]);
}

#[test]
fn private_mode_keeps_no_file_and_refuses_merge() {
    let dir = scratch("private");
    let mut history = History::private(100);
    let mut empty_path = History::try_load(PathBuf::new(), 100).unwrap();

    history.try_add("in memory").unwrap();
    empty_path.try_add("in memory").unwrap();

    assert_eq!(history.entries(), ["in memory"]);
    assert_eq!(empty_path.entries(), ["in memory"]);
    assert_eq!(
        history.merge(),
        Err("can't merge history in private mode".to_string())
    );
    assert!(empty_path.merge().is_err());

    history.try_clear().unwrap();

    assert!(history.entries().is_empty());
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // Clearing in private mode must not leave an origin behind for the next entry.
    history.try_add("after clear").unwrap();
    assert_eq!(history.entries(), ["after clear"]);
    assert_eq!(history.expansion_entries(), ["after clear"]);
}

#[test]
fn append_into_a_file_parent_is_reported() {
    let parent = scratch("parent-file").join("blocked");
    let mut history = History::try_load(parent.join("history"), 100).unwrap();
    std::fs::write(&parent, "not a directory\n").unwrap();

    let error = history.try_add("command").unwrap_err();

    assert!(error.contains("blocked"), "unexpected error: {error}");
    assert_eq!(history.entries(), ["command"]);
}

#[test]
fn default_path_without_home_or_xdg_is_memory_only() {
    if std::env::var_os("SHARKY_HISTORY_CHILD").is_none() {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "default_path_without_home_or_xdg_is_memory_only"])
            .env("SHARKY_HISTORY_CHILD", "1")
            .env_remove("SHARKY_HISTORY")
            .env_remove("XDG_DATA_HOME")
            .env_remove("HOME")
            .status()
            .unwrap();
        assert!(status.success());
        return;
    }

    assert!(History::default_path().as_os_str().is_empty());
    let mut history = History::try_load(History::default_path(), 100).unwrap();
    assert!(history.merge().is_err());
}

#[test]
fn symlink_history_path_is_refused() {
    let dir = scratch("symlink");
    let victim = dir.join("victim");
    std::fs::write(&victim, "untouched\n").unwrap();
    let path = dir.join("history");
    std::os::unix::fs::symlink(&victim, &path).unwrap();

    let error = History::try_load(path, 100).err().unwrap();

    assert!(
        error.contains("not a regular file"),
        "unexpected error: {error}"
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched\n");
}

#[test]
fn symlink_swapped_in_after_load_is_refused_on_write() {
    let dir = scratch("symlink-swap");
    let victim = dir.join("victim");
    std::fs::write(&victim, "untouched\n").unwrap();
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("first").unwrap();

    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&victim, &path).unwrap();

    let error = history.try_add("second").unwrap_err();

    assert!(error.contains("history"), "unexpected error: {error}");
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched\n");
}

#[test]
fn directory_history_path_is_refused() {
    let path = scratch("directory-path").join("history");
    std::fs::create_dir(&path).unwrap();

    let history = History::try_load(path.clone(), 100);

    assert!(history.is_err());
    assert!(path.is_dir());
}

/// Runs the access off-thread so an open that blocks on a special file fails the
/// test instead of hanging the whole suite.
fn refusing_to_block<T: Send + 'static>(access: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(access());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("history access blocked on a special file")
}

#[test]
fn fifo_history_path_is_refused_without_blocking() {
    let path = scratch("fifo-path").join("history");
    nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

    let error = refusing_to_block(move || History::try_load(path, 100).err().unwrap());

    assert!(
        error.contains("not a regular file"),
        "unexpected error: {error}"
    );
}

#[test]
fn fifo_parent_is_refused_without_blocking() {
    let parent = scratch("fifo-parent").join("blocked");
    nix::unistd::mkfifo(&parent, nix::sys::stat::Mode::S_IRWXU).unwrap();
    let path = parent.join("history");

    let error = refusing_to_block(move || -> Option<String> {
        match History::try_load(path, 100) {
            Ok(mut history) => history.try_add("command").err(),
            Err(err) => Some(err),
        }
    });

    let error = error.unwrap();
    assert!(error.contains("blocked"), "unexpected error: {error}");
}

#[test]
fn load_failure_keeps_its_diagnostic_instead_of_looking_private() {
    let dir = scratch("try-load");
    let path = dir.join("history");
    std::fs::create_dir(&path).unwrap();

    let error = match History::try_load(path.clone(), 100) {
        Ok(_) => panic!("a directory must not load as history"),
        Err(error) => error,
    };

    assert!(
        error.contains("not a regular file"),
        "unexpected error: {error}"
    );
    assert!(error.contains("history"), "unexpected error: {error}");
    assert!(History::try_load(dir.join("absent"), 100).is_ok());
    assert!(History::try_load(PathBuf::new(), 100)
        .unwrap()
        .entries()
        .is_empty());
}

#[test]
fn append_failure_is_reported_and_keeps_the_memory_entry() {
    if running_as_root() {
        return;
    }
    let dir = scratch("append-failure");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.set_vacuum_offset(0);
    history.try_add("before").unwrap();

    chmod(&path, 0o400);
    let error = history.try_add("during").unwrap_err();
    chmod(&path, 0o600);

    assert!(error.contains("history"), "unexpected error: {error}");
    assert_eq!(history.entries(), ["before", "during"]);
    assert_eq!(commands(&path), ["before"]);
}

#[test]
fn clear_failure_is_reported_and_keeps_the_file() {
    if running_as_root() {
        return;
    }
    let dir = scratch("clear-failure");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("kept").unwrap();

    chmod(&path, 0o400);
    let error = history.try_clear().unwrap_err();
    chmod(&path, 0o600);

    assert!(error.contains("history"), "unexpected error: {error}");
    assert_eq!(history.entries(), ["kept"]);
    assert_eq!(commands(&path), ["kept"]);
}

#[test]
fn clear_truncates_the_file() {
    let path = scratch("clear").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();

    history.try_add("one").unwrap();
    history.try_add("two").unwrap();
    chmod(&path, 0o644);
    history.try_clear().unwrap();

    assert!(history.entries().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(History::try_load(path, 100).unwrap().entries().is_empty());
}

#[test]
fn vacuum_never_prunes_distinct_entries_to_the_memory_limit() {
    let path = scratch("vacuum-keep").join("history");
    let mut history = History::try_load(path.clone(), 3).unwrap();
    // Pin the trigger so the compaction lands on the 25th append.
    history.set_vacuum_offset(0);

    for index in 0..25 {
        history.try_add(&format!("command {index}")).unwrap();
    }

    assert_eq!(
        history.entries(),
        ["command 22", "command 23", "command 24"]
    );
    assert_eq!(lines(&path).len(), 25);
    assert_eq!(History::try_load(path, 100).unwrap().entries().len(), 25);
}

#[test]
fn vacuum_collapses_duplicates_across_sessions_to_the_newest_occurrence() {
    let path = scratch("vacuum-dedup").join("history");
    let mut other = History::try_load(path.clone(), 100).unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();
    // The production trigger starts at a random offset; pinning it makes the
    // compaction land on the last append instead of on `repeat` or before it.
    history.set_vacuum_offset(0);

    other.try_add("repeat").unwrap();
    // Kept apart so the second "repeat" is an older-command repeat, not a
    // consecutive one that would collapse on its own.
    history.try_add("between").unwrap();
    history.try_add("repeat").unwrap();
    for index in 0..22 {
        history.try_add(&format!("unique {index}")).unwrap();
    }
    history.try_add("last").unwrap();

    let file = commands(&path);
    assert_eq!(
        file.iter().filter(|command| *command == "repeat").count(),
        1
    );
    // The surviving record sits where the newest occurrence was, not where the
    // oldest one was.
    assert_eq!(file.first().unwrap(), "between");
    assert_eq!(file.get(1).unwrap(), "repeat");
    assert_eq!(file.last().unwrap(), "last");
    assert_eq!(file.len(), 25);
}

#[test]
fn concurrent_writers_lose_nothing_and_never_interleave() {
    let path = scratch("concurrent").join("history");
    let writers = 8;
    let per_writer = 10;
    let filler = "x".repeat(2000);
    let mut expected = Vec::new();
    for writer in 0..writers {
        for index in 0..per_writer {
            expected.push(format!("writer {writer} command {index} {filler}"));
        }
    }

    std::thread::scope(|scope| {
        for writer in 0..writers {
            let path = path.clone();
            let expected = &expected;
            scope.spawn(move || {
                let mut history = History::try_load(path, 1000).unwrap();
                for index in 0..per_writer {
                    history
                        .try_add(&expected[writer * per_writer + index])
                        .unwrap();
                }
            });
        }
    });

    let entries = History::try_load(path, 1000).unwrap().entries().to_vec();
    assert_eq!(entries.len(), writers * per_writer);
    for command in &expected {
        assert!(entries.contains(command), "lost {command:.40}");
    }
}

/// Builds a v2 line through the encoder, so the codec tests exercise both sides.
fn v2_line(command: &str, when: i64, cwd: Option<&str>, paths: &[&str]) -> String {
    let mut record = history::Record::new(command);
    record.when = Some(when);
    record.cwd = cwd.map(str::to_string);
    record.paths = paths.iter().map(|path| path.to_string()).collect();
    record.encode()
}

#[test]
fn v2_roundtrips_tabs_and_backslashes() {
    let command = "printf 'a\tb' \\ x\ny café";
    let cwd = "/tmp/a\tb \\ c\né";
    let mut record = history::Record::new(command);
    record.when = Some(1_700_000_000_000_000);
    record.cwd = Some(cwd.to_string());

    let line = record.encode();

    // With no paths the line has exactly the separators of its six fields.
    assert_eq!(line.matches('\t').count(), 5, "line: {line:?}");
    assert!(!line.contains('\n'), "one record per line: {line:?}");
    assert_eq!(line.split('\t').next(), Some("h2"));

    let decoded = history::decode(&line);

    assert!(!decoded.is_legacy());
    assert_eq!(decoded.command, command);
    assert_eq!(decoded.cwd.as_deref(), Some(cwd));
    assert_eq!(decoded.when, Some(1_700_000_000_000_000));
    assert!(decoded.paths.is_empty());
    assert_eq!(decoded.encode(), line, "re-encoding must be stable");
}

#[test]
fn legacy_line_stays_a_legacy_record() {
    let line = "echo a\\nb\\\\c";

    let record = history::decode(line);

    assert!(record.is_legacy());
    assert_eq!(record.command, "echo a\nb\\c");
    assert_eq!(record.when, None);
    assert_eq!(record.cwd, None);
    assert!(record.paths.is_empty());
    assert_eq!(record.original_line(), Some(line));
    assert_eq!(record.encode(), line, "legacy bytes are preserved verbatim");
}

#[test]
fn unknown_trailing_fields_are_ignored() {
    let no_paths = history::decode("h2\t17\t/tmp\t0\techo hi\tfuture\tmore");
    assert!(!no_paths.is_legacy());
    assert_eq!(no_paths.when, Some(17));
    assert_eq!(no_paths.cwd.as_deref(), Some("/tmp"));
    assert_eq!(no_paths.command, "echo hi");

    // The command index follows the declared path count, not a fixed position.
    let one_path = history::decode("h2\t18\t/src\t1\t/lib\techo hi\tjunk");
    assert!(!one_path.is_legacy());
    assert_eq!(one_path.paths, ["/lib"]);
    assert_eq!(one_path.command, "echo hi");
}

#[test]
fn mixed_legacy_and_v2_lines_load_in_file_order() {
    let path = scratch("mixed").join("history");
    // The v2 line carries the newest timestamp, so file order and time order differ.
    let file = format!(
        "oldest\n{}\nnewest\n",
        v2_line("middle", 9_000_000, Some("/work"), &["/work/a.rs"])
    );
    std::fs::write(&path, file).unwrap();

    let history = History::try_load(path, 100).unwrap();

    assert_eq!(history.entries(), ["oldest", "middle", "newest"]);
}

#[test]
fn v2_record_with_eight_paths_round_trips() {
    let cwd = "/home/mafu/src/café";
    let mut record = history::Record::new("make -j2");
    record.when = Some(42);
    record.cwd = Some(cwd.to_string());
    record.paths = (0..8).map(|index| format!("/w/p\t{index}")).collect();

    let line = record.encode();

    assert_eq!(line.matches('\t').count(), 13, "line: {line:?}");
    let decoded = history::decode(&line);
    assert!(!decoded.is_legacy());
    assert_eq!(decoded.paths, record.paths);
    assert_eq!(decoded.command, "make -j2");
    assert_eq!(decoded.cwd.as_deref(), Some(cwd));
}

#[test]
fn torn_v2_tail_is_ignored() {
    let path = scratch("torn-v2").join("history");
    let kept = v2_line("kept", 5, Some("/work"), &["/work/a"]);
    let torn = v2_line("torn", 6, Some("/work"), &["/work/a"]);
    std::fs::write(&path, format!("{kept}\n{}", &torn[..torn.len() - 4])).unwrap();

    let history = History::try_load(path, 100).unwrap();

    assert_eq!(history.entries(), ["kept"]);
}

#[test]
fn legacy_line_that_starts_with_h2_is_not_v2() {
    let commands = [
        "h2 not a header",
        "h2\tone",
        "h2\twhen\tcwd\tzero\techo",
        "h2\t1\t/tmp\tnope\techo",
    ];
    let path = scratch("h2-legacy").join("history");
    std::fs::write(&path, format!("{}\n", commands.join("\n"))).unwrap();

    for command in commands {
        let record = history::decode(command);
        assert!(record.is_legacy(), "decoded as v2: {command:?}");
        assert_eq!(record.command, command);
        assert_eq!(record.original_line(), Some(command));
    }
    assert_eq!(History::try_load(path, 100).unwrap().entries(), commands);
}

#[test]
fn repeat_of_an_old_command_moves_to_the_newest_position() {
    let path = scratch("recency").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    // A random offset of 22..24 would vacuum on one of these three appends and
    // collapse the repeated `ls`, so stored.len() would be 2 instead of 3.
    history.set_vacuum_offset(0);

    history.try_add("ls").unwrap();
    history.try_add("pwd").unwrap();
    history.try_add("ls").unwrap();

    // `!!` must resolve to the repeat, so the view ends with it and holds one copy.
    assert_eq!(history.entries(), ["pwd", "ls"]);
    let stored = records(&path);
    assert_eq!(stored.len(), 3, "a repeat of an older command appends");
    let newest = history::decode(stored.last().unwrap());
    assert_eq!(newest.command, "ls");
    assert_eq!(newest.hits, 2, "the repeat counts one more hit");
}

#[test]
fn identical_consecutive_command_is_stored_once() {
    let path = scratch("consecutive").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.set_vacuum_offset(0);

    history.try_add("ls").unwrap();
    history.try_add("ls").unwrap();
    history.try_add("ls").unwrap();

    assert_eq!(history.entries(), ["ls"]);
    assert_eq!(records(&path).len(), 1);
}

#[test]
fn view_keeps_one_copy_of_each_command() {
    let path = scratch("view").join("history");
    std::fs::write(&path, "a\nb\na\n").unwrap();
    let mut history = History::try_load(path.clone(), 100).unwrap();

    // Loading collapses to the newest position of each command.
    assert_eq!(history.entries(), ["b", "a"]);

    history.try_add("b").unwrap();

    assert_eq!(history.entries(), ["a", "b"]);
}

#[test]
fn command_containing_a_carriage_return_round_trips() {
    // A pasted CRLF line keeps the CR the shell received instead of losing it.
    // Both records matter: the first is the one a splitting reader silently trims.
    let legacy = scratch("crlf-legacy").join("history");
    std::fs::write(&legacy, "echo x\r\necho y\r\n").unwrap();
    assert_eq!(
        History::try_load(legacy, 100).unwrap().entries(),
        ["echo x\r", "echo y\r"]
    );

    // A CR the command carries mid-text survives the v2 codec too, escaped so the
    // record still has no raw control separator.
    let path = scratch("crlf-v2").join("history");
    let command = "printf 'a\r\nb'";
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add(command).unwrap();

    assert_eq!(records(&path).len(), 1, "an escaped CR keeps one record");
    assert_eq!(history::decode(&records(&path)[0]).command, command);
    assert_eq!(History::try_load(path, 100).unwrap().entries(), [command]);
}

#[test]
fn append_writes_v2_with_when_cwd_paths_and_hits() {
    let path = scratch("append-v2").join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    let before = now_micros();

    history
        .try_add_with("make -j2", Some("/w"), &["/w/a.rs", "/w/b.rs"])
        .unwrap();

    let after = now_micros();
    let stored = records(&path);
    assert_eq!(stored.len(), 1);
    assert!(stored[0].starts_with("h2\t"), "line: {:?}", stored[0]);
    let record = history::decode(&stored[0]);
    assert!(!record.is_legacy());
    assert_eq!(record.command, "make -j2");
    assert_eq!(record.cwd.as_deref(), Some("/w"));
    assert_eq!(record.paths, ["/w/a.rs", "/w/b.rs"]);
    assert_eq!(record.hits, 1);
    let when = record.when.unwrap();
    assert!(
        (before..=after).contains(&when),
        "when {when} outside [{before}, {after}]"
    );
    assert_eq!(history.entries(), ["make -j2"]);
}

#[test]
fn v2_record_round_trips_its_hit_count() {
    let mut record = history::Record::new("cargo test");
    record.when = Some(5);
    record.hits = 7;

    let line = record.encode();

    assert_eq!(line.split('\t').next_back(), Some("7"));
    let decoded = history::decode(&line);
    assert_eq!(decoded.hits, 7);
    assert_eq!(decoded.command, "cargo test");
    assert_eq!(decoded.encode(), line, "re-encoding must be stable");
}

#[test]
fn v2_record_without_hits_defaults_to_one() {
    // A line written before the hit field existed ends at the command.
    let record = history::decode("h2\t1700000000000000\t/tmp\t0\techo hi");

    assert!(!record.is_legacy());
    assert_eq!(record.command, "echo hi");
    assert_eq!(record.hits, 1);
}

#[test]
fn vacuum_preserves_hits_and_legacy_bytes() {
    let path = scratch("vacuum-hits").join("history");
    let first = v2_line("dup", 1, Some("/w"), &[]);
    let mut newer = history::Record::new("dup");
    newer.when = Some(2);
    newer.hits = 2;
    std::fs::write(&path, format!("legacy cmd\n{first}\n{}\n", newer.encode())).unwrap();

    let mut history = History::try_load(path.clone(), 100).unwrap();
    assert_eq!(history.entries(), ["legacy cmd", "dup"]);
    // Pin the trigger so the compaction lands on the 25th append.
    history.set_vacuum_offset(0);

    for index in 0..25 {
        history.try_add(&format!("other {index}")).unwrap();
    }

    let stored = records(&path);
    assert_eq!(stored[0], "legacy cmd", "legacy bytes stay verbatim");
    let dups: Vec<_> = stored
        .iter()
        .map(|line| history::decode(line))
        .filter(|record| record.command == "dup")
        .collect();
    assert_eq!(dups.len(), 1);
    assert_eq!(dups[0].hits, 2, "the newest record keeps its hit count");
}

#[test]
fn vacuum_keeps_only_the_newest_records_when_over_the_cap() {
    let path = scratch("cap-newest").join("history");
    let original: Vec<String> = (0..12).map(|index| format!("command {index}")).collect();
    std::fs::write(&path, format!("{}\n", original.join("\n"))).unwrap();

    let history = History::try_load(path.clone(), 100).unwrap();
    history.vacuum_with_cap(5);

    assert_eq!(
        commands(&path),
        [
            "command 7",
            "command 8",
            "command 9",
            "command 10",
            "command 11"
        ],
        "the cut follows file position, not any timestamp"
    );
    assert_eq!(records(&path).len(), 5);
    assert_eq!(History::try_load(path, 100).unwrap().entries().len(), 5);
}

#[test]
fn vacuum_preserves_legacy_record_bytes() {
    let path = scratch("cap-legacy").join("history");
    // Distinct lines so trimming, not dedup, is what drops the old ones. The kept
    // legacy line carries escapes that a re-encode would silently change.
    let v2 = v2_line("v2 kept", 1, Some("/w"), &["/w/a"]);
    std::fs::write(
        &path,
        format!("echo a\\nb\ncafé\nh2 not a header\ntab\\there\n{v2}\nlast legacy\n"),
    )
    .unwrap();

    let history = History::try_load(path.clone(), 100).unwrap();
    history.vacuum_with_cap(3);

    assert_eq!(
        records(&path),
        ["tab\\there", v2.as_str(), "last legacy"],
        "kept records keep their original bytes"
    );
}

#[test]
fn short_session_still_vacuums_eventually() {
    let path = scratch("short-session").join("history");
    std::fs::write(&path, "dup\ndup\nkeep\n").unwrap();

    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.set_vacuum_offset(history::VACUUM_EVERY - 1);

    history.try_add("new").unwrap();

    assert_eq!(commands(&path), ["dup", "keep", "new"]);
    assert_eq!(records(&path).len(), 3, "one append triggered a compaction");
}
