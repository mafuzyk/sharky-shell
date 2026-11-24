use std::collections::HashMap;
use std::ffi::CString;
use std::io::Read;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::libc::_exit;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

use crate::shell::Shell;

pub const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// `hash` table. Bash seeds it only from PATH lookups of external commands,
/// so builtins and names containing a slash never appear here. It belongs to
/// the shell: another Shell instance starts with an empty table, and changing
/// PATH invalidates every remembered location. So that `command -p` cannot
/// poison it, private lookups take a separate cache.
pub type CommandCache = HashMap<String, PathBuf>;

/// Snapshot of the current hash table, sorted by command name.
pub fn hashed(cache: &CommandCache) -> Vec<(String, PathBuf)> {
    let mut entries: Vec<_> = cache
        .iter()
        .map(|(name, path)| (name.clone(), path.clone()))
        .collect();
    entries.sort();
    entries
}

/// `hash -t NAME`: the cached path, if any.
pub fn hashed_path(cache: &CommandCache, name: &str) -> Option<PathBuf> {
    cache.get(name).cloned()
}

/// `hash -p PATH NAME` and `hash NAME...`.
pub fn hash_add(cache: &mut CommandCache, name: &str, path: &Path) {
    cache.insert(name.to_string(), path.to_path_buf());
}

pub fn hash_remove(cache: &mut CommandCache, name: &str) -> bool {
    cache.remove(name).is_some()
}

/// Resolves a command through PATH and remembers the hit in the shell's cache.
pub fn resolve(name: &str, shell: &mut Shell) -> Option<PathBuf> {
    let path = path_of(shell).to_string();
    resolve_with_cache(name, &path, &mut shell.command_cache)
}

fn resolve_with_cache(
    name: &str,
    path: &str,
    cache: &mut CommandCache,
) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }

    if let Some(cached) = cache.get(name) {
        return Some(cached.clone());
    }
    let resolved = search_path(name, path);
    if let Some(found) = &resolved
        && found.is_file()
    {
        hash_add(cache, name, found);
    }
    resolved
}

/// Resolves a command against an explicit `path` without touching the shell's
/// hash table. `command -p` and other private lookups must not poison it.
/// A fresh search also means no entry resolved under a different path set can
/// be reused here: every call searches `path` from scratch.
pub fn resolve_in(name: &str, path: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }

    search_path(name, path)
}

/// Command lookup for `command -v`/`-V` and `type`. Bash treats a name
/// containing a slash as a command only when it names an executable file, and
/// never searches PATH for it.
pub fn lookup(name: &str, shell: &Shell) -> Option<PathBuf> {
    lookup_in(name, path_of(shell))
}

pub fn lookup_in(name: &str, path: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = Path::new(name);
        return is_executable(path).then(|| path.to_path_buf());
    }

    search_path(name, path)
}

fn search_path(name: &str, path: &str) -> Option<PathBuf> {
    let mut denied = None;

    for dir in path.split(':') {
        let dir = if dir.is_empty() {
            Path::new(".")
        } else {
            Path::new(dir)
        };
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
        // Bash remembers the first existing regular file it may not execute
        // and reports 126 for it when no runnable candidate turns up; a
        // directory or a dangling symlink is ignored entirely.
        if denied.is_none() && candidate.is_file() {
            denied = Some(candidate);
        }
    }

    denied
}

fn path_of(shell: &Shell) -> &str {
    shell
        .env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or(DEFAULT_PATH)
}

pub fn exec_now(path: &Path, argv: &[String], env: &[(String, String)]) -> Result<(), Errno> {
    exec_env(path, argv, env, true)
}

/// The `exec` builtin path: bash does not rewrite `_` when the shell replaces
/// itself, so the environment is passed exactly as built.
pub fn exec_now_plain(
    path: &Path,
    argv: &[String],
    env: &[(String, String)],
) -> Result<(), Errno> {
    exec_env(path, argv, env, false)
}

