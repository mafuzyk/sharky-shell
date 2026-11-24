//! Shared helpers for the integration tests.
//!
//! On NixOS the FHS paths `/bin/echo` and `/bin/printf` do not exist and the
//! coreutils (including `sleep`) live in `/run/current-system/sw/bin`. Resolve
//! executables through a PATH that includes that directory plus the inherited
//! PATH so tests do not depend on a particular filesystem layout, while still
//! returning an unambiguous absolute path for commands such as `exec`.

// Only the agent session test target consumes this helper today.
#[allow(dead_code)]
pub mod agent;

use std::ffi::OsString;
use std::path::PathBuf;

/// Directories searched for test executables, NixOS system path first.
pub fn tool_search_path() -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::from("/run/current-system/sw/bin")];
    if let Some(inherited) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&inherited));
    }
    paths
}

/// A `PATH` value that resolves coreutils on both NixOS and FHS hosts.
#[allow(dead_code)]
pub fn tool_path() -> OsString {
    std::env::join_paths(tool_search_path()).expect("PATH components must not contain ':'")
}

/// Absolute path to `name`, with a clear panic when it is genuinely absent.
#[allow(dead_code)]
pub fn resolve(name: &str) -> PathBuf {
    tool_search_path()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("required test executable {name:?} not found on PATH"))
}
