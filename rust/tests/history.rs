#[path = "../src/history.rs"]
#[allow(dead_code)] // this target exercises a subset of the module
mod history;

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

    assert_eq!(lines(&path), ["ls -la"]);
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
    let raw = std::fs::read_to_string(&path).unwrap();

    assert_eq!(raw, "echo a\\nb\\\\c\n");
    assert_eq!(History::try_load(path, 100).unwrap().entries(), [command]);
}

#[test]
fn independent_instances_both_append_and_survive() {
    let path = scratch("two-writers").join("history");
    let mut a = History::try_load(path.clone(), 100).unwrap();
    let mut b = History::try_load(path.clone(), 100).unwrap();

    a.try_add("from a").unwrap();
    b.try_add("from b").unwrap();

    assert_eq!(lines(&path), ["from a", "from b"]);
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

    assert_eq!(lines(&path), ["first", "second"]);
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
    assert!(
        History::try_load(PathBuf::new(), 100)
            .unwrap()
            .entries()
            .is_empty()
    );
}

#[test]
fn append_failure_is_reported_and_keeps_the_memory_entry() {
    if running_as_root() {
        return;
    }
    let dir = scratch("append-failure");
    let path = dir.join("history");
    let mut history = History::try_load(path.clone(), 100).unwrap();
    history.try_add("before").unwrap();

    chmod(&path, 0o400);
    let error = history.try_add("during").unwrap_err();
    chmod(&path, 0o600);

    assert!(error.contains("history"), "unexpected error: {error}");
    assert_eq!(history.entries(), ["before", "during"]);
    assert_eq!(lines(&path), ["before"]);
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
    assert_eq!(lines(&path), ["kept"]);
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

    other.try_add("repeat").unwrap();
    history.try_add("repeat").unwrap();
    for index in 0..23 {
        history.try_add(&format!("unique {index}")).unwrap();
    }
    history.try_add("last").unwrap();

    let file = lines(&path);
    assert_eq!(file.iter().filter(|line| *line == "repeat").count(), 1);
    assert_eq!(file.first().unwrap(), "repeat");
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
