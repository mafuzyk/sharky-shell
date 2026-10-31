use std::path::PathBuf;

use crate::shell::Shell;
use crate::term::{self, Key};

pub enum Line {
    Content(String),
    Interrupted,
    Eof,
}

pub struct Editor {
    history: Vec<String>,
    index: usize,
    draft: String,
    path: PathBuf,
    limit: usize,
}

impl Editor {
    pub fn new(path: PathBuf, limit: usize) -> Self {
        let history = std::fs::read_to_string(&path)
            .map(|text| text.lines().map(str::to_string).collect())
            .unwrap_or_default();

        Editor {
            history,
            index: 0,
            draft: String::new(),
            path,
            limit,
        }
    }

    pub fn read_line(&mut self, prompt: &str, shell: &Shell) -> std::io::Result<Line> {
        let Some(_raw) = term::RawMode::enable() else {
            return fallback_line();
        };

        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0;
        self.index = self.history.len();
        self.draft.clear();

        loop {
            self.render(prompt, &buffer, cursor)?;

            match term::read_key()? {
                Key::Enter => {
                    term::write_stdout("\r\n")?;
                    return Ok(Line::Content(buffer.iter().collect()));
                }
                Key::Ctrl('c') => {
                    term::write_stdout("^C\r\n")?;
                    return Ok(Line::Interrupted);
                }
                Key::Ctrl('d') => {
                    if buffer.is_empty() {
                        term::write_stdout("\r\n")?;
                        return Ok(Line::Eof);
                    }
                    if cursor < buffer.len() {
                        buffer.remove(cursor);
                    }
                }
                Key::Ctrl('l') => {
                    term::write_stdout("\x1b[2J\x1b[H")?;
                }
                Key::Ctrl('a') | Key::Home => cursor = 0,
                Key::Ctrl('e') | Key::End => cursor = buffer.len(),
                Key::Ctrl('u') => {
                    buffer.drain(..cursor);
                    cursor = 0;
                }
                Key::Ctrl('k') => buffer.truncate(cursor),
                Key::Ctrl('w') => {
                    let mut start = cursor;
                    while start > 0 && buffer[start - 1].is_whitespace() {
                        start -= 1;
                    }
                    while start > 0 && !buffer[start - 1].is_whitespace() {
                        start -= 1;
                    }
                    buffer.drain(start..cursor);
                    cursor = start;
                }
                Key::Left => cursor = cursor.saturating_sub(1),
                Key::Right => cursor = (cursor + 1).min(buffer.len()),
                Key::Backspace => {
                    if cursor > 0 {
                        buffer.remove(cursor - 1);
                        cursor -= 1;
                    }
                }
                Key::Delete => {
                    if cursor < buffer.len() {
                        buffer.remove(cursor);
                    }
                }
                Key::Up => self.recall_newer(&mut buffer, &mut cursor),
                Key::Down => self.recall_older(&mut buffer, &mut cursor),
                Key::Tab => complete(&mut buffer, &mut cursor, shell),
                Key::Char(char) => {
                    buffer.insert(cursor, char);
                    cursor += 1;
                }
                Key::Unknown | Key::Escape | Key::Ctrl(_) => {}
            }
        }
    }

    pub fn push_history(&mut self, line: &str) {
        let line = line.trim_end();
        if line.is_empty() || line.lines().count() > 1 {
            return;
        }
        if self.history.last().map(String::as_str) == Some(line) {
            return;
        }
        self.history.push(line.to_string());
        if self.history.len() > self.limit {
            let excess = self.history.len() - self.limit;
            self.history.drain(..excess);
        }
    }

    pub fn save(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let start = self.history.len().saturating_sub(self.limit);
        let text = self.history[start..].join("\n");
        let _ = std::fs::write(&self.path, text);
    }

    fn recall_newer(&mut self, buffer: &mut Vec<char>, cursor: &mut usize) {
        if self.index == 0 {
            return;
        }
        if self.index == self.history.len() {
            self.draft = buffer.iter().collect();
        }
        self.index -= 1;
        *buffer = self.history[self.index].chars().collect();
        *cursor = buffer.len();
    }

    fn recall_older(&mut self, buffer: &mut Vec<char>, cursor: &mut usize) {
        if self.index + 1 > self.history.len() {
            return;
        }
        self.index += 1;
        *buffer = if self.index == self.history.len() {
            self.draft.clone()
        } else {
            self.history[self.index].clone()
        }
        .chars()
        .collect();
        *cursor = buffer.len();
    }

    fn render(&self, prompt: &str, buffer: &[char], cursor: usize) -> std::io::Result<()> {
        let mut out = String::from("\r");
        out.push_str(prompt);
        out.extend(buffer.iter());
        out.push_str("\x1b[K");

        let back = buffer.len().saturating_sub(cursor);
        if back > 0 {
            out.push_str(&format!("\x1b[{back}D"));
        }

        term::write_stdout(&out)
    }
}

fn fallback_line() -> std::io::Result<Line> {
    use std::io::BufRead;
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line)?;
    if read == 0 {
        return Ok(Line::Eof);
    }
    Ok(Line::Content(line.trim_end_matches('\n').to_string()))
}

fn complete(buffer: &mut Vec<char>, cursor: &mut usize, shell: &Shell) {
    let mut start = *cursor;
    while start > 0 && !buffer[start - 1].is_whitespace() {
        start -= 1;
    }

    let word: String = buffer[start..*cursor].iter().collect();
    if word.is_empty() {
        return;
    }

    let command_position = buffer[..start].iter().all(|char| char.is_whitespace());
    let mut candidates = if word.contains('/') || !command_position {
        file_candidates(&word)
    } else {
        command_candidates(&word, shell)
    };

    candidates.sort();
    candidates.dedup();

    let Some(first) = candidates.first() else {
        return;
    };

    let common = candidates
        .iter()
        .fold(first.clone(), |prefix, candidate| common_prefix(&prefix, candidate));

    let insert = if candidates.len() == 1 {
        let suffix = &candidates[0][word.len()..];
        match std::fs::metadata(&candidates[0]) {
            Ok(meta) if meta.is_dir() => format!("{suffix}/"),
            _ => format!("{suffix} "),
        }
    } else if common.len() > word.len() {
        common[word.len()..].to_string()
    } else {
        return;
    };

    for (offset, char) in insert.chars().enumerate() {
        buffer.insert(*cursor + offset, char);
    }
    *cursor += insert.chars().count();
}

fn command_candidates(word: &str, shell: &Shell) -> Vec<String> {
    let mut out: Vec<String> = shell
        .command_words()
        .into_iter()
        .filter(|name| name.starts_with(word))
        .collect();

    for dir in shell.path_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(word) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                out.push(name);
            }
        }
    }

    out
}

fn file_candidates(word: &str) -> Vec<String> {
    let (dir, base) = match word.rfind('/') {
        Some(index) => (&word[..index], &word[index + 1..]),
        None => ("", word),
    };

    let directory = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(base) || name.starts_with('.') {
                return None;
            }
            Some(if dir.is_empty() {
                name
            } else {
                format!("{dir}/{name}")
            })
        })
        .collect()
}

fn common_prefix(left: &str, right: &str) -> String {
    left.chars()
        .zip(right.chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect()
}
