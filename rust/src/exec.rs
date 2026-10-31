use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use nix::libc::_exit;
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, execve, fork};

use crate::shell::Shell;

const DEFAULT_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

pub fn run(argv: &[String], shell: &Shell) -> i32 {
    let Some(path) = resolve(&argv[0], shell) else {
        eprintln!("sharky: command not found: {}", argv[0]);
        return 127;
    };

    let argv: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()).unwrap_or_default())
        .collect();
    let envp: Vec<CString> = shell
        .env
        .iter()
        .filter_map(|(key, value)| CString::new(format!("{key}={value}")).ok())
        .collect();
    let path = match CString::new(path.as_os_str().as_encoded_bytes()) {
        Ok(path) => path,
        Err(_) => return 126,
    };

    // SAFETY: the child only runs execve and _exit, both async-signal-safe.
    match unsafe { fork() } {
        Ok(ForkResult::Parent { child }) => match waitpid(child, None) {
            Ok(WaitStatus::Exited(_, status)) => status,
            Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
            Ok(_) => 1,
            Err(err) => {
                eprintln!("sharky: wait: {err}");
                1
            }
        },
        Ok(ForkResult::Child) => {
            let Err(err) = execve(&path, &argv, &envp);
            // shortcut: allocates in the forked child; safe because sharky is single-threaded
            eprintln!("sharky: {}: {err}", path.to_string_lossy());
            // SAFETY: after a failed exec the child must not unwind or run destructors.
            unsafe { _exit(126) };
        }
        Err(err) => {
            eprintln!("sharky: fork: {err}");
            1
        }
    }
}

fn resolve(name: &str, shell: &Shell) -> Option<PathBuf> {
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

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
