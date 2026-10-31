use crate::shell::Shell;
use crate::term::{self, Key};

pub enum Line {
    Content(String),
    Interrupted,
    Eof,
}

pub struct Editor {
    index: usize,
    draft: String,
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            index: 0,
            draft: String::new(),
        }
    }

    pub fn read_line(&mut self, prompt: &str, shell: &mut Shell) -> std::io::Result<Line> {
        let Some(_raw) = term::RawMode::enable() else {
            return fallback_line();
        };

        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0;
        self.index = shell.history_entries().len();
        self.draft.clear();

        let prompt_width = last_line_width(prompt);
        term::write_stdout(prompt)?;

        loop {
            self.render_input(&buffer, cursor, prompt_width)?;

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
                    term::write_stdout(prompt)?;
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
                Key::Up => self.recall_newer(&mut buffer, &mut cursor, shell),
                Key::Down => self.recall_older(&mut buffer, &mut cursor, shell),
                Key::Tab => complete(&mut buffer, &mut cursor, shell),
                Key::Char(char) => {
                    buffer.insert(cursor, char);
                    cursor += 1;
                }
                Key::Unknown | Key::Escape | Key::Ctrl(_) => {}
            }
        }
    }

    fn recall_newer(&mut self, buffer: &mut Vec<char>, cursor: &mut usize, shell: &Shell) {
        let history = shell.history_entries();
        if self.index == 0 {
            return;
        }
        if self.index == history.len() {
            self.draft = buffer.iter().collect();
        }
        self.index -= 1;
        *buffer = history[self.index].chars().collect();
        *cursor = buffer.len();
    }

    fn recall_older(&mut self, buffer: &mut Vec<char>, cursor: &mut usize, shell: &Shell) {
        let history = shell.history_entries();
        if self.index + 1 > history.len() {
            return;
        }
        self.index += 1;
        *buffer = if self.index == history.len() {
            self.draft.clone()
        } else {
            history[self.index].clone()
        }
        .chars()
        .collect();
        *cursor = buffer.len();
    }

    fn render_input(&self, buffer: &[char], cursor: usize, prompt_width: usize) -> std::io::Result<()> {
        let columns = term::width().max(1);
        let prompt_row = prompt_width / columns;
        let cursor_row = (prompt_width + cursor) / columns;

        let mut out = String::new();
        if cursor_row > prompt_row {
            out.push_str(&format!("\x1b[{}A", cursor_row - prompt_row));
        }
        out.push('\r');
        let column = prompt_width % columns;
        if column > 0 {
            out.push_str(&format!("\x1b[{column}C"));
        }
        out.push_str("\x1b[J");
        out.extend(buffer.iter());

        let end_row = (prompt_width + buffer.len()) / columns;
        let end_column = (prompt_width + cursor) % columns;
        if end_row > cursor_row {
            out.push_str(&format!("\x1b[{}A", end_row - cursor_row));
        }
        out.push('\r');
        if end_column > 0 {
            out.push_str(&format!("\x1b[{end_column}C"));
        }

        term::write_stdout(&out)
    }
}

fn last_line_width(prompt: &str) -> usize {
    let line = prompt.rsplit('\n').next().unwrap_or("");
    let mut width = 0;
    let mut chars = line.chars().peekable();

    while let Some(char) = chars.next() {
        if char != '\x1b' {
            width += 1;
            continue;
        }
        match chars.next() {
            Some('[') => {
                for char in chars.by_ref() {
                    if ('@'..='~').contains(&char) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(char) = chars.next() {
                    if char == '\x07' {
                        break;
                    }
                    if char == '\x1b' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) | None => {}
        }
    }

    width
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