fn exec_env(
    path: &Path,
    argv: &[String],
    env: &[(String, String)],
    underscore: bool,
) -> Result<(), Errno> {
    let command_line: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).unwrap_or_default())
        .collect();
    // Bash gives a normal child `_` the resolved path of the command it is
    // about to run, overriding whatever value the shell's `_` holds.
    let mut envp: Vec<CString> = env
        .iter()
        .filter(|(key, _)| !(underscore && key == "_"))
        .filter_map(|(key, value)| CString::new(format!("{key}={value}")).ok())
        .collect();
    if underscore {
        let mut entry = Vec::from(&b"_="[..]);
        entry.extend_from_slice(path.as_os_str().as_encoded_bytes());
        if let Ok(entry) = CString::new(entry) {
            envp.push(entry);
        }
    }

    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| Errno::EINVAL)?;

    match execve(&path_c, &command_line, &envp) {
        Ok(_) => Ok(()),
        Err(Errno::ENOEXEC) => exec_as_script(path, argv, &envp),
        Err(err) => Err(err),
    }
}

/// Bash's ENOEXEC fallback: the kernel did not recognise the file, so the shell
/// runs it as a script with its own interpreter, with `$0` set to the resolved
/// path and the arguments forwarded. A binary file (see [`is_binary`]) is
/// refused with 126 instead of being fed to a shell.
fn exec_as_script(path: &Path, argv: &[String], envp: &[CString]) -> Result<(), Errno> {
    if is_binary(path)? {
        return Err(Errno::ENOEXEC);
    }

    let shell = std::env::current_exe().map_err(|_| Errno::ENOEXEC)?;
    let shell_c = CString::new(shell.as_os_str().as_encoded_bytes()).map_err(|_| Errno::ENOEXEC)?;
    let mut script_argv = Vec::with_capacity(argv.len() + 1);
    script_argv.push(shell_c.clone());
    script_argv.push(CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| Errno::EINVAL)?);
    for arg in argv.iter().skip(1) {
        script_argv.push(CString::new(arg.as_bytes()).map_err(|_| Errno::EINVAL)?);
    }

    execve(&shell_c, &script_argv, envp).map(|_| ())
}

