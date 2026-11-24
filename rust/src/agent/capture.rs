//! Temp-file capture and descriptor identity for the agent runtime.
//!
//! One [`Capture`] owns an unlinked temp file for one stream. The agent swaps
//! fd 0/1/2 onto those files per request ([`StdioGuard`]) and puts back what it
//! found afterwards, so a user `exec >log` inside a request persists across
//! requests as it does in bash (design §4, §5).
//!
//! Two invariants keep the swap honest:
//!
//! * Capture, payload and saved-stdio descriptors never live on fd 0/1/2. A
//!   request can leave a standard descriptor closed, and the next `open` would
//!   otherwise hand that number to a capture, turning `dup2(1, 1)` into a
//!   no-op and letting the restore repoint the file the result is read from.
//! * A capture whose name cannot be unlinked is an error, never a silent
//!   leftover: the whole point of the file is that nothing can reach it by
//!   name after the request.
//!
//! Nothing here knows about the protocol: no JSON, no session state, no shell.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// `(st_dev, st_ino)` of an open descriptor.
pub(crate) type Identity = (u64, u64);

/// Chunk used while reading or replaying a capture file.
const READ_CHUNK: usize = 64 * 1024;

/// Lowest descriptor number a capture, a payload or a saved stdio copy may use.
///
/// The same floor `shell.rs` uses for saved redirection descriptors: fd 0/1/2
/// belong to the request, and a capture that landed there would be rewired by
/// the restore it is supposed to survive.
pub(crate) const FD_FLOOR: RawFd = 10;

/// `F_DUPFD_CLOEXEC` at or above [`FD_FLOOR`], or the errno.
///
/// `F_DUPFD_CLOEXEC` fails with `EBADF` when `fd` is closed, so the caller can
/// tell "nothing there" from a real failure without a separate probe.
pub(crate) fn duplicate_above_min(fd: RawFd, min_fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `fcntl` only reads this process's descriptor table and returns a
    // new descriptor owned by this process.
    let copy = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, min_fd) };
    if copy < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(copy)
    }
}

fn duplicate_above(fd: RawFd) -> io::Result<RawFd> {
    duplicate_above_min(fd, FD_FLOOR)
}

/// Moves `file` off the stdio range, keeping `FD_CLOEXEC`, and closes the
/// descriptor it was created with.
fn lift_off_stdio(file: File) -> io::Result<File> {
    let fd = file.as_raw_fd();
    if fd >= FD_FLOOR {
        return Ok(file);
    }
    let copy = duplicate_above(fd)?;
    // Dropping the low descriptor gives the table back what it had before the
    // `open`: the number was free, so it is free again. The file itself stays
    // open through `copy`.
    drop(file);
    // SAFETY: `copy` came from `fcntl` above and was never owned by a `File`.
    Ok(unsafe { File::from_raw_fd(copy) })
}

/// Candidate directories for anonymous temporary files. Empty paths are
/// skipped: `TMPDIR=""` must not make a name relative to the current
/// directory, and an empty cwd must not either.
pub(crate) fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(tmpdir) = std::env::var_os("TMPDIR")
        && !tmpdir.is_empty()
    {
        dirs.push(PathBuf::from(tmpdir));
    }
    dirs.push(PathBuf::from("/tmp"));
    dirs.push(PathBuf::from("/var/tmp"));
    if let Ok(cwd) = std::env::current_dir()
        && !cwd.as_os_str().is_empty()
    {
        dirs.push(cwd);
    }
    dirs
}

/// Creates an unlinked temp file: `create_new`, mode 0600, `O_CLOEXEC`, name
/// removed at once, descriptor moved above [`FD_FLOOR`].
///
/// This is the `--json -c` discipline (`cli.rs`) moved here so the session and
/// the legacy path share one implementation. The returned descriptor is all
/// that remains of the file; nothing is ever reachable by name.
pub(crate) fn capture_file(kind: &str) -> io::Result<File> {
    capture_file_in(kind, temp_dirs())
}

/// Creates a capture in the first usable candidate directory.
///
/// The candidate iterator is explicit so a unit test can inject an
/// all-failing list and exercise the real error branch without changing the
/// process-wide `TMPDIR`.
fn capture_file_in(
    kind: &str,
    dirs: impl IntoIterator<Item = impl AsRef<Path>>,
) -> io::Result<File> {
    for dir in dirs {
        match create_unlinked(dir.as_ref(), kind, |path| std::fs::remove_file(path)) {
            Ok(file) => return Ok(file),
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no usable temporary directory",
    ))
}

