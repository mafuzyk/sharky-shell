//! Read-only diagnostics subcommands.
//!
//! `sharky status` prints facts about the shell and its resolved paths.
//! `sharky doctor` runs local checks and reports their outcome. Both are
//! dispatched from `cli::run` before the shell is constructed: they never load
//! configuration, apply `pearl`, evaluate profiles, or create or modify files.
//! They also never print environment values or configuration values: only
//! paths, counts, statuses and fixed tool names are reported.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sharky_config as config;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Tools the shell expects to find on `PATH` for ordinary interactive use.
/// They are only inspected through `exec::resolve`; nothing is executed.
const REQUIRED_TOOLS: &[&str] = &["sh", "ls", "cat"];

/// Dispatch a diagnostics subcommand. `None` means the invocation is not a
/// diagnostics request and normal CLI parsing continues.
pub(crate) fn try_run(args: &[String]) -> Option<i32> {
    let name = match args.first().map(String::as_str) {
        Some("status") => "status",
        Some("doctor") => "doctor",
        _ => return None,
    };
    if let Some(option) = args[1..].iter().find(|arg| arg.starts_with('-')) {
        eprintln!("sharky: {name}: unexpected option '{option}'");
        return Some(2);
    }
    if let Some(argument) = args.get(1) {
        eprintln!("sharky: {name}: unexpected argument '{argument}'");
        return Some(2);
    }
    Some(if name == "status" {
        print_status()
    } else {
        run_doctor()
    })
}

/// Mirror the pure helper used by `Shell::load_config`. Kept here so diagnostics
/// can resolve paths without touching the shell or its configuration loader.
fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SHARKY_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("sharky"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/sharky"))
}

/// Compile-time feature flags that are active without reading configuration.
/// The crate declares none today; this is the one place to extend when opt-in
/// flags are introduced.
fn active_feature_flags() -> &'static [&'static str] {
    &[]
}

fn existence(path: &Path) -> &'static str {
    if path.exists() {
        "exists"
    } else {
        "missing"
    }
}

fn print_status() -> i32 {
    let mut out = format!("sharky {VERSION}\n");
    match config_dir() {
        Some(dir) => {
            let config = dir.join("config.sharky");
            let theme = dir.join("theme.sharky");
            out.push_str(&format!(
                "config: {} ({})\n",
                config.display(),
                existence(&config)
            ));
            out.push_str(&format!(
                "theme: {} ({})\n",
                theme.display(),
                existence(&theme)
            ));
        }
        None => out.push_str("config: (none)\ntheme: (none)\n"),
    }

    let history = crate::history::History::default_path();
    if history.as_os_str().is_empty() {
        out.push_str("history: (private)\n");
    } else {
        out.push_str(&format!(
            "history: {} ({})\n",
            history.display(),
            existence(&history)
        ));
    }

    let flags = active_feature_flags();
    if flags.is_empty() {
        out.push_str("features: none\n");
    } else {
        out.push_str(&format!("features: {}\n", flags.join(", ")));
    }
    print_text(&out);
    0
}

struct Check {
    name: &'static str,
    ok: bool,
    reason: String,
}

impl Check {
    fn ok(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            reason: reason.into(),
        }
    }

    fn fail(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            reason: reason.into(),
        }
    }

    /// Print one line and return whether it passed.
    fn report(self) -> bool {
        let marker = if self.ok { "OK" } else { "FAIL" };
        println!("{marker:<4} {}: {}", self.name, self.reason);
        self.ok
    }
}

fn check_stream(name: &'static str, fd: std::os::fd::RawFd) -> Check {
    // SAFETY: reads this process's own descriptor table; F_GETFD has no side
    // effects and fails only when the descriptor is closed.
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) } < 0 {
        return Check::fail(name, "descriptor is closed");
    }
    let state = if crate::term::is_tty(fd) {
        "terminal"
    } else {
        "not a terminal"
    };
    // A missing TTY is a fact, not a fault: doctor is also meant for pipes and
    // redirected output. The check fails only when the descriptor is unusable.
    Check::ok(name, state)
}

