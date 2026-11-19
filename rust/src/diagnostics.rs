//! Read-only diagnostics subcommands.
//!
//! `sharky status` prints facts about the shell and its resolved paths. It is
//! dispatched from `cli::run` before the shell is constructed: it never loads
//! configuration, applies `pearl`, evaluates profiles, or creates or modifies
//! files. It also never prints environment values or configuration values:
//! only paths, statuses and fixed flag names are reported.

use std::io::Write;
use std::path::PathBuf;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Dispatch a diagnostics subcommand. `None` means the invocation is not a
/// diagnostics request and normal CLI parsing continues.
pub(crate) fn try_run(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("status") {
        return None;
    }
    if let Some(option) = args[1..].iter().find(|arg| arg.starts_with('-')) {
        eprintln!("sharky: status: unexpected option '{option}'");
        return Some(2);
    }
    if let Some(argument) = args.get(1) {
        eprintln!("sharky: status: unexpected argument '{argument}'");
        return Some(2);
    }
    Some(print_status())
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

fn existence(path: &std::path::Path) -> &'static str {
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

/// Write plain text and flush it: `main` uses `process::exit`, which does not
/// run destructors, so buffered bytes after the last newline must be forced out.
fn print_text(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}
