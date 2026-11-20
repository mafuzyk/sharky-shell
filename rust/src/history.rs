//! File-backed command history.
//!
//! A command becomes durable at `try_add`: one v2 record is appended to the file
//! under an exclusive `flock` on its parent directory, so two shells never clobber
//! each other. The `limit` the caller passes bounds memory only; the file is shared
//! between sessions that may pass different limits, so nothing on disk is pruned to
//! it. Compaction every `VACUUM_EVERY` appends collapses duplicate records, keeping
//! the newest occurrence of each command with its own bytes, and bounds the file at
//! [`HISTORY_CAP`] records by file position. The trigger counter starts at a random
//! offset so even a short-lived session eventually compacts.
//!
//! The in-memory view holds one copy of each command at the position of its newest
//! record, so a repeat of an older command has to be appended again to become the
//! newest one and answer `!!` correctly. Two identical consecutive commands are one
//! event and write nothing the second time.
//!
//! Writes and reads are newline records: a crash can leave a torn tail without its
//! newline, which readers drop and the next append truncates away before writing.
//! All disk access locks the parent directory and refuses symlinks, directories,
//! fifos and devices instead of following or blocking on them. Every disk failure
//! reaches the caller through `try_load`, `try_add`, `try_clear` and `merge`.
//!
//! Reading accepts both record formats: the legacy bare escaped command and the v2
//! TAB-separated record documented on [`Record`]. Both escape their own newlines,
//! so a record is always one line and a torn tail stays detectable. Records are
//! split on a bare `\n`, never on `str::lines`, so a carriage return a CRLF paste
//! left in the command survives.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nix::fcntl::{Flock, FlockArg};

/// Appends between compactions; without one the file grows by one line per command.
pub const VACUUM_EVERY: usize = 25;

/// Hard bound on records in the shared history file, independent of any session's
/// in-memory `tide history` limit. A session with a small limit must not truncate a
/// file other sessions share; this cap is the only disk bound and applies by file
/// position, never by a record's declared timestamp.
pub const HISTORY_CAP: usize = 262_144;

const PRIVATE_MERGE: &str = "can't merge history in private mode";

/// Marker of a v2 record: detection needs it to be the first field, so a legacy
/// command that merely starts with those bytes stays legacy.
const V2_MARKER: &str = "h2";

/// A v2 line needs these fields besides its paths: marker, when, cwd, npaths, cmd.
const V2_BASE_FIELDS: usize = 5;

/// Paths beyond this are dropped on read. A record is a hint, never a manifest.
pub const MAX_PATHS: usize = 8;

/// One history record.
///
/// The v2 grammar is one line per record with fields separated by a raw TAB:
///
/// ```text
/// h2 \t <when-us> \t <cwd> \t <npaths> \t <path>... \t <cmd> \t <hits>
/// ```
///
/// `when` is microseconds since the Unix epoch. The command sits at field index
/// `4 + npaths`, so a record with paths never shifts its command. `hits` follows the
/// command as an extra field: how many times the command has been recorded up to
/// this line. A reader that predates it stops at the command, and a v2 line without
/// it decodes as one hit. Fields past `hits` are ignored, which lets a newer writer
/// add fields without breaking this reader. `npaths` must agree with the fields that
/// follow, or the line is not a valid v2 header and loads as legacy.
///
/// Escapes inside every text field: backslash is doubled (`\\`), newline becomes
/// `\n`, TAB becomes `\t` and carriage return becomes `\r`. A raw TAB is therefore
/// always a separator and a record never spans two lines.
///
/// A legacy (v1) line is a bare escaped command with no header. It loads with
/// `when = None` and keeps the line it was decoded from, so compaction can write
/// it back byte for byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub command: String,
    pub when: Option<i64>,
    pub cwd: Option<String>,
    pub paths: Vec<String>,
    /// How many times the command had been recorded when this line was written.
    pub hits: u64,
    /// The line as it sits on disk, present only for a legacy record.
    original: Option<String>,
}