/// [`capture_file`] with the directory and the removal step as parameters.
///
/// The directory is explicit so a test never has to mutate the process-wide
/// `TMPDIR`, and `unlink` is explicit because the kernel cannot be made to fail
/// the removal of a file this process just created (write permission on the
/// directory allows both), so the failure path would otherwise be untestable.
pub(crate) fn create_unlinked(
    dir: &Path,
    kind: &str,
    unlink: impl Fn(&Path) -> io::Result<()>,
) -> io::Result<File> {
    for _ in 0..4 {
        let path = temp_path(dir, kind);
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => {
                // The descriptor is all we need. A name that survives the
                // request would leave the output readable by anyone who can
                // list the directory, so failing to remove it fails the
                // capture instead.
                if let Err(err) = unlink(&path) {
                    drop(file);
                    // The descriptor is gone, so retry once: a removal that
                    // only lost a race can still succeed, while a permission
                    // or read-only filesystem failure is reported as is.
                    let _ = unlink(&path);
                    return Err(err);
                }
                return lift_off_stdio(file);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "capture file already exists",
    ))
}

fn temp_path(dir: &Path, kind: &str) -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "sharky-json-{}-{counter}-{nanos}-{kind}",
        std::process::id()
    ))
}

/// One stream's unlinked temp file, plus the identity recorded when it opened.
pub(crate) struct Capture {
    file: File,
    identity: Identity,
}

impl Capture {
    /// Creates one capture. The file and its descriptor are already off fd
    /// 0/1/2 and `FD_CLOEXEC` when this returns.
    pub(crate) fn create(kind: &str) -> io::Result<Self> {
        // `capture_file` already lifts the descriptor; repeating the step here
        // is what keeps the invariant owned by `Capture` rather than by a
        // caller that might one day open its own temp file.
        let file = lift_off_stdio(capture_file(kind)?)?;
        let identity = identity(file.as_raw_fd())?;
        Ok(Self { file, identity })
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// Identity of the file this capture owns, for the restore comparison.
    pub(crate) fn identity(&self) -> Identity {
        self.identity
    }

    /// Writes `bytes` at offset 0 without moving the shared file offset.
    ///
    /// The offset matters: after `dup2(capture_fd, 0)` the child shares this
    /// open file description, so it must already be positioned at the start.
    pub(crate) fn fill(&self, bytes: &[u8]) -> io::Result<()> {
        self.write_at_offset(0, bytes)
    }

    /// Appends `bytes` at the current end of the capture file without moving offset 0.
    pub(crate) fn append(&self, bytes: &[u8]) -> io::Result<()> {
        let offset = self.file.metadata()?.len();
        self.write_at_offset(offset, bytes)
    }

    fn write_at_offset(&self, start: u64, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            let count = self
                .file
                .write_at(&bytes[written..], start + written as u64)?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "capture fill: file accepted no bytes",
                ));
            }
            written += count;
        }
        Ok(())
    }

    /// Reads up to `limit` newly appended bytes starting from `offset`.
    pub(crate) fn read_from(&self, offset: u64, limit: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        while out.len() < limit {
            let want = (limit - out.len()).min(chunk.len());
            let count = self
                .file
                .read_at(&mut chunk[..want], offset + out.len() as u64)?;
            if count == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..count]);
        }
        Ok(out)
    }

    /// Reads at most `limit` bytes from the start and reports the real size.
    ///
    /// `truncated` compares the bytes actually returned with the size from
    /// `fstat`, so a caller can report truncation explicitly (§5.3). The read
    /// uses `pread`, leaving the shared offset at 0 for a `dup2` onto fd 0.
    ///
    /// The result is the request-end snapshot: `total` and `truncated` are
    /// measured after the read, so they describe the file at that moment. A
    /// writer that outlives the request (a background job still holding fd 1 or
    /// 2) can grow the file afterwards; that later output belongs to no request
    /// and is not merged into a later response. Nothing in the snapshot is lost
    /// silently: every byte past `limit` is counted in `total` and flagged by
    /// `truncated`.
    pub(crate) fn read_back(&self, limit: usize) -> io::Result<Captured> {
        let mut bytes = Vec::new();
        let mut chunk = [0u8; READ_CHUNK];
        while bytes.len() < limit {
            let want = (limit - bytes.len()).min(chunk.len());
            let count = self.file.read_at(&mut chunk[..want], bytes.len() as u64)?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }

        let total = self.file.metadata()?.len();
        let truncated = total > bytes.len() as u64;
        let content = match String::from_utf8(bytes) {
            Ok(text) => CaptureContent::Text(text),
            Err(err) => CaptureContent::Binary(err.into_bytes()),
        };
        Ok(Captured {
            content,
            total,
            truncated,
        })
    }
}

