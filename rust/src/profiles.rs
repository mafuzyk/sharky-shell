use std::path::Path;

use crate::shell::Shell;

const FILES: &[&str] = &[
    ".profile",
    ".bash_profile",
    ".bash_login",
    ".bashrc",
    ".zshrc",
    ".zprofile",
    ".config/fish/config.fish",
];

pub fn login(shell: &mut Shell) {
    let home = Path::new(&shell.var("HOME")).to_path_buf();

    for path in [
        std::path::PathBuf::from("/etc/sharky/profile.sharky"),
        home.join(".config/sharky/profile.sharky"),
    ] {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Err(err) = shell.eval(&source) {
            let line = if err.line > 0 {
                format!(":{}", err.line)
            } else {
                String::new()
            };
            eprintln!("sharky: {}{line}: {}", path.display(), err.message);
        }
    }

    for path in [
        std::path::PathBuf::from("/etc/profile"),
        home.join(".profile"),
        home.join(".bash_profile"),
        home.join(".zshprofile"),
    ] {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in source.lines() {
            if let Some((name, value)) = literal_assignment(line) {
                shell.set_var(&name, value);
            }
        }
    }
}

fn literal_assignment(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let (name, value) = line.split_once('=')?;
    let name = name.trim();
    if name.is_empty()
        || !name
            .chars()
            .next()
            .is_some_and(|char| char.is_ascii_alphabetic() || char == '_')
        || !name
            .chars()
            .all(|char| char.is_ascii_alphanumeric() || char == '_')
    {
        return None;
    }

    let value = value.trim();
    if value.contains("$(")
        || value.contains('`')
        || value.contains(';')
        || value.contains("&&")
        || value.contains("||")
        || value.contains('|')
        || value.contains('<')
        || value.contains('>')
    {
        return None;
    }

    let value = if let Some(inner) = value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
    {
        if inner.contains('$') || inner.contains('"') {
            return None;
        }
        inner.to_string()
    } else if let Some(inner) = value
        .strip_prefix('\'')
        .and_then(|inner| inner.strip_suffix('\''))
    {
        if inner.contains('\'') {
            return None;
        }
        inner.to_string()
    } else {
        if value.contains('"') || value.contains('\'') || value.contains('$') || value.contains('\\')
        {
            return None;
        }
        value.to_string()
    };

    Some((name.to_string(), value))
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