#[allow(dead_code)] // `is_legacy` and `original_line` are exercised by the codec tests
impl Record {
    /// A record this session creates; no legacy bytes are attached to it.
    pub fn new(command: impl Into<String>) -> Self {
        Record {
            command: command.into(),
            when: None,
            cwd: None,
            paths: Vec::new(),
            hits: 1,
            original: None,
        }
    }

    pub fn is_legacy(&self) -> bool {
        self.original.is_some()
    }

    /// The original line for a legacy record, still escaped and without its newline.
    pub fn original_line(&self) -> Option<&str> {
        self.original.as_deref()
    }

    /// The record as one line, without the terminating newline. A legacy record is
    /// reproduced verbatim; anything else is written as v2. A missing `when`
    /// encodes as `0`, the epoch.
    pub fn encode(&self) -> String {
        if let Some(line) = &self.original {
            return line.clone();
        }
        let paths = &self.paths[..self.paths.len().min(MAX_PATHS)];
        let mut line = String::with_capacity(self.command.len() + 40);
        line.push_str(V2_MARKER);
        line.push('\t');
        line.push_str(&self.when.unwrap_or(0).to_string());
        line.push('\t');
        line.push_str(&encode_field(self.cwd.as_deref().unwrap_or_default()));
        line.push('\t');
        line.push_str(&paths.len().to_string());
        for path in paths {
            line.push('\t');
            line.push_str(&encode_field(path));
        }
        line.push('\t');
        line.push_str(&encode_field(&self.command));
        line.push('\t');
        line.push_str(&self.hits.max(1).to_string());
        line
    }
}

/// Decodes one line from either format. Detection needs the marker, at least the
/// base fields, an integer `when` and a `usize` `npaths` that fits the line;
/// everything else is a legacy command.
pub fn decode(line: &str) -> Record {
    decode_v2(line).unwrap_or_else(|| Record {
        command: unescape(line),
        when: None,
        cwd: None,
        paths: Vec::new(),
        hits: 1,
        original: Some(line.to_string()),
    })
}

fn decode_v2(line: &str) -> Option<Record> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < V2_BASE_FIELDS || fields[0] != V2_MARKER {
        return None;
    }
    let when = fields[1].parse::<i64>().ok()?;
    let npaths = fields[3].parse::<usize>().ok()?;
    // The command index follows the declared path count; a count the line cannot
    // hold means this is not a v2 header at all.
    let command_index = 4usize.checked_add(npaths)?;
    let command = *fields.get(command_index)?;
    // The hit count is an extra field after the command, so a line written before
    // it existed still decodes, as one hit. A non-numeric field there is a future
    // one and is ignored the same way.
    let hits = fields
        .get(command_index + 1)
        .and_then(|field| field.parse::<u64>().ok())
        .unwrap_or(1)
        .max(1);
    let cwd = decode_field(fields[2]);
    let paths = fields[4..4 + npaths]
        .iter()
        .take(MAX_PATHS)
        .map(|path| decode_field(path))
        .collect();
    Some(Record {
        command: decode_field(command),
        when: Some(when),
        cwd: (!cwd.is_empty()).then_some(cwd),
        paths,
        hits,
        original: None,
    })
}

