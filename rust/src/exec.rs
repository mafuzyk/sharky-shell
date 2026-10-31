use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::libc::_exit;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, execve};

use crate::shell::Shell;

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn resolve(name: &str, shell: &Shell) -> Option<PathBuf> {
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }

    let path = shell
        .env
        .iter()
        .find(|(key, _)| key == "PATH")
        .map(|(_, value)| value.as_str())
        .unwrap_or(DEFAULT_PATH);

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

pub fn exec_replace(path: &Path, argv: &[String], env: &[(String, String)]) -> ! {
    let argv: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).unwrap_or_default())
        .collect();
    let envp: Vec<CString> = env
        .iter()
        .filter_map(|(key, value)| CString::new(format!("{key}={value}")).ok())
        .collect();

    let Ok(path_c) = CString::new(path.as_os_str().as_encoded_bytes()) else {
        eprintln!("sharky: {}: invalid path", path.display());
        unsafe { _exit(126) }
    };

    let Err(err) = execve(&path_c, &argv, &envp);
    eprintln!("sharky: {}: {err}", path.display());
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
