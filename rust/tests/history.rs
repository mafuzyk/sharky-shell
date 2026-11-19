#[path = "../src/history.rs"]
#[allow(dead_code)] // the binary uses default_path and the save shim, this target does not
mod history;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use history::History;

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!(
        "sharky-history-{name}-{}",
        std::process::id()
    ));
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

#[test]
fn add_persists_without_drop() {
    let path = scratch("durable").join("history");
    let mut history = History::load(path.clone(), 100);

    history.add("ls -la");

    assert_eq!(lines(&path), ["ls -la"]);
}

#[test]
fn history_file_is_private_at_creation() {
    let path = scratch("mode").join("nested/history");
    let mut history = History::load(path.clone(), 100);

    history.add("secret command");

    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn escape_roundtrips_newlines_and_backslashes() {
    let dir = scratch("escape");
    let path = dir.join("history");
    let command = "echo a\nb\\c";
    let mut history = History::load(path.clone(), 100);

    history.add(command);
    let raw = std::fs::read_to_string(&path).unwrap();

    assert_eq!(raw, "echo a\\nb\\\\c\n");
    assert_eq!(History::load(path, 100).entries(), [command]);
}

#[test]
fn independent_instances_both_append_and_survive() {
    let path = scratch("two-writers").join("history");
    let mut a = History::load(path.clone(), 100);
    let mut b = History::load(path.clone(), 100);

    a.add("from a");
    b.add("from b");

    assert_eq!(lines(&path), ["from a", "from b"]);
    assert_eq!(History::load(path, 100).entries(), ["from a", "from b"]);
}

#[test]
fn stale_view_waits_for_merge() {
    let path = scratch("merge").join("history");
    let mut a = History::load(path.clone(), 100);
    let mut b = History::load(path.clone(), 100);

    a.add("first");
    assert!(b.entries().is_empty());

    a.add("second");
    b.add("mine");
    assert_eq!(b.entries(), ["mine"]);

    b.merge().unwrap();

    assert_eq!(b.entries(), ["mine", "first", "second"]);
}

#[test]
fn private_mode_keeps_no_file_and_refuses_merge() {
    let dir = scratch("private");
    let mut history = History::private(100);
    let mut empty_path = History::load(PathBuf::new(), 100);

    history.add("in memory");
    empty_path.add("in memory");

    assert!(history.is_private());
    assert!(empty_path.is_private());
    assert_eq!(history.entries(), ["in memory"]);
    assert_eq!(
        history.merge(),
        Err("can't merge history in private mode".to_string())
    );
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn clear_truncates_the_file() {
    let path = scratch("clear").join("history");
    let mut history = History::load(path.clone(), 100);

    history.add("one");
    history.add("two");
    history.clear();

    assert!(history.entries().is_empty());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
    assert!(History::load(path, 100).entries().is_empty());
}

#[test]
fn vacuum_keeps_the_newest_entries_within_the_limit() {
    let path = scratch("vacuum").join("history");
    let mut history = History::load(path.clone(), 3);

    for index in 0..25 {
        history.add(&format!("command {index}"));
    }

    assert_eq!(lines(&path), ["command 22", "command 23", "command 24"]);
    assert_eq!(History::load(path, 3).entries().len(), 3);
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
                let mut history = History::load(path, 1000);
                for index in 0..per_writer {
                    history.add(&expected[writer * per_writer + index]);
                }
            });
        }
    });

    let entries = History::load(path, 1000).entries().to_vec();
    assert_eq!(entries.len(), writers * per_writer);
    for command in &expected {
        assert!(entries.contains(command), "lost {command:.40}");
    }
}
