//! File-backed command history.
//!
//! A command becomes durable at `try_add`: one escaped line is appended to the
//! file under an exclusive `flock` on its parent directory, so two shells never
//! clobber each other. The `limit` the caller passes bounds memory only; the file
//! is shared between sessions that may pass different limits, so nothing on disk
//! is pruned to it. Compaction every `VACUUM_EVERY` appends only collapses
//! duplicate lines, keeping the newest occurrence of each command.
//!
//! Writes and reads are newline records: a crash can leave a torn tail without its
//! newline, which readers drop and the next append truncates away before writing.
//! All disk access locks the parent directory and refuses symlinks, directories,
//! fifos and devices instead of following or blocking on them. Every disk failure
//! reaches the caller through `try_load`, `try_add`, `try_clear` and `merge`.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::{Flock, FlockArg};

/// Appends between compactions; without one the file grows by one line per command.
const VACUUM_EVERY: usize = 25;

const PRIVATE_MERGE: &str = "can't merge history in private mode";

pub struct History {
    entries: Vec<String>,
    path: Option<PathBuf>,
    limit: usize,
    appends: usize,
}

impl History {
    /// Empty means memory only: private mode, or no `SHARKY_HISTORY`/`XDG_DATA_HOME`/
    /// `HOME` to place a file in. There is deliberately no world-writable fallback.
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("SHARKY_HISTORY") {
            return PathBuf::from(path);
        }
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty()) {
            return PathBuf::from(dir).join("sharky/history");
        }
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            return PathBuf::from(home).join(".local/share/sharky/history");
        }
        PathBuf::new()
    }

    /// Reads the file under a shared lock. An empty path is explicit private mode;
    /// every other disk failure returns its diagnostic instead of being folded into
    /// privacy, so the caller can warn the user and then choose `private` itself.
    pub fn try_load(path: PathBuf, limit: usize) -> Result<Self, String> {
        if path.as_os_str().is_empty() {
            return Ok(History::private(limit));
        }
        let entries = read_history(&path)?;
        let mut history = History {
            entries,
            path: Some(path),
            limit,
            appends: 0,
        };
        history.trim();
        Ok(history)
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

    /// Appends to memory, then to the file unless private. The memory entry stays
    /// when the append fails, so the session keeps the command and `merge` can
    /// preserve it as a local-only entry.
    pub fn try_add(&mut self, command: &str) -> Result<(), String> {
        let command = command.trim_end();
        if command.is_empty() || self.entries.iter().any(|entry| entry == command) {
            return Ok(());
        }
        self.entries.push(command.to_string());
        self.trim();
        if self.path.is_none() {
            return Ok(());
        }
        self.append(command)
    }

    /// Rebuilds memory from disk order (the shared chronology) and appends the
    /// entries only this session holds, so a command whose append failed is kept.
    /// Duplicates collapse to their disk position, then the memory limit applies.
    pub fn merge(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err(PRIVATE_MERGE.to_string());
        };
        let disk = read_history(&path)?;
        let local = std::mem::take(&mut self.entries);
        let mut seen = HashSet::with_capacity(disk.len() + local.len());
        let mut merged = Vec::with_capacity(disk.len() + local.len());
        for entry in disk.into_iter().chain(local) {
            if seen.insert(entry.clone()) {
                merged.push(entry);
            }
        }
        self.entries = merged;
        self.trim();
        Ok(())
    }

    /// Truncates the file under an exclusive lock, then clears memory. Memory is
    /// left intact when truncation fails, so a failed clear is never reported as done.
    pub fn try_clear(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            self.entries.clear();
            return Ok(());
        };
        let dir = parent(&path);
        prepare_dir(&dir)?;
        let lock = lock_dir(&dir, FlockArg::LockExclusive)?;
        reject_special(&path)?;
        let created = !path.exists();
        let mut options = OpenOptions::new();
        options.write(true).create(true);
        let file = open_history(&path, &mut options).map_err(|err| describe("open", &path, err))?;
        // Tighten before truncating, so a refused chmod cannot destroy the file.
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| describe("chmod", &path, err))?;
        file.set_len(0)
            .map_err(|err| describe("truncate", &path, err))?;
        file.sync_data()
            .map_err(|err| describe("sync", &path, err))?;
        drop(file);
        if created {
            sync_dir(&dir);
        }
        drop(lock);
        self.entries.clear();
        Ok(())
    }

    fn append(&mut self, command: &str) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let dir = parent(&path);
        prepare_dir(&dir)?;
        let lock = lock_dir(&dir, FlockArg::LockExclusive)?;
        reject_special(&path)?;
        let created = !path.exists();
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        let mut file =
            open_history(&path, &mut options).map_err(|err| describe("open", &path, err))?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|err| describe("chmod", &path, err))?;
        discard_partial_tail(&mut file, &path)?;
        file.write_all(format!("{}\n", escape(command)).as_bytes())
            .map_err(|err| describe("write", &path, err))?;
        file.sync_data()
            .map_err(|err| describe("sync", &path, err))?;
        drop(file);
        if created {
            sync_dir(&dir);
        }
        self.appends += 1;
        if self.appends % VACUUM_EVERY == 0 {
            self.vacuum(&dir, &path);
        }
        drop(lock);
        Ok(())
    }

    /// Compacts under the caller's exclusive lock. Duplicate lines collapse to
    /// their newest position; distinct entries are kept because the session limit
    /// must not shrink a file other sessions share.
    fn vacuum(&self, dir: &Path, path: &Path) {
        let Ok(entries) = read_unlocked(path) else {
            return;
        };
        if entries.is_empty() {
            return;
        }
        let total = entries.len();
        let mut seen = HashSet::new();
        let mut kept = Vec::with_capacity(total);
        for entry in entries.into_iter().rev() {
            if seen.insert(entry.clone()) {
                kept.push(entry);
            }
        }
        if kept.len() == total {
            return;
        }
        kept.reverse();

        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let temp = path.with_file_name(name);
        let mut text = String::new();
        for entry in &kept {
            text.push_str(&escape(entry));
            text.push('\n');
        }

        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let Ok(mut file) = open_history(&temp, &mut options) else {
            return;
        };
        let written = file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .and_then(|()| file.write_all(text.as_bytes()))
            .and_then(|()| file.sync_data());
        drop(file);
        if written.is_ok() {
            let _ = std::fs::rename(&temp, path);
        } else {
            let _ = std::fs::remove_file(&temp);
        }
        sync_dir(dir);
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

fn describe(action: &str, path: &Path, err: std::io::Error) -> String {
    format!("can't {action} {}: {err}", path.display())
}

/// Created before a lock can exist, so the mkdir itself is the one unlocked write.
fn prepare_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|err| describe("create", dir, err))?;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