/// Bash's binary test for the ENOEXEC fallback: a NUL byte in the first block,
/// or the ELF magic, marks a file the shell must not read as a script. Other
/// control bytes are text to bash and reach the parser.
fn is_binary(path: &Path) -> Result<bool, Errno> {
    let mut file = std::fs::File::open(path).map_err(io_errno)?;
    let mut sample = [0u8; 80];
    let mut filled = 0;
    while filled < sample.len() {
        match file.read(&mut sample[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(io_errno(err)),
        }
    }
    let sample = &sample[..filled];
    Ok(sample.contains(&0) || sample.starts_with(b"\x7fELF"))
}

fn io_errno(err: std::io::Error) -> Errno {
    err.raw_os_error()
        .map(Errno::from_raw)
        .unwrap_or(Errno::EIO)
}

/// Bash reports a path that does not exist as 127 (command not found) and a
/// path that exists but cannot be executed as 126 (not executable). Both come
/// from the same `execve` failure, so the errno decides.
pub fn exec_failure_status(err: Errno) -> i32 {
    match err {
        Errno::ENOENT => 127,
        _ => 126,
    }
}

pub fn exec_replace(path: &Path, argv: &[String], env: &[(String, String)]) -> ! {
    if let Err(err) = exec_now(path, argv, env) {
        eprintln!("sharky: {}: {err}", path.display());
        unsafe { _exit(exec_failure_status(err)) }
    }
    unsafe { _exit(126) }
}

pub fn wait(pid: Pid) -> i32 {
    match wait_checked(pid) {
        Ok(status) => status,
        Err(err) => {
            eprintln!("sharky: wait: {err}");
            1
        }
    }
}

/// Like [`wait`], but keeps `ECHILD` distinct so the `wait` builtin can report
/// a pid that is not a child with bash's 127 and diagnostic. Internal callers
/// use [`wait`], which turns the error into a status of 1.
pub fn wait_checked(pid: Pid) -> Result<i32, Errno> {
    loop {
        match waitpid(pid, None) {
            Ok(WaitStatus::Exited(_, status)) => return Ok(status),
            Ok(WaitStatus::Signaled(_, signal, _)) => return Ok(128 + signal as i32),
            Ok(_) => return Ok(1),
            // A fatal signal interrupts the wait; the shell is leaving, so
            // report its status instead of blocking again behind a restarted
            // wait. Other interruptions keep waiting.
            Err(Errno::EINTR) => {
                if let Some(signal) = crate::signal::fatal_signal() {
                    return Ok(128 + signal);
                }
                continue;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Waits for a pid and reports a job-control stop instead of blocking on it.
/// Plain `waitpid` never returns for a stopped child, so `wait %job` on a
/// SIGSTOPed job would hang forever. `None` means the pid is no longer a child
/// of this process, which happens once another wait consumed its status: a
/// waiter must skip it instead of inventing a failure.
pub fn wait_stopped(pid: Pid) -> Option<WaitStatus> {
    wait_member(pid)
}

pub fn wait_traced(pid: Pid) -> Option<WaitStatus> {
    wait_member(pid)
}

/// Waits for one child and reports its final status, or `None` when the pid is
/// no longer a child of this process (see [`wait_stopped`]).
///
/// In session mode this pumps the agent supervisor between polls, so it is the
/// session's single blocking wait. The parallel executor uses it for one of its
/// own task leaders and never asks for `-1`, which is what keeps this module
/// the only `waitpid` consumer.
pub(crate) fn wait_member(pid: Pid) -> Option<WaitStatus> {
    // Session mode only. Outside an agent session this is one relaxed atomic
    // load and the blocking `waitpid` below is byte-identical to before.
    if crate::agent::active() {
        return wait_member_session(pid);
    }
    loop {
        match waitpid(pid, Some(WaitPidFlag::WUNTRACED)) {
            Ok(status) => return Some(status),
            // Abandon the wait when the shell itself was told to die: the
            // caller unwinds, runs the EXIT action and exits 128+signal.
            Err(Errno::EINTR) => {
                if crate::signal::fatal_signal().is_some() {
                    return None;
                }
                continue;
            }
            Err(Errno::ECHILD) => return None,
            Err(err) => {
                eprintln!("sharky: wait: {err}");
                return None;
            }
        }
    }
}

/// The S3 wait hook of design §8.1: the same single-threaded reap ownership,
/// but between non-blocking polls it runs the supervisor pump, so `cancel`,
/// `timeout_ms` and their TERM→KILL escalation take effect while a command is
/// running. `WCONTINUED` reports a resumed stopped child, which is not a final
/// status: the loop keeps waiting exactly as the blocking path would.
fn wait_member_session(pid: Pid) -> Option<WaitStatus> {
    loop {
        if crate::signal::fatal_signal().is_some() {
            return None;
        }
        crate::agent::service();
        if crate::signal::fatal_signal().is_some() {
            return None;
        }
        match waitpid(
            pid,
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED),
        ) {
            Ok(WaitStatus::StillAlive) => {
                if crate::signal::fatal_signal().is_some() {
                    return None;
                }
                crate::agent::wait_poll();
            }
            Ok(WaitStatus::Continued(_)) => continue,
            Ok(status) => return Some(status),
            Err(Errno::EINTR) => {
                if crate::signal::fatal_signal().is_some() {
                    return None;
                }
                continue;
            }
            Err(Errno::ECHILD) => return None,
            Err(err) => {
                eprintln!("sharky: wait: {err}");
                return None;
            }
        }
    }
}

/// A non-blocking poll of one child. `None` means the status was already
/// consumed elsewhere, so a reaper must treat the process as gone.
pub fn wait_scan(pid: Pid) -> Option<WaitStatus> {
    loop {
        match waitpid(
            pid,
            Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED),
        ) {
            Ok(status) => return Some(status),
            Err(Errno::EINTR) => continue,
            Err(Errno::ECHILD) => return None,
            Err(_) => return Some(WaitStatus::StillAlive),
        }
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    // Bash asks the kernel with the effective IDs: a file with an execute bit
    // only for another user class is not a candidate for this process.
    let Ok(path) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `path` is a valid NUL-terminated path for the duration of the call.
    unsafe {
        nix::libc::faccessat(
            nix::libc::AT_FDCWD,
            path.as_ptr(),
            nix::libc::X_OK,
            nix::libc::AT_EACCESS,
        ) == 0
    }
}
