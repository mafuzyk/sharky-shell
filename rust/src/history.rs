use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// Appends between vacuums; without one the file grows by one line per new command forever.
const VACUUM_EVERY: usize = 25;

pub struct History {
    entries: Vec<String>,
    path: Option<PathBuf>,
    limit: usize,
    appends: usize,
}

impl History {
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("SHARKY_HISTORY") {
            return PathBuf::from(path);
        }
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("sharky/history");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/share/sharky/history");
        }
        PathBuf::from("/tmp/sharky-history")
    }

    pub fn load(path: PathBuf, limit: usize) -> Self {
        if path.as_os_str().is_empty() {
            return History::private(limit);
        }
        let mut history = History {
            entries: read_entries(&path),
            path: Some(path),
            limit,
            appends: 0,
        };
        history.trim();
        history
    }

    /// No file at all: nothing is read, created or written.
    pub fn private(limit: usize) -> Self {
        History {
            entries: Vec::new(),
            path: None,
            limit,
            appends: 0,
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_private(&self) -> bool {
        self.path.is_none()
    }

    pub fn add(&mut self, command: &str) {
        let command = command.trim_end();
        if command.is_empty() || self.entries.iter().any(|entry| entry == command) {
            return;
        }
        self.entries.push(command.to_string());
        self.trim();
        self.append(command);
    }

    pub fn merge(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err("can't merge history in private mode".to_string());
        };
        for entry in read_entries(&path) {
            if !self.entries.contains(&entry) {
                self.entries.push(entry);
            }
        }
        self.trim();
        Ok(())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        let Some(path) = self.path.clone() else {
            return;
        };
        let dir = parent(&path);
        prepare_dir(&dir);
        let _lock = lock_dir(&dir);
        if let Ok(file) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            let _ = file.sync_data();
        }
    }

    /// Compatibility shim: entries are durable at `add` now. Remove with the last caller.
    pub fn save(&self) {}

    fn append(&mut self, command: &str) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let dir = parent(&path);
        prepare_dir(&dir);
        let _lock = lock_dir(&dir);
        if let Ok(mut file) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
        {
            let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
            let _ = file.write_all(format!("{}\n", escape(command)).as_bytes());
            let _ = file.sync_data();
        }
        self.appends += 1;
        if self.appends % VACUUM_EVERY == 0 {
            self.vacuum(&dir, &path);
        }
    }

    /// Compacts under the caller's lock: newest duplicate wins, then the limit keeps the tail.
    fn vacuum(&self, dir: &Path, path: &Path) {
        let mut entries = read_entries(path);
        if entries.is_empty() {
            return;
        }
        let mut seen = HashSet::new();
        let mut kept = Vec::with_capacity(entries.len());
        for entry in entries.drain(..).rev() {
            if seen.insert(entry.clone()) {
                kept.push(entry);
            }
        }
        kept.reverse();
        trim_entries(&mut kept, self.limit);

        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let temp = path.with_file_name(name);
        let mut text = String::new();
        for entry in &kept {
            text.push_str(&escape(entry));
            text.push('\n');
        }

        let Ok(mut file) = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)
        else {
            return;
        };
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
        let written = file.write_all(text.as_bytes()).and_then(|()| file.sync_data());
        drop(file);
        if written.is_ok() {
            let _ = std::fs::rename(&temp, path);
        } else {
            let _ = std::fs::remove_file(&temp);
        }
        let _ = File::open(dir).and_then(|dir| dir.sync_all());
    }

    fn trim(&mut self) {
        trim_entries(&mut self.entries, self.limit);
    }
}

fn parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn prepare_dir(dir: &Path) {
    if dir.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// Exclusive lock on the parent directory, released on drop.
fn lock_dir(dir: &Path) -> Option<Flock<File>> {
    Flock::lock(File::open(dir).ok()?, FlockArg::LockExclusive).ok()
}

fn read_entries(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|text| {
            text.lines()
                .map(unescape)
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn trim_entries(entries: &mut Vec<String>, limit: usize) {
    if entries.len() > limit {
        let excess = entries.len() - limit;
        entries.drain(..excess);
    }
}

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();

    while let Some(char) = chars.next() {
        if char != '\\' {
            out.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}