fn lock_dir(dir: &Path, arg: FlockArg) -> Result<Flock<File>, String> {
    // `O_DIRECTORY` makes the lock fd a directory or nothing, so a fifo swapped in
    // for the directory cannot block the open.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY)
        .open(dir)
        .map_err(|err| describe("open", dir, err))?;
    Flock::lock(file, arg).map_err(|(_, err)| format!("can't lock {}: {err}", dir.display()))
}

/// Refuses anything that is not a regular file before it can be followed or
/// blocked on; `O_NOFOLLOW` below covers the race after this check.
fn reject_special(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(format!(
            "refusing history path {}: not a regular file",
            path.display()
        )),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(describe("inspect", path, err)),
    }
}

/// `O_NONBLOCK` keeps the open from hanging on a fifo that replaced the file after
/// `reject_special`; the metadata check below then refuses it.
fn open_history(path: &Path, options: &mut OpenOptions) -> std::io::Result<File> {
    let file = options
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// Drops a record a crash left unfinished, so the append that follows starts a new
/// line instead of extending a torn one. Complete newline records are untouched.
fn discard_partial_tail(file: &mut File, path: &Path) -> Result<(), String> {
    let length = file
        .metadata()
        .map_err(|err| describe("inspect", path, err))?
        .len();
    if length == 0 {
        return Ok(());
    }
    let mut last = [0u8; 1];
    file.seek(SeekFrom::End(-1))
        .and_then(|_| file.read_exact(&mut last))
        .map_err(|err| describe("read", path, err))?;
    if last[0] == b'\n' {
        return Ok(());
    }
    // Rare repair path: rescan for the last record boundary.
    let mut bytes = Vec::new();
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.read_to_end(&mut bytes))
        .map_err(|err| describe("read", path, err))?;
    let cut = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |position| position as u64 + 1);
    file.set_len(cut)
        .map_err(|err| describe("truncate", path, err))
}

fn sync_dir(dir: &Path) {
    let _ = File::open(dir).and_then(|dir| dir.sync_all());
}

/// Caller already holds the directory lock.
fn read_unlocked(path: &Path) -> Result<Vec<String>, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    let mut file = match open_history(path, &mut options) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(describe("read", path, err)),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|err| describe("read", path, err))?;
    parse(path, &bytes)
}

fn read_history(path: &Path) -> Result<Vec<String>, String> {
    let dir = parent(path);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let lock = lock_dir(&dir, FlockArg::LockShared)?;
    reject_special(path)?;
    let entries = read_unlocked(path);
    drop(lock);
    entries
}

/// Only newline-terminated records count: a crash can leave a partial tail, which
/// must not become an entry or make the whole file undecodable when its UTF-8 is
/// cut mid-character. A file written by an older release always ends in a newline,
/// so its records parse unchanged.
fn parse(path: &Path, bytes: &[u8]) -> Result<Vec<String>, String> {
    let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
        return Ok(Vec::new());
    };
    let complete = std::str::from_utf8(&bytes[..end]).map_err(|err| {
        describe(
            "read",
            path,
            std::io::Error::new(ErrorKind::InvalidData, err),
        )
    })?;
    Ok(complete
        .lines()
        .map(unescape)
        .filter(|entry| !entry.is_empty())
        .collect())
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