/// What one stream contained, after reading it back.
pub(crate) struct Captured {
    pub(crate) content: CaptureContent,
    pub(crate) total: u64,
    pub(crate) truncated: bool,
}

impl Captured {
    pub(crate) fn byte_len(&self) -> usize {
        match &self.content {
            CaptureContent::Text(text) => text.len(),
            CaptureContent::Binary(bytes) => bytes.len(),
        }
    }
}

/// Valid UTF-8 stays text; anything else keeps its exact bytes for base64.
pub(crate) enum CaptureContent {
    Text(String),
    Binary(Vec<u8>),
}

fn stat_fd(fd: RawFd) -> io::Result<nix::libc::stat> {
    let mut stat = std::mem::MaybeUninit::<nix::libc::stat>::uninit();
    // SAFETY: `fd` is only read from; `stat` points at a valid `stat` buffer.
    if unsafe { nix::libc::fstat(fd, stat.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded, so the buffer is initialised.
    Ok(unsafe { stat.assume_init() })
}

/// `(st_dev, st_ino)` for `fd`, or the errno as an `io::Error`.
// The casts are free on targets where `dev_t`/`ino_t` are already `u64`, but
// keep the public identity type stable where they are not.
#[allow(clippy::unnecessary_cast)]
pub(crate) fn identity(fd: RawFd) -> io::Result<Identity> {
    let stat = stat_fd(fd)?;
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

/// Whether `fd` is a regular file nobody can name any more.
///
/// The only regular file of that shape a session can hold is one of its own
/// captures (`exec 2>&1` in an earlier request points fd 2 at the stdout
/// capture), and it has no reader left. Pipes and FIFOs also report no links,
/// which is why the check asks for a regular file as well.
// Same rationale as `identity`: the casts are free where `mode_t` is `u32`
// already, but keep the constant types explicit for targets where it is not.
#[allow(clippy::unnecessary_cast)]
fn is_unlinked_regular(fd: RawFd) -> io::Result<bool> {
    let stat = stat_fd(fd)?;
    let kind = (stat.st_mode as u32) & (nix::libc::S_IFMT as u32);
    Ok(kind == nix::libc::S_IFREG as u32 && stat.st_nlink == 0)
}

/// `dup2` with errno mapping. `dup2` clears `FD_CLOEXEC` on the copy, which is
/// what the per-request descriptors on fd 0/1/2 need.
pub(crate) fn dup2_onto(source_fd: RawFd, target_fd: RawFd) -> io::Result<()> {
    if source_fd == target_fd {
        return Ok(());
    }
    // SAFETY: plain dup2(2); both descriptors are owned by the caller.
    if unsafe { nix::libc::dup2(source_fd, target_fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// What a standard descriptor pointed at when the guard took it over.
enum SavedStdio {
    /// Nothing: the descriptor was closed (`exec 1>&-` in a shell that has the
    /// close redirect), and "closed stays closed" means the restore puts it
    /// back that way.
    Closed,
    /// A `dup` of the descriptor, `FD_CLOEXEC`, at or above [`FD_FLOOR`], so
    /// the request cannot reach it by number and an `execve` drops it.
    Open {
        copy: RawFd,
        identity: Identity,
        /// False for a destination no reader can reach (see
        /// [`is_unlinked_regular`]): replaying into it would only grow an
        /// unlinked file for the life of the session.
        replayable: bool,
    },
}

impl SavedStdio {
    fn take(target_fd: RawFd) -> io::Result<Self> {
        let copy = match duplicate_above(target_fd) {
            Ok(copy) => copy,
            Err(err) if err.raw_os_error() == Some(nix::libc::EBADF) => return Ok(Self::Closed),
            Err(err) => return Err(err),
        };
        let probe = identity(copy).and_then(|identity| {
            is_unlinked_regular(copy).map(|unlinked| (identity, !unlinked))
        });
        match probe {
            Ok((identity, replayable)) => Ok(Self::Open {
                copy,
                identity,
                replayable,
            }),
            Err(err) => {
                let _ = close_descriptor(copy);
                Err(err)
            }
        }
    }

    fn close(self) {
        if let Self::Open { copy, .. } = self {
            let _ = close_descriptor(copy);
        }
    }
}

/// One descriptor the guard took over for the duration of a request.
struct Swapped {
    target_fd: RawFd,
    /// Capture the target currently points at; the restore only fires when the
    /// target still points here.
    ours: Identity,
    /// Where the request writes its output (`fd 1`/`fd 2`), so the captured
    /// bytes can be replayed into the destination the user had chosen.
    replay: bool,
    source_fd: RawFd,
    saved: SavedStdio,
}

/// Saves fd 0/1/2, swaps capture files onto them, and puts the saved
/// descriptors back on drop.
///
/// The driver creates one guard per request and swaps fd 0/1/2. Saving the
/// current descriptor is what makes a user redirect survive: `exec >log` in
/// request N leaves fd 1 on the log, request N+1 saves that log, runs with the
/// capture on fd 1 (so its output still reaches the response) and puts the log
/// back afterwards. The captured bytes are replayed into the saved destination
/// first, so the log reads exactly as it would in bash.
///
/// `null_fd` must stay open for the guard's whole lifetime (it is the swap
/// source for a request without a stdin payload, and its identity is how the
/// replay recognises "the session's own /dev/null"), and every [`Capture`] used
/// as a swap source must outlive the guard: the restore compares identities and
/// the replay reads from the capture descriptor.
pub(crate) struct StdioGuard {
    null_identity: Identity,
    swapped: Vec<Swapped>,
}

impl StdioGuard {
    /// `null_fd` is the session's `/dev/null`: its identity is how a saved
    /// destination is recognised as "nothing anybody reads", so the replay is
    /// skipped for the ordinary case.
    pub(crate) fn new(null_fd: RawFd) -> io::Result<Self> {
        Ok(Self {
            null_identity: identity(null_fd)?,
            swapped: Vec::new(),
        })
    }

    /// Saves whatever `target_fd` points at, then points it at `source_fd`.
    pub(crate) fn swap(&mut self, target_fd: RawFd, source_fd: RawFd) -> io::Result<()> {
        self.swap_onto(target_fd, source_fd, false)
    }

    /// [`swap`](Self::swap) for an output stream: when the request ends, the
    /// bytes written to the capture are also replayed into the destination the
    /// descriptor pointed at before the request.
    pub(crate) fn swap_output(&mut self, target_fd: RawFd, source_fd: RawFd) -> io::Result<()> {
        self.swap_onto(target_fd, source_fd, true)
    }

    /// Flushes Rust stdio, saves the target, then `dup2`s `source_fd` onto it.
    ///
    /// The flush comes first because `println!` is line-buffered: unflushed
    /// bytes would otherwise be written to whatever the descriptor points at
    /// after the swap.
    fn swap_onto(
        &mut self,
        target_fd: RawFd,
        source_fd: RawFd,
        replay: bool,
    ) -> io::Result<()> {
        if self.swapped.iter().any(|entry| entry.target_fd == target_fd) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "descriptor was already swapped by this guard",
            ));
        }
        flush_stdio();
        let saved = SavedStdio::take(target_fd)?;
        let ours = match identity(source_fd) {
            Ok(ours) => ours,
            Err(err) => {
                saved.close();
                return Err(err);
            }
        };
        if let Err(err) = dup2_onto(source_fd, target_fd) {
            saved.close();
            return Err(err);
        }
        self.swapped.push(Swapped {
            target_fd,
            ours,
            replay,
            source_fd,
            saved,
        });
        Ok(())
    }

    /// Flushes Rust stdio, replays output, and restores every descriptor the
    /// request did not re-point itself.
    ///
    /// A target that no longer carries our capture was re-pointed (`exec >log`)
    /// or closed by the request; both are the user's decision and are left
    /// alone. Calling this twice is harmless; `Drop` calls it as a backstop.
    pub(crate) fn restore(&mut self) -> io::Result<()> {
        if self.swapped.is_empty() {
            return Ok(());
        }
        flush_stdio();
        let mut first_error = None;
        let null_identity = self.null_identity;
        for entry in std::mem::take(&mut self.swapped).into_iter().rev() {
            if let Err(err) = restore_entry(entry, null_identity) {
                first_error.get_or_insert(err);
            }
        }
        match first_error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }
}

fn restore_entry(entry: Swapped, null_identity: Identity) -> io::Result<()> {
    let Swapped {
        target_fd,
        ours,
        replay,
        source_fd,
        saved,
    } = entry;
    let mut first_error = None;

    // Before the descriptor moves back: the request's output belongs in the
    // destination the user chose, at that file's own offset, in the order bash
    // would have written it. The session's own /dev/null (and a destination no
    // reader can reach) is not a destination anybody reads, so it is skipped.
    // A destination that went away or is not writable (`EPIPE`, `EBADF` from
    // `exec 1<file`, `EACCES`, `ENOSPC`) is not a session failure: only a read
    // failure on our own capture breaks result integrity.
    if replay
        && let SavedStdio::Open {
            copy,
            identity,
            replayable,
        } = &saved
        && *replayable
        && *identity != null_identity
        && let Err(err) = replay_capture(source_fd, *copy)
    {
        first_error.get_or_insert(err);
    }

    if matches!(identity(target_fd), Ok(current) if current == ours) {
        let restored = match &saved {
            SavedStdio::Closed => close_descriptor(target_fd),
            SavedStdio::Open { copy, .. } => dup2_onto(*copy, target_fd),
        };
        if let Err(err) = restored {
            first_error.get_or_insert(err);
        }
    }

    saved.close();
    match first_error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

impl Drop for StdioGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Closes `fd`, mapping the errno. Only used on descriptors the caller owns.
fn close_descriptor(fd: RawFd) -> io::Result<()> {
    // SAFETY: the caller owns `fd` and closes it exactly once.
    if unsafe { nix::libc::close(fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Copies everything `source_fd` holds into `dest_fd` without moving the
/// source's offset, so the response read still starts at the beginning.
///
/// `dest_fd` is written sequentially through [`write_safely`]: it shares its
/// open file description with the descriptor the user redirected, so the bytes
/// land after whatever that descriptor already wrote. Write failures on
/// `dest_fd` (a read-only `exec 1<file`, a closed pipe reader, a full disk)
/// stop the replay without failing the session: the response itself still
/// holds the capture.
fn replay_capture(source_fd: RawFd, dest_fd: RawFd) -> io::Result<()> {
    let mut offset = 0u64;
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let count = pread_some(source_fd, &mut chunk, offset)?;
        if count == 0 {
            return Ok(());
        }
        if write_safely(dest_fd, &chunk[..count]).is_err() {
            return Ok(());
        }
        offset += count as u64;
    }
}

fn pread_some(fd: RawFd, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    loop {
        // SAFETY: `buffer` is valid for writes of its own length.
        let count = unsafe {
            nix::libc::pread(
                fd,
                buffer.as_mut_ptr() as *mut _,
                buffer.len(),
                offset as nix::libc::off_t,
            )
        };
        if count >= 0 {
            return Ok(count as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

fn write_all(fd: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: `bytes` is valid for reads of its own length.
        let count = unsafe { nix::libc::write(fd, bytes.as_ptr() as *const _, bytes.len()) };
        if count > 0 {
            bytes = &bytes[count as usize..];
            continue;
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "descriptor accepted no bytes",
            ));
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    Ok(())
}

/// Writes every byte of `bytes` to `fd` with `SIGPIPE` blocked.
///
/// The shell restores the default `SIGPIPE` disposition at startup, so a
/// reader that went away would otherwise kill the agent mid-write instead of
/// reporting `EPIPE`. Used for the protocol stream (design §8.3) and for the
/// replay into a saved user destination, which can be a pipe.
pub(crate) fn write_safely(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut mask = std::mem::MaybeUninit::<nix::libc::sigset_t>::uninit();
    let mut previous = std::mem::MaybeUninit::<nix::libc::sigset_t>::uninit();
    // SAFETY: both sigset_t buffers are uninitialised storage of the right
    // type; `sigemptyset`/`sigaddset` initialise `mask`, and `sigprocmask`
    // writes the old mask into `previous`.
    unsafe {
        nix::libc::sigemptyset(mask.as_mut_ptr());
        nix::libc::sigaddset(mask.as_mut_ptr(), nix::libc::SIGPIPE);
        if nix::libc::sigprocmask(nix::libc::SIG_BLOCK, mask.as_ptr(), previous.as_mut_ptr()) < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let result = write_all(fd, bytes);
    if result.as_ref().err().and_then(io::Error::raw_os_error) == Some(nix::libc::EPIPE) {
        consume_sigpipe();
    }

    // SAFETY: `previous` was filled by the `sigprocmask` above.
    unsafe {
        nix::libc::sigprocmask(
            nix::libc::SIG_SETMASK,
            previous.as_ptr(),
            std::ptr::null_mut(),
        );
    }
    result
}

/// Takes a `SIGPIPE` that the blocked write in [`write_safely`] made pending,
/// so it is not delivered when the mask is restored.
fn consume_sigpipe() {
    let mut mask = std::mem::MaybeUninit::<nix::libc::sigset_t>::uninit();
    let timeout = nix::libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `mask` is initialised by the two calls below and the timeout
    // points at a valid `timespec`; `sigtimedwait` only reports or consumes a
    // pending signal, and `EAGAIN` (nothing pending) is ignored.
    unsafe {
        nix::libc::sigemptyset(mask.as_mut_ptr());
        nix::libc::sigaddset(mask.as_mut_ptr(), nix::libc::SIGPIPE);
        nix::libc::sigtimedwait(mask.as_ptr(), std::ptr::null_mut(), &timeout);
    }
}

/// Flushes the Rust stdio handles so buffered bytes cannot land on the next fd.
pub(crate) fn flush_stdio() {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
}

/// Advisory OS file lock shared across independent Sharky agent processes.
pub(crate) struct CrossSessionLock {
    file: File,
}

impl CrossSessionLock {
    pub(crate) fn try_acquire(name: &str) -> io::Result<Option<Self>> {
        let encoded = encode_lock_name(name);
        for dir in temp_dirs() {
            let path = dir.join(format!("sharky-lock-{encoded}.lock"));
            let file = match OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_CLOEXEC)
                .open(&path)
            {
                Ok(file) => lift_off_stdio(file)?,
                Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            loop {
                // SAFETY: `file` owns an open fd in this process.
                let rc = unsafe {
                    nix::libc::flock(
                        file.as_raw_fd(),
                        nix::libc::LOCK_EX | nix::libc::LOCK_NB,
                    )
                };
                if rc == 0 {
                    return Ok(Some(Self { file }));
                }
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.raw_os_error() == Some(nix::libc::EWOULDBLOCK)
                    || err.raw_os_error() == Some(nix::libc::EAGAIN)
                {
                    return Ok(None);
                }
                return Err(err);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no usable temporary directory for cross-session lock",
        ))
    }

    pub(crate) fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Drop for CrossSessionLock {
    fn drop(&mut self) {
        // Explicit LOCK_UN ensures releasing even if a forked child inherited the fd.
        // SAFETY: `self.file` is valid for the lifetime of `&mut self`.
        unsafe {
            nix::libc::flock(self.file.as_raw_fd(), nix::libc::LOCK_UN);
        }
    }
}

fn encode_lock_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            ':' => out.push_str("_c_"),
            '_' => out.push_str("__"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_file_reports_an_error_when_every_candidate_fails() {
        // Missing directories make every candidate fail with NotFound, the
        // same error the parent-side path reports when no fallback works.
        // Injecting the list keeps that real branch covered without mutating
        // the process-wide TMPDIR.
        let first = Path::new("/tmp/opencode/sharky-capture-missing-one");
        let second = Path::new("/tmp/opencode/sharky-capture-missing-two");
        assert!(!first.exists() && !second.exists());
        let error = capture_file_in("unit", [first, second])
            .expect_err("capture must fail when every candidate is unusable");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(error.to_string(), "no usable temporary directory");
    }
}