/// Report an existing file that `config::load` would silently skip because it
/// cannot be read. Absent files are fine; no file is opened for writing.
fn unreadable(path: &Path) -> Option<&'static str> {
    match std::fs::metadata(path) {
        Ok(meta) if !meta.is_file() => Some("not a regular file"),
        Ok(meta) if meta.permissions().mode() & 0o444 == 0 => Some("no read permission"),
        Ok(_) => None,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some("cannot inspect"),
    }
}

fn check_config(dir: Option<&Path>) -> Check {
    let Some(dir) = dir else {
        return Check::ok("config", "no config directory resolved; defaults in use");
    };
    if let Some(problem) = unreadable(&dir.join("config.sharky")) {
        return Check::fail(
            "config",
            format!("config.sharky: {problem} (load would skip it)"),
        );
    }
    match config::load(dir) {
        Ok(_) => Check::ok("config", "parsed by config::load"),
        // The message may quote a configuration value, so report only its line.
        Err(err) => Check::fail(
            "config",
            format!("parse error in config.sharky or theme.sharky at line {}", err.line),
        ),
    }
}

fn check_theme(dir: Option<&Path>) -> Check {
    let Some(dir) = dir else {
        return Check::ok("theme", "no config directory resolved; defaults in use");
    };
    let path = dir.join("theme.sharky");
    if let Some(problem) = unreadable(&path) {
        return Check::fail("theme", format!("theme.sharky: {problem}"));
    }
    let Ok(source) = std::fs::read_to_string(&path) else {
        return Check::ok("theme", "absent; defaults in use");
    };
    match config::parse_theme(&source, "theme.sharky") {
        Ok(_) => Check::ok("theme", "parsed"),
        // The message may quote a theme value, so report only its line.
        Err(err) => Check::fail("theme", format!("parse error at line {}", err.line)),
    }
}

fn check_history() -> Check {
    let path = crate::history::History::default_path();
    if path.as_os_str().is_empty() {
        return Check::ok("history", "private mode; no history path");
    }
    match std::fs::metadata(&path) {
        Ok(meta) => {
            if !meta.is_file() {
                return Check::fail("history", "path is not a regular file");
            }
            let mode = meta.permissions().mode();
            if mode & 0o002 != 0 {
                Check::fail("history", "world-writable file")
            } else if mode & 0o200 == 0 {
                Check::fail("history", "not writable by owner")
            } else {
                Check::ok("history", "writable with safe permissions")
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            match std::fs::metadata(parent) {
                Ok(meta)
                    if meta.is_dir() && meta.permissions().mode() & 0o300 == 0o300 =>
                {
                    Check::ok("history", "absent; parent directory is writable")
                }
                Ok(_) => Check::fail("history", "absent; parent directory is not writable"),
                Err(_) => Check::fail("history", "absent; parent directory is missing"),
            }
        }
        Err(_) => Check::fail("history", "cannot inspect"),
    }
}

fn check_tools() -> Check {
    let shell = crate::shell::Shell::new();
    let missing = REQUIRED_TOOLS
        .iter()
        .copied()
        .filter(|name| crate::exec::resolve(name, &shell).is_none())
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Check::ok("tools", format!("{} resolvable on PATH", REQUIRED_TOOLS.join(", ")))
    } else {
        Check::fail("tools", format!("not on PATH: {}", missing.join(", ")))
    }
}

fn run_doctor() -> i32 {
    let dir = config_dir();
    let checks = [
        check_stream("stdin", 0),
        check_stream("stdout", 1),
        check_config(dir.as_deref()),
        check_theme(dir.as_deref()),
        check_history(),
        check_tools(),
    ];
    let mut ok = true;
    for check in checks {
        // Non-short-circuiting: every check must print its line.
        ok &= check.report();
    }
    i32::from(!ok)
}

/// Write plain text and flush it: `main` uses `process::exit`, which does not
/// run destructors, so buffered bytes after the last newline must be forced out.
fn print_text(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}
