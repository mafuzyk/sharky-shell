use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;
use nix::libc::_exit;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

use crate::shell::Shell;

pub const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn resolve(name: &str, shell: &Shell) -> Option<PathBuf> {
    resolve_in(name, path_of(shell))
}

/// Resolves `name` against an explicit `path`, so `command -p` can search the
/// default path without a temporary environment change. A name containing a
/// slash is kept as written even when it is not executable: execution reports
/// the failing `execve` error, which is what bash does.
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
    path.split(':')
        .map(|dir| {
            if dir.is_empty() {
                Path::new(".")
            } else {
                Path::new(dir)
            }
        })
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
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
    let argv: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).unwrap_or_default())
        .collect();
    let envp: Vec<CString> = env
        .iter()
        .filter_map(|(key, value)| CString::new(format!("{key}={value}")).ok())
        .collect();

    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| Errno::EINVAL)?;

    execve(&path_c, &argv, &envp).map(|_| ())
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
    match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, status)) => status,
        Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
        Ok(_) => 1,
        Err(err) => {
            eprintln!("sharky: wait: {err}");
            1
        }
    }
}

pub fn wait_traced(pid: Pid) -> WaitStatus {
    waitpid(pid, Some(WaitPidFlag::WUNTRACED))
        .unwrap_or(WaitStatus::Exited(pid, 1))
}

pub fn wait_scan(pid: Pid) -> WaitStatus {
    waitpid(pid, Some(WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED))
        .unwrap_or(WaitStatus::StillAlive)
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
