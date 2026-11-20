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
//! Every entry remembers where it came from. What this session loaded at startup and
//! what it ran itself may answer a history event; a record picked up from another
//! session afterwards may not, so a command typed in one terminal is never executed
//! in another by `!!`. [`History::refresh`] is what picks those records up, and the
//! interactive loop calls it before each primary prompt; one `stat` decides whether
//! the file has to be read at all.
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
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
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
    /// Parallel to `entries`: where each one came from, which decides whether a
    /// history event may resolve it. Invariant: same length as `entries`.
    origins: Vec<Origin>,
    /// Parallel to `entries`: the newest timestamp known for each command, absent
    /// for a legacy record. Invariant: same length as `entries`.
    whens: Vec<Option<i64>>,
    /// The newest hit count known for each entry, keyed by command. Kept beside the
    /// view so a repeat can carry the count forward without re-reading the file.
    hits: HashMap<String, u64>,
    /// The location and paths each entry named, keyed by command. Like `hits`, it is
    /// keyed rather than parallel so an entry can drop without leaving a hole.
    meta: HashMap<String, Meta>,
    path: Option<PathBuf>,
    limit: usize,
    appends: usize,
    /// What the file held when this view was built; one `stat` per prompt tells
    /// whether anything has to be read.
    seen: Snapshot,
}

/// Where a view entry came from.
///
/// [`Origin::Loaded`] was read at startup and [`Origin::Local`] was appended by this
/// session, so both may answer `!!`, `!n` and `!prefix`. [`Origin::Refreshed`] was
/// contributed by another session after startup and may not: the editor, the
/// autosuggest and the `history` builtin show it, and that is all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Loaded,
    Local,
    Refreshed,
}

/// One entry of the in-memory view: the command, the newest timestamp known for
/// it and where the view picked it up. `when` is absent for a legacy record,
/// which never carried a timestamp; a reader must not invent one.
///
/// The `history` builtin lists these and deletes by exact command, and the
/// suggest path reuses the same list, so the shape stays command-centric rather
/// than exposing the on-disk [`Record`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub command: String,
    pub when: Option<i64>,
    #[allow(dead_code)] // read by the suggest path, not by the `history` builtin
    pub origin: Origin,
    #[allow(dead_code)] // read by the suggest path to validate the paths it names
    pub cwd: Option<String>,
    #[allow(dead_code)] // read by the suggest path to validate the paths it names
    pub paths: Vec<String>,
    /// How many times the command was recorded when its newest line was written.
    pub hits: u64,
}

/// Where a command ran and the paths it named, kept beside the view so the suggest
/// path can decide whether a candidate still applies. Empty paths mean the record
/// named nothing that existed when it was written, which never disqualifies it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Meta {
    cwd: Option<String>,
    paths: Vec<String>,
}

/// The state of the file the in-memory view was built from, as one `stat` reports
/// it. Comparison is exact on purpose: any doubt means a read, never a missed
/// record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Snapshot {
    /// No file existed when the view was built.
    Absent,
    /// The file held exactly this identity, size and modification time.
    At(Fingerprint),
    /// The view is older than the file and the next refresh has to read it.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Snapshot {
    fn current(path: &Path) -> Self {
        match fingerprint(path) {
            Some(fingerprint) => Snapshot::At(fingerprint),
            None => Snapshot::Absent,
        }
    }
}