pub struct History {
    entries: Vec<String>,
    /// The newest hit count known for each entry, keyed by command. Kept beside the
    /// view so a repeat can carry the count forward without re-reading the file.
    hits: HashMap<String, u64>,
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
        let (entries, hits) = view(read_history(&path)?);
        let mut history = History {
            entries,
            hits,
            path: Some(path),
            limit,
            appends: initial_vacuum_offset(),
        };
        history.trim();
        Ok(history)
    }

    /// No file at all: nothing is read, created or written.
    pub fn private(limit: usize) -> Self {
        History {
            entries: Vec::new(),
            hits: HashMap::new(),
            path: None,
            limit,
            appends: initial_vacuum_offset(),
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Overrides the vacuum trigger counter. Production starts it at a random
    /// offset in `0..VACUUM_EVERY`; tests pin it so the first compaction is
    /// predictable.
    #[allow(dead_code)] // test hook for deterministic compaction timing
    pub fn set_vacuum_offset(&mut self, offset: usize) {
        self.appends = offset % VACUUM_EVERY;
    }

    /// Compacts the file now with an explicit cap. Production uses [`HISTORY_CAP`];
    /// tests pass a small cap to exercise trimming without writing a huge file.
    /// Acquires the same exclusive directory lock an append would, so callers must
    /// not already hold it.
    #[allow(dead_code)] // test hook for exercising the cap without a huge file
    pub fn vacuum_with_cap(&self, cap: usize) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let dir = parent(&path);
        if !dir.exists() {
            return;
        }
        let Ok(lock) = lock_dir(&dir, FlockArg::LockExclusive) else {
            return;
        };
        self.vacuum(&dir, &path, cap);
        drop(lock);
    }

    /// Appends to memory, then to the file unless private. The memory entry stays
    /// when the append fails, so the session keeps the command and `merge` can
    /// preserve it as a local-only entry.
    ///
    /// Two consecutive identical commands are one event: the newest entry already is
    /// that command, so the second one writes nothing. A repeat of an older command
    /// moves back to the newest position and appends a record with one more hit than
    /// the count this session knows for it, so `!!` resolves to the repeat.
    pub fn try_add(&mut self, command: &str) -> Result<(), String> {
        self.try_add_with(command, None, &[])
    }

    /// [`History::try_add`] plus the metadata a picker can use: where the command
    /// ran and the paths it named. Omitted values stay empty on the record.
    pub fn try_add_with(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        paths: &[&str],
    ) -> Result<(), String> {
        let command = command.trim_end();
        if command.is_empty() {
            return Ok(());
        }
        if self.entries.last().map(String::as_str) == Some(command) {
            return Ok(());
        }
        let hits = match self.entries.iter().position(|entry| entry == command) {
            Some(position) => {
                self.entries.remove(position);
                self.hits.get(command).copied().unwrap_or(1) + 1
            }
            None => 1,
        };
        self.entries.push(command.to_string());
        self.hits.insert(command.to_string(), hits);
        self.trim();
        if self.path.is_none() {
            return Ok(());
        }
        let mut record = Record::new(command);
        record.when = Some(now_micros());
        record.cwd = cwd.map(str::to_string);
        record.paths = paths
            .iter()
            .take(MAX_PATHS)
            .map(|path| path.to_string())
            .collect();
        record.hits = hits;
        self.append(&record)
    }

    /// Rebuilds memory from disk order (the shared chronology) and appends the
    /// entries only this session holds, so a command whose append failed is kept.
    /// Duplicates collapse to their newest disk position, then the memory limit
    /// applies.
    pub fn merge(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err(PRIVATE_MERGE.to_string());
        };
        let (mut entries, mut hits) = view(read_history(&path)?);
        let local_entries = std::mem::take(&mut self.entries);
        let local_hits = std::mem::take(&mut self.hits);
        for command in local_entries {
            if hits.contains_key(&command) {
                continue;
            }
            let count = local_hits.get(&command).copied().unwrap_or(1);
            hits.insert(command.clone(), count);
            entries.push(command);
        }
        self.entries = entries;
        self.hits = hits;
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
        self.hits.clear();
        Ok(())
    }

    fn append(&mut self, record: &Record) -> Result<(), String> {
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
        file.write_all(format!("{}\n", record.encode()).as_bytes())
            .map_err(|err| describe("write", &path, err))?;
        file.sync_data()
            .map_err(|err| describe("sync", &path, err))?;
        drop(file);
        if created {
            sync_dir(&dir);
        }
        self.appends += 1;
        if self.appends % VACUUM_EVERY == 0 {
            self.vacuum(&dir, &path, HISTORY_CAP);
        }
        drop(lock);
        Ok(())
    }

    /// Compacts under the caller's exclusive lock. Duplicate commands collapse to
    /// their newest record, which keeps that record's hit count; distinct records
    /// are kept as they stand, because the session limit must not shrink a file
    /// other sessions share. When the file holds more than `cap` records, only the
    /// newest `cap` by file position survive the dedup. Legacy records are rewritten
    /// with their own bytes, v2 ones with their hits field.
    fn vacuum(&self, dir: &Path, path: &Path, cap: usize) {
        let Ok(records) = read_unlocked(path) else {
            return;
        };
        if records.is_empty() {
            return;
        }
        let total = records.len();
        // File position under the lock is the truth: a record's `when` is hint data
        // and must never decide which end of the file gets discarded.
        let start = total.saturating_sub(cap);
        let mut seen = HashSet::new();
        let mut kept: Vec<Record> = Vec::with_capacity(total - start);
        for record in records.into_iter().skip(start).rev() {
            if seen.insert(record.command.clone()) {
                kept.push(record);
            }
        }
        // Rewriting is needed only when dedup or the cap actually dropped records.
        if kept.len() == total {
            return;
        }
        kept.reverse();

        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let temp = path.with_file_name(name);
        let mut text = String::new();
        for record in &kept {
            text.push_str(&record.encode());
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
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            for command in self.entries.drain(..excess) {
                self.hits.remove(&command);
            }
        }
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
fn read_unlocked(path: &Path) -> Result<Vec<Record>, String> {
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

fn read_history(path: &Path) -> Result<Vec<Record>, String> {
    let dir = parent(path);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let lock = lock_dir(&dir, FlockArg::LockShared)?;
    reject_special(path)?;
    let records = read_unlocked(path);
    drop(lock);
    records
}

/// Collapses records to the in-memory view: one entry per command, at the position
/// of its newest record, carrying that record's hit count. Older duplicates on disk
/// never reach the view, so a repeat has to be appended again to become the newest
/// entry.
fn view(records: Vec<Record>) -> (Vec<String>, HashMap<String, u64>) {
    let mut hits = HashMap::with_capacity(records.len());
    let mut entries = Vec::with_capacity(records.len());
    for record in records.into_iter().rev() {
        if hits.contains_key(&record.command) {
            continue;
        }
        hits.insert(record.command.clone(), record.hits);
        entries.push(record.command);
    }
    entries.reverse();
    (entries, hits)
}

/// Only newline-terminated records count: a crash can leave a partial tail, which
/// must not become an entry or make the whole file undecodable when its UTF-8 is
/// cut mid-character. Both formats escape their own newlines, so a record never
/// spans two lines and a bare `\n` split yields them exactly. The split is
/// deliberate: `str::lines` also eats a trailing `\r`, which is part of a command
/// pasted with CRLF endings. Order is file position: legacy lines carry no
/// timestamp, and the newest append is the truth.
pub fn parse(path: &Path, bytes: &[u8]) -> Result<Vec<Record>, String> {
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
        .split('\n')
        .map(decode)
        .filter(|record| !record.command.is_empty())
        .collect())
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as i64)
        .unwrap_or(0)
}

