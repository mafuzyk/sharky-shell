use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub struct History {
    entries: Vec<String>,
    path: PathBuf,
    limit: usize,
}

impl History {
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os("SHARKY_HISTORY") {
            return PathBuf::from(path);
        }
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(dir).join("sharky/history");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/share/sharky/history");
        }
        PathBuf::from("/tmp/sharky-history")
    }

    pub fn load(path: PathBuf, limit: usize) -> Self {
        let entries = std::fs::read_to_string(&path)
            .map(|text| {
                text.lines()
                    .map(unescape)
                    .filter(|entry| !entry.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let mut history = History {
            entries,
            path,
            limit,
        };
        history.trim();
        history
    }

    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn add(&mut self, command: &str) {
        let command = command.trim_end();
        if command.is_empty() || self.entries.iter().any(|entry| entry == command) {
            return;
        }
        self.entries.push(command.to_string());
        self.trim();
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.save();
    }

    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&escape(entry));
            text.push('\n');
        }

        if let Ok(mut file) = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.path)
        {
            let _ = file.write_all(text.as_bytes());
        }
        let _ = std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600));
    }

    fn trim(&mut self) {
        if self.entries.len() > self.limit {
            let excess = self.entries.len() - self.limit;
            self.entries.drain(..excess);
        }
    }
}

fn escape(entry: &str) -> String {
    entry.replace('\\', "\\\\").replace('\n', "\\n")
}

fn unescape(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();

    while let Some(char) = chars.next() {
        if char != '\\' {
            out.push(char);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}