/// The file's identity in one `stat`; `None` when it cannot be statted, including
/// when it does not exist. A refusal is not an error here, because the caller
/// compares it and reads only on a difference.
fn fingerprint(path: &Path) -> Option<Fingerprint> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(Fingerprint {
        dev: metadata.dev(),
        ino: metadata.ino(),
        size: metadata.size(),
        mtime: metadata.mtime(),
        mtime_nsec: metadata.mtime_nsec(),
    })
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
        // Statted before the read: a change that lands while the read runs then
        // shows up as a difference, and the next refresh reads again instead of
        // missing it. The other order could hide a record for good.
        let seen = Snapshot::current(&path);
        let loaded = view(read_history(&path)?);
        let origins = vec![Origin::Loaded; loaded.entries.len()];
        let mut history = History {
            entries: loaded.entries,
            origins,
            whens: loaded.whens,
            hits: loaded.hits,
            meta: loaded.meta,
            path: Some(path),
            limit,
            appends: initial_vacuum_offset(),
            seen,
        };
        history.trim();
        Ok(history)
    }

    /// No file at all: nothing is read, created or written.
    pub fn private(limit: usize) -> Self {
        History {
            entries: Vec::new(),
            origins: Vec::new(),
            whens: Vec::new(),
            hits: HashMap::new(),
            meta: HashMap::new(),
            path: None,
            limit,
            appends: initial_vacuum_offset(),
            seen: Snapshot::Absent,
        }
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The in-memory view with the metadata a search surface needs: each command
    /// once, at its newest position, with that record's timestamp and origin.
    /// Order is the same as [`History::entries`], oldest first.
    pub fn records(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .zip(&self.whens)
            .zip(&self.origins)
            .map(|((command, when), origin)| {
                let meta = self.meta.get(command);
                Entry {
                    command: command.clone(),
                    when: *when,
                    origin: *origin,
                    cwd: meta.and_then(|meta| meta.cwd.clone()),
                    paths: meta.map(|meta| meta.paths.clone()).unwrap_or_default(),
                    hits: self.hits.get(command).copied().unwrap_or(1),
                }
            })
            .collect()
    }

    /// The commands a history event may resolve: what this session loaded at startup
    /// and what it ran itself, in the same order as [`History::entries`].
    ///
    /// Records another session contributed are excluded, so a refresh can add to
    /// the editor and the listing without ever giving `!!` a command this session
    /// did not run. The list is also the numbering `!n` uses: after a refresh its
    /// positions are the expandable entries, not the editor's.
    pub fn expansion_entries(&self) -> Vec<String> {
        self.entries
            .iter()
            .zip(&self.origins)
            .filter(|(_, origin)| **origin != Origin::Refreshed)
            .map(|(entry, _)| entry.clone())
            .collect()
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
    /// The short form of [`History::try_add_with`], for callers that carry no
    /// metadata: the shell records through the full form, and the codec tests use
    /// this one.
    #[allow(dead_code)]
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
            // A command a sibling ran is not a duplicate of ours until this session
            // has run it too. Promote it, so `!!` here resolves to it from now on
            // while the file still holds the one record.
            if let Some(origin) = self.origins.last_mut() {
                if *origin == Origin::Refreshed {
                    *origin = Origin::Local;
                }
            }
            return Ok(());
        }
        let hits = match self.entries.iter().position(|entry| entry == command) {
            Some(position) => {
                self.entries.remove(position);
                self.origins.remove(position);
                self.whens.remove(position);
                self.hits.get(command).copied().unwrap_or(1) + 1
            }
            None => 1,
        };
        let when = now_micros();
        let paths: Vec<String> = paths
            .iter()
            .take(MAX_PATHS)
            .map(|path| path.to_string())
            .collect();
        self.entries.push(command.to_string());
        self.origins.push(Origin::Local);
        self.whens.push(Some(when));
        self.hits.insert(command.to_string(), hits);
        self.meta.insert(
            command.to_string(),
            Meta {
                cwd: cwd.map(str::to_string),
                paths: paths.clone(),
            },
        );
        self.trim();
        if self.path.is_none() {
            return Ok(());
        }
        let mut record = Record::new(command);
        record.when = Some(when);
        record.cwd = cwd.map(str::to_string);
        record.paths = paths;
        record.hits = hits;
        self.append(&record)
    }

    /// Picks up what other sessions appended, which is what the interactive loop
    /// calls before each primary prompt.
    ///
    /// One `stat` decides: while the file still has the identity, size and
    /// modification time the view was built from, nothing is read and no lock is
    /// taken. Any difference re-reads the whole file under a shared lock and merges
    /// it in file order, which also covers a compaction or a clear that moved the
    /// file's inode or shrank it.
    ///
    /// This session's own entries keep their order relative to each other, an entry
    /// a failed append kept out of the file survives, and a record the disk no longer
    /// has (a clear elsewhere) disappears unless this session appended it. Records
    /// already known keep their origin, so a sibling's command stays
    /// [`Origin::Refreshed`] and out of [`History::expansion_entries`].
    ///
    /// Private mode has no path and returns before any syscall.
    pub fn refresh(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let current = Snapshot::current(&path);
        if current == self.seen {
            return Ok(());
        }
        // Recorded before the read, so a file that cannot be read is reported once
        // instead of on every prompt; the next real change still differs.
        self.seen = current;
        self.reload(&path)
    }

    /// Rebuilds memory from disk order (the shared chronology) and keeps the entries
    /// only this session holds, so a command whose append failed is not lost.
    /// Duplicates collapse to their newest disk position, then the memory limit
    /// applies.
    ///
    /// This is [`History::refresh`] with the fingerprint check skipped: it always
    /// re-reads, so it is idempotent. Private mode has nothing to merge.
    pub fn merge(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Err(PRIVATE_MERGE.to_string());
        };
        self.seen = Snapshot::current(&path);
        self.reload(&path)
    }

    /// The one merge path behind [`History::refresh`] and [`History::merge`].
    ///
    /// The disk holds the shared chronology, so its order wins for everything on it,
    /// and an entry keeps the origin the view already had for it: a command this
    /// session ran stays [`Origin::Local`] and a sibling's stays
    /// [`Origin::Refreshed`]. An entry this session appended that the file does not
    /// have stays, at the end, where memory-only entries have always gone; an entry
    /// the disk dropped was never local to this session and goes away.
    fn reload(&mut self, path: &Path) -> Result<(), String> {
        let View {
            entries,
            mut hits,
            mut whens,
            mut meta,
        } = view(read_history(path)?);
        let old_entries = std::mem::take(&mut self.entries);
        let old_origins = std::mem::take(&mut self.origins);
        let old_whens = std::mem::take(&mut self.whens);
        let old_hits = std::mem::take(&mut self.hits);
        let mut old_meta = std::mem::take(&mut self.meta);
        let mut previous: HashMap<String, Origin> = old_entries
            .iter()
            .cloned()
            .zip(old_origins)
            .collect();
        let mut old_when: HashMap<String, Option<i64>> = old_entries
            .iter()
            .cloned()
            .zip(old_whens)
            .collect();
        let mut merged = Vec::with_capacity(old_entries.len() + entries.len());
        let mut origins = Vec::with_capacity(merged.capacity());
        for entry in entries {
            origins.push(previous.remove(entry.as_str()).unwrap_or(Origin::Refreshed));
            merged.push(entry);
        }
        for command in old_entries {
            if hits.contains_key(&command) {
                // Already on disk, so its file position decides where it sits and
                // the record carries the hit count.
                continue;
            }
            if previous.get(&command) != Some(&Origin::Local) {
                continue;
            }
            let count = old_hits.get(&command).copied().unwrap_or(1);
            hits.insert(command.clone(), count);
            if let Some(found) = old_meta.remove(&command) {
                meta.insert(command.clone(), found);
            }
            origins.push(Origin::Local);
            whens.push(old_when.remove(&command).flatten());
            merged.push(command);
        }
        self.entries = merged;
        self.origins = origins;
        self.whens = whens;
        self.hits = hits;
        self.meta = meta;
        self.trim();
        Ok(())
    }

    /// Truncates the file under an exclusive lock, then clears memory. Memory is
    /// left intact when truncation fails, so a failed clear is never reported as done.
    pub fn try_clear(&mut self) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            self.entries.clear();
            self.origins.clear();
            self.whens.clear();
            self.meta.clear();
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
        self.seen = Snapshot::current(&path);
        drop(lock);
        self.entries.clear();
        self.origins.clear();
        self.whens.clear();
        self.hits.clear();
        self.meta.clear();
        Ok(())
    }

    /// Removes every record whose command equals `pattern` exactly, then rewrites
    /// the file under the same exclusive directory lock an append takes, so a
    /// sibling appending meanwhile is either read or serialized after the rewrite,
    /// never lost between them.
    ///
    /// Kept lines are copied byte for byte, so a legacy record stays byte identical,
    /// a v2 record keeps its `hits` field and any field this reader ignores, and an
    /// undecodable line is preserved rather than re-encoded. Only complete newline
    /// records take part: the torn tail a crash left is not a record and the rewrite
    /// ends with a newline, exactly the state the next append expects.
    ///
    /// Private mode deletes from memory only. The count is the number of removed
    /// records on disk (several legacy duplicates can match); memory holds one entry
    /// per command, so it drops at most one.
    pub fn try_delete_exact(&mut self, pattern: &str) -> Result<usize, String> {
        let Some(path) = self.path.clone() else {
            self.purge_exact(pattern);
            return Ok(0);
        };
        let dir = parent(&path);
        prepare_dir(&dir)?;
        let lock = lock_dir(&dir, FlockArg::LockExclusive)?;
        reject_special(&path)?;
        let bytes = read_bytes_unlocked(&path)?;
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n').map(|at| at + 1) else {
            // No complete record on disk, so the file stays as it is; a matching
            // memory entry (a failed append) is still what the user asked to delete.
            drop(lock);
            self.purge_exact(pattern);
            return Ok(0);
        };
        let mut lines: Vec<&[u8]> = bytes[..end].split(|byte| *byte == b'\n').collect();
        // The split ends with an empty slice after the final newline; that is a
        // separator, not a blank record.
        if lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        let mut kept: Vec<&[u8]> = Vec::with_capacity(lines.len());
        let mut removed = 0usize;
        for line in lines {
            let matches = !line.is_empty()
                && std::str::from_utf8(line)
                    .ok()
                    .is_some_and(|line| decode(line).command == pattern);
            if matches {
                removed += 1;
            } else {
                kept.push(line);
            }
        }
        // No match leaves the file, its inode and its torn tail exactly as they were.
        if removed == 0 {
            drop(lock);
            self.purge_exact(pattern);
            return Ok(0);
        }
        let mut text = Vec::with_capacity(bytes.len());
        for line in &kept {
            text.extend_from_slice(line);
            text.push(b'\n');
        }

        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        let temp = path.with_file_name(name);
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file =
            open_history(&temp, &mut options).map_err(|err| describe("create", &temp, err))?;
        let written = file
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .and_then(|()| file.write_all(&text))
            .and_then(|()| file.sync_data());
        drop(file);
        if let Err(err) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(describe("write", &temp, err));
        }
        if let Err(err) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(describe("rename", &temp, err));
        }
        sync_dir(&dir);
        drop(lock);
        self.purge_exact(pattern);
        self.seen = Snapshot::current(&path);
        Ok(removed)
    }

    /// Drops exact matches from the in-memory view. The view holds one entry per
    /// command, so this removes at most one and returns how many it removed.
    fn purge_exact(&mut self, pattern: &str) -> usize {
        let mut removed = 0;
        let mut index = 0;
        while index < self.entries.len() {
            if self.entries[index] == pattern {
                self.entries.remove(index);
                self.origins.remove(index);
                self.whens.remove(index);
                self.hits.remove(pattern);
                self.meta.remove(pattern);
                removed += 1;
            } else {
                index += 1;
            }
        }
        removed
    }

    fn append(&mut self, record: &Record) -> Result<(), String> {
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let dir = parent(&path);
        prepare_dir(&dir)?;
        let lock = lock_dir(&dir, FlockArg::LockExclusive)?;
        reject_special(&path)?;
        // Under the exclusive lock the file cannot move, so this is exactly what the
        // view was compared against a moment ago.
        let known = self.seen;
        let before = Snapshot::current(&path);
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
        // The fingerprint may describe the new file only when the old one was
        // already what this view holds: a record another session wrote before this
        // one would otherwise be counted as read and never picked up. Otherwise the
        // view is marked older than the file and the next prompt merges it.
        self.seen = if before == known {
            Snapshot::current(&path)
        } else {
            Snapshot::Stale
        };
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
        debug_assert_eq!(self.entries.len(), self.origins.len());
        debug_assert_eq!(self.entries.len(), self.whens.len());
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.origins.drain(..excess);
            self.whens.drain(..excess);
            for command in self.entries.drain(..excess) {
                self.hits.remove(&command);
                self.meta.remove(&command);
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
    parse(path, &read_bytes_unlocked(path)?)
}

/// The raw file under the caller's lock; `NotFound` is an empty file. Bytes are
/// kept as they are so a rewrite can copy undecodable lines verbatim.
fn read_bytes_unlocked(path: &Path) -> Result<Vec<u8>, String> {
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
    Ok(bytes)
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
struct View {
    entries: Vec<String>,
    hits: HashMap<String, u64>,
    whens: Vec<Option<i64>>,
    meta: HashMap<String, Meta>,
}

fn view(records: Vec<Record>) -> View {
    let mut hits = HashMap::with_capacity(records.len());
    let mut entries = Vec::with_capacity(records.len());
    let mut whens = Vec::with_capacity(records.len());
    let mut meta = HashMap::with_capacity(records.len());
    for record in records.into_iter().rev() {
        if hits.contains_key(&record.command) {
            continue;
        }
        hits.insert(record.command.clone(), record.hits);
        whens.push(record.when);
        meta.insert(
            record.command.clone(),
            Meta {
                cwd: record.cwd.clone(),
                paths: record.paths.clone(),
            },
        );
        entries.push(record.command);
    }
    entries.reverse();
    whens.reverse();
    View {
        entries,
        hits,
        whens,
        meta,
    }
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
