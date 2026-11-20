use std::path::{Path, PathBuf};

use crate::shell::Shell;

/// Startup files that only other shells read. A login shell executes the bash
/// login chain instead, so these are not scanned for `PATH` edits here.
const FILES: &[&str] = &[
    ".bashrc",
    ".zshrc",
    ".zprofile",
    ".config/fish/config.fish",
];

/// User login profiles, in the order bash tries them. Only the first one that
/// exists is executed.
const LOGIN_FILES: &[&str] = &[".bash_profile", ".bash_login", ".profile"];

/// Runs the login startup files, executing each one the way bash does.
///
/// The order mirrors bash's login sequence, with the sharky-specific files in
/// their own layer so they are never discarded by the standard files:
///
/// 1. `/etc/profile`, which on NixOS also pulls in `set-environment`,
///    `/etc/profile.local` and `/etc/bashrc`.
/// 2. `/etc/sharky/profile.sharky`, the system-wide sharky hook. It runs after
///    `/etc/profile` because NixOS assigns `PATH` there and would otherwise
///    drop anything the hook prepends.
/// 3. The first existing of `~/.bash_profile`, `~/.bash_login` and `~/.profile`.
/// 4. `~/.config/sharky/profile.sharky`, so a user's sharky settings win.
///
/// A file that fails to parse is reported with its path and line and the login
/// continues with the next file, matching bash's survival of a broken profile.
/// An `exit` requested by a file ends the login sequence at once: bash never
/// reads a later startup file after the shell has been asked to leave.
pub fn login(shell: &mut Shell) {
    source(shell, Path::new("/etc/profile"));
    if shell.exit_requested.is_some() {
        return;
    }
    source(shell, Path::new("/etc/sharky/profile.sharky"));
    if shell.exit_requested.is_some() {
        return;
    }

    let home = PathBuf::from(shell.var("HOME"));
    if home.as_os_str().is_empty() {
        return;
    }
    if let Some(path) = LOGIN_FILES
        .iter()
        .map(|name| home.join(name))
        .find(|path| path.is_file())
    {
        source(shell, &path);
        if shell.exit_requested.is_some() {
            return;
        }
    }
    source(shell, &home.join(".config/sharky/profile.sharky"));
}

/// Executes one startup file through [`Shell::eval`], so expansions, `export`,
/// conditionals and `.` all behave as they would interactively.
fn source(shell: &mut Shell, path: &Path) {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!("sharky: {}: {err}", path.display());
            return;
        }
    };

    match shell.eval_incremental(&source) {
        Ok(()) => {}
        Err(err) => {
            // bash reads a profile statement by statement, so the complete
            // statements above the offending line have already run and are not
            // replayed; the rest of the file is abandoned and the login
            // sequence continues with the next file.
            let line = if err.line > 0 {
                format!(":{}", err.line)
            } else {
                String::new()
            };
            eprintln!("sharky: {}{line}: {}", path.display(), err.message);
        }
    }
}

pub fn discover(home: &Path) -> Vec<String> {
    let mut entries = Vec::new();

    for name in FILES {
        let Ok(text) = std::fs::read_to_string(home.join(name)) else {
            continue;
        };
        for line in text.lines() {
            collect_line(home, line, &mut entries);
        }
    }

    entries.retain(|dir| Path::new(dir).is_dir());
    let mut unique = Vec::new();
    for dir in entries {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

fn collect_line(home: &Path, line: &str, entries: &mut Vec<String>) {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return;
    }

    if let Some(rest) = line.strip_prefix("fish_add_path ") {
        collect_words(home, rest, entries);
    } else if line.starts_with("set ") {
        if let Some((_, rest)) = line.split_once(" PATH ") {
            collect_words(home, rest, entries);
        }
    } else {
        let assignment = line.strip_prefix("export ").unwrap_or(line).trim();
        collect_assignment(home, assignment, entries);
    }
}

fn collect_assignment(home: &Path, text: &str, entries: &mut Vec<String>) {
    let Some(value) = text.strip_prefix("PATH=") else {
        return;
    };
    let value = value.trim();
    if value.contains("$(")
        || value.contains('`')
        || value.contains(';')
        || value.contains('&')
        || value.contains('|')
        || value.starts_with('\'')
    {
        return;
    }
    if !value.contains("$PATH") && !value.contains("${PATH}") {
        return;
    }

    for segment in value.split(':') {
        if let Some(dir) = expand(home, segment) {
            entries.push(dir);
        }
    }
}

fn collect_words(home: &Path, text: &str, entries: &mut Vec<String>) {
    for word in text.split_whitespace() {
        if word.starts_with('-') {
            continue;
        }
        if let Some(dir) = expand(home, word) {
            entries.push(dir);
        }
    }
}

fn expand(home: &Path, segment: &str) -> Option<String> {
    let segment = segment.trim().trim_matches('"');
    if segment.is_empty() || segment == "$PATH" || segment == "${PATH}" {
        return None;
    }
    if segment.contains('$') && !segment.contains("$HOME") && !segment.contains("${HOME}") {
        return None;
    }

    let home_text = home.display().to_string();
    let mut text = segment
        .replace("${HOME}", &home_text)
        .replace("$HOME", &home_text);

    if text == "~" {
        text = home_text.clone();
    } else if let Some(rest) = text.strip_prefix("~/") {
        text = format!("{home_text}/{rest}");
    }

    if text.contains('$')
        || text.contains('`')
        || text.contains('"')
        || text.contains('\'')
        || !text.starts_with('/')
    {
        return None;
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sharky-profiles-{}-{suffix}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_simple_path_exports_without_executing_them() {
        let dir = home("simple");
        for name in [".spicetify", ".local/bin", "bin"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(
            dir.join(".bashrc"),
            r#"# comment
export PATH="$HOME/.spicetify:$PATH"
export PATH="${HOME}/.local/bin:${PATH}"
PATH="~/bin:$PATH"
export PATH="$(pwd)/evil:$PATH"
export FOO=bar
export PATH="/replacement"
"#,
        )
        .unwrap();

        let entries = discover(&dir);
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(
            entries,
            vec![
                dir.join(".spicetify").display().to_string(),
                dir.join(".local/bin").display().to_string(),
                dir.join("bin").display().to_string(),
            ]
        );
    }

    #[test]
    fn ignores_missing_directories_and_duplicates() {
        let dir = home("dedupe");
        std::fs::create_dir_all(dir.join("real")).unwrap();
        std::fs::write(
            dir.join(".zshrc"),
            "export PATH=\"~/real:~/missing:~/real:$PATH\"\n",
        )
        .unwrap();

        let entries = discover(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries, vec![dir.join("real").display().to_string()]);
    }

    #[test]
    fn understands_fish_path_helpers() {
        let dir = home("fish");
        std::fs::create_dir_all(dir.join("fishbin")).unwrap();
        std::fs::create_dir_all(dir.join(".config/fish")).unwrap();
        std::fs::write(
            dir.join(".config/fish/config.fish"),
            "fish_add_path ~/fishbin\nset -gx PATH $PATH ~/fishbin\n",
        )
        .unwrap();

        let entries = discover(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(entries, vec![dir.join("fishbin").display().to_string()]);
    }
}