/// A per-session starting point in `0..VACUUM_EVERY`, so a session that appends
/// fewer than `VACUUM_EVERY` records still reaches a compaction. Time and process
/// id decorrelate concurrent sessions without a dependency or a shared seed.
fn initial_vacuum_offset() -> usize {
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64);
    (seed % VACUUM_EVERY as u64) as usize
}

/// Escapes one v2 text field: backslash doubled, newline as `\n`, TAB as `\t` and
/// carriage return as `\r`.
fn encode_field(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for ch in field.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            other => out.push(other),
        }
    }
    out
}

/// Decodes one v2 text field, where `\t` and `\r` are also escapes.
fn decode_field(field: &str) -> String {
    decode_escapes(field, true)
}

/// Shared decoder. Unknown escapes keep their backslash, so data is never lost.
/// Only v2 owns the TAB and carriage-return escapes; a legacy line never had them,
/// so they stay the two characters a user typed there.
fn decode_escapes(line: &str, v2: bool) -> String {
    let mut out = String::new();
    let mut chars = line.chars();

    while let Some(char) = chars.next() {
        if char != '\\' {
            out.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') if v2 => out.push('\t'),
            Some('r') if v2 => out.push('\r'),
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

/// Decodes a v1 command. `\t` stays the two characters a user typed there, since
/// v1 never escaped a TAB.
fn unescape(line: &str) -> String {
    decode_escapes(line, false)
}
