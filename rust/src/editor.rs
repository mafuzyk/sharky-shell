use std::collections::HashMap;

use crate::highlight;
use crate::prompt;
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
    kill: String,
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            index: 0,
            draft: String::new(),
            kill: String::new(),
        }
    }

    pub fn read_line(&mut self, prompt: &str, shell: &mut Shell) -> std::io::Result<Line> {
        let Some(_raw) = term::RawMode::enable() else {
            return fallback_line();
        };

        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0;
        let mut pasting = false;
        self.index = shell.history_entries().len();
        self.draft.clear();

        let prompt_width = last_line_width(prompt);
        let prompt_last = prompt.rsplit('\n').next().unwrap_or("").to_string();
        term::write_stdout(prompt)?;

        let bindings = build_bindings(shell);
        let mut undo: Vec<(Vec<char>, usize)> = Vec::new();
        let mut menu: Option<CompletionMenu> = None;
        let mut vi_normal = false;
        let mut vi_count = 0usize;
        let mut vi_pending: Option<char> = None;
        let mut vi_replace = false;

        loop {
            let suggestion = if vi_normal {
                None
            } else {
                autosuggest(shell, &buffer, cursor, prompt_width)
            };
            let ghost = suggestion
                .as_deref()
                .map(|text| prompt::painted(shell, "suggestion", text))
                .unwrap_or_default();
            let display = highlight::render(shell, &buffer);
            let (menu_text, menu_rows) = menu
                .as_ref()
                .map(|menu| render_menu(shell, menu, term::width()))
                .unwrap_or_default();
            self.render_input(
                &buffer,
                cursor,
                prompt_width,
                &ghost,
                &display,
                &menu_text,
                menu_rows,
            )?;

            let key = term::read_key()?;

            if pasting {
                match key {
                    Key::PasteEnd => pasting = false,
                    Key::Enter => {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.insert(cursor, '\n');
                        cursor += 1;
                    }
                    Key::Tab => {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.insert(cursor, '\t');
                        cursor += 1;
                    }
                    Key::Backspace => {
                        if cursor > 0 {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor - 1);
                            cursor -= 1;
                        }
                    }
                    Key::Delete => {
                        if cursor < buffer.len() {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor);
                        }
                    }
                    Key::Char(char) => {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.insert(cursor, char);
                        cursor += 1;
                    }
                    _ => {}
                }
                continue;
            }

            if let Some(active) = menu.as_mut() {
                match key {
                    Key::Tab => {
                        active.selected = (active.selected + 1) % active.candidates.len();
                        continue;
                    }
                    Key::ShiftTab => {
                        active.selected = if active.selected == 0 {
                            active.candidates.len() - 1
                        } else {
                            active.selected - 1
                        };
                        continue;
                    }
                    Key::Enter => {
                        if let Some(active) = menu.take() {
                            insert_selection(&mut buffer, &mut cursor, &active);
                        }
                        continue;
                    }
                    Key::Escape => {
                        menu = None;
                        continue;
                    }
                    Key::Ctrl('c') => menu = None,
                    _ => menu = None,
                }
            }

            if key == Key::Escape && shell.vi_mode() {
                if !vi_normal {
                    vi_normal = true;
                    cursor = cursor.saturating_sub(1);
                }
                vi_pending = None;
                vi_replace = false;
                vi_count = 0;
                continue;
            }

            if vi_normal {
                match key {
                    Key::Enter => {
                        expand_abbreviation(&mut buffer, &mut cursor, shell, "");
                        term::write_stdout("\r\n")?;
                        return Ok(Line::Content(buffer.iter().collect()));
                    }
                    Key::Backspace => {
                        if cursor < buffer.len() {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor);
                        } else if cursor > 0 {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor - 1);
                            cursor -= 1;
                        }
                        vi_count = 0;
                        continue;
                    }
                    Key::Ctrl('r') => {
                        if history_search(prompt_width, &prompt_last, shell, &mut buffer, &mut cursor)?
                        {
                            term::write_stdout("\r\n")?;
                            return Ok(Line::Content(buffer.iter().collect()));
                        }
                        vi_count = 0;
                        continue;
                    }
                    Key::Escape => {
                        vi_pending = None;
                        vi_replace = false;
                        vi_count = 0;
                        continue;
                    }
                    Key::Char(char) => {
                        vi_command(
                            char,
                            &mut buffer,
                            &mut cursor,
                            &mut vi_normal,
                            &mut vi_count,
                            &mut vi_pending,
                            &mut vi_replace,
                            &mut undo,
                            shell,
                            self,
                        );
                        continue;
                    }
                    _ => {
                        vi_count = 0;
                        continue;
                    }
                }
            }

            if let Some(bound) = bound_key(key)
                && let Some(action) = bindings.get(&bound).copied()
            {
                match self.apply_action(
                    action,
                    &mut buffer,
                    &mut cursor,
                    suggestion.as_deref(),
                    prompt,
                    shell,
                    &mut undo,
                )? {
                    Outcome::Continue => continue,
                    Outcome::Submit => {
                        expand_abbreviation(&mut buffer, &mut cursor, shell, "");
                        term::write_stdout("\r\n")?;
                        return Ok(Line::Content(buffer.iter().collect()));
                    }
                    Outcome::Abort => {
                        term::write_stdout("^C\r\n")?;
                        return Ok(Line::Interrupted);
                    }
                    Outcome::HistorySearch => {
                        if history_search(prompt_width, &prompt_last, shell, &mut buffer, &mut cursor)?
                        {
                            term::write_stdout("\r\n")?;
                            return Ok(Line::Content(buffer.iter().collect()));
                        }
                        continue;
                    }
                    Outcome::Complete => {
                        menu = complete_menu(&mut buffer, &mut cursor, shell);
                        continue;
                    }
                    Outcome::FuzzyFiles => {
                        term::write_stdout("\x1b[?1049h")?;
                        let selected = fuzzy_files(shell);
                        term::write_stdout("\x1b[?1049l")?;
                        if let Some(path) = selected? {
                            push_undo(&mut undo, &buffer, cursor);
                            insert_text(&mut buffer, &mut cursor, &escape_completion(&path));
                        }
                        continue;
                    }
                }
            }

            match key {
                Key::PasteStart => pasting = true,
                Key::Ctrl('d') => {
                    if buffer.is_empty() {
                        term::write_stdout("\r\n")?;
                        return Ok(Line::Eof);
                    }
                    if cursor < buffer.len() {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.remove(cursor);
                    }
                }
                Key::Ctrl('u') => {
                    if cursor > 0 {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.drain(..cursor);
                        cursor = 0;
                    }
                }
                Key::Ctrl('w') => {
                    push_undo(&mut undo, &buffer, cursor);
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
                Key::Char(' ') => {
                    if !expand_abbreviation(&mut buffer, &mut cursor, shell, " ") {
                        push_undo(&mut undo, &buffer, cursor);
                        buffer.insert(cursor, ' ');
                        cursor += 1;
                    }
                }
                Key::Char(char) => {
                    push_undo(&mut undo, &buffer, cursor);
                    buffer.insert(cursor, char);
                    cursor += 1;
                }
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_action(
        &mut self,
        action: Action,
        buffer: &mut Vec<char>,
        cursor: &mut usize,
        suggestion: Option<&str>,
        prompt: &str,
        shell: &Shell,
        undo: &mut Vec<(Vec<char>, usize)>,
    ) -> std::io::Result<Outcome> {
        match action {
            Action::AcceptLine => return Ok(Outcome::Submit),
            Action::Abort => return Ok(Outcome::Abort),
            Action::HistorySearch => return Ok(Outcome::HistorySearch),
            Action::Complete => return Ok(Outcome::Complete),
            Action::FuzzyFiles => return Ok(Outcome::FuzzyFiles),
            Action::ClearScreen => {
                term::write_stdout("\x1b[2J\x1b[H")?;
                term::write_stdout(prompt)?;
            }
            Action::BackwardChar => *cursor = cursor.saturating_sub(1),
            Action::ForwardChar => {
                if *cursor == buffer.len() {
                    self.insert_suggestion(buffer, cursor, suggestion, undo);
                } else {
                    *cursor += 1;
                }
            }
            Action::AcceptSuggestion => {
                self.insert_suggestion(buffer, cursor, suggestion, undo);
            }
            Action::BackwardWord => *cursor = backward_word(buffer, *cursor),
            Action::ForwardWord => *cursor = forward_word(buffer, *cursor),
            Action::BeginningOfLine => *cursor = line_start(buffer, *cursor),
            Action::EndOfLine => {
                let end = line_end(buffer, *cursor);
                if end == *cursor && *cursor == buffer.len() {
                    self.insert_suggestion(buffer, cursor, suggestion, undo);
                } else {
                    *cursor = end;
                }
            }
            Action::DeleteChar => {
                if *cursor < buffer.len() {
                    push_undo(undo, buffer, *cursor);
                    buffer.remove(*cursor);
                }
            }
            Action::BackwardDeleteChar => {
                if *cursor > 0 {
                    push_undo(undo, buffer, *cursor);
                    buffer.remove(*cursor - 1);
                    *cursor -= 1;
                }
            }
            Action::KillLine => {
                let end = line_end(buffer, *cursor);
                if *cursor < end {
                    push_undo(undo, buffer, *cursor);
                    self.kill = buffer[*cursor..end].iter().collect();
                    buffer.drain(*cursor..end);
                }
            }
            Action::Yank => {
                if !self.kill.is_empty() {
                    push_undo(undo, buffer, *cursor);
                    let text: Vec<char> = self.kill.chars().collect();
                    buffer.splice(*cursor..*cursor, text.iter().copied());
                    *cursor += text.len();
                }
            }
            Action::TransposeChars => {
                if buffer.len() >= 2 && *cursor >= 2 {
                    push_undo(undo, buffer, *cursor);
                    buffer.swap(*cursor - 2, *cursor - 1);
                    if *cursor < buffer.len() {
                        *cursor += 1;
                    }
                }
            }
            Action::UpcaseWord | Action::DowncaseWord | Action::CapitalizeWord => {
                let (start, end) = current_word(buffer, *cursor);
                if start < end {
                    push_undo(undo, buffer, *cursor);
                    for (offset, char) in buffer[start..end].iter_mut().enumerate() {
                        *char = match action {
                            Action::UpcaseWord => char.to_ascii_uppercase(),
                            Action::DowncaseWord => char.to_ascii_lowercase(),
                            Action::CapitalizeWord if offset == 0 => char.to_ascii_uppercase(),
                            Action::CapitalizeWord => char.to_ascii_lowercase(),
                            _ => *char,
                        };
                    }
                    *cursor = end;
                }
            }
            Action::Undo => {
                if let Some((previous, position)) = undo.pop() {
                    *buffer = previous;
                    *cursor = position.min(buffer.len());
                }
            }
            Action::HistoryPrev => {
                if buffer.contains(&'\n') {
                    *cursor = move_line(buffer, *cursor, true);
                } else {
                    self.recall_newer(buffer, cursor, shell);
                }
            }
            Action::HistoryNext => {
                if buffer.contains(&'\n') {
                    *cursor = move_line(buffer, *cursor, false);
                } else {
                    self.recall_older(buffer, cursor, shell);
                }
            }
        }
        Ok(Outcome::Continue)
    }

    fn insert_suggestion(
        &mut self,
        buffer: &mut Vec<char>,
        cursor: &mut usize,
        suggestion: Option<&str>,
        undo: &mut Vec<(Vec<char>, usize)>,
    ) {
        if *cursor == buffer.len()
            && let Some(suffix) = suggestion
        {
            push_undo(undo, buffer, *cursor);
            buffer.extend(suffix.chars());
            *cursor = buffer.len();
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

    #[allow(clippy::too_many_arguments)]
    fn render_input(
        &self,
        buffer: &[char],
        cursor: usize,
        prompt_width: usize,
        ghost: &str,
        display: &str,
        menu_text: &str,
        menu_rows: usize,
    ) -> std::io::Result<()> {
        let columns = term::width().max(1);
        let (cursor_row, cursor_column) = position(buffer, cursor, columns, prompt_width);
        let (end_row, _) = position(buffer, buffer.len(), columns, prompt_width);
        let prompt_row = prompt_width / columns;
        let start_column = prompt_width % columns;

        let mut out = String::new();
        if cursor_row > prompt_row {
            out.push_str(&format!("\x1b[{}A", cursor_row - prompt_row));
        }
        out.push('\r');
        if start_column > 0 {
            out.push_str(&format!("\x1b[{start_column}C"));
        }
        // Clear from the end of the prompt so the prompt and its icons survive
        // the redraw; everything below it (input, menu, ghost) is replaced.
        out.push_str("\x1b[J");
        for char in display.chars() {
            if char == '\n' {
                out.push_str("\r\n");
            } else {
                out.push(char);
            }
        }
        out.push_str(ghost);

        if menu_rows > 0 {
            out.push_str("\r\n");
            out.push_str(menu_text);
        }

        let bottom_row = end_row + menu_rows;
        if bottom_row > cursor_row {
            out.push_str(&format!("\x1b[{}A", bottom_row - cursor_row));
        }
        out.push('\r');
        if cursor_column > 0 {
            out.push_str(&format!("\x1b[{cursor_column}C"));
        }

        term::write_stdout(&out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    AcceptLine,
    Abort,
    ClearScreen,
    BackwardChar,
    ForwardChar,
    BackwardWord,
    ForwardWord,
    BeginningOfLine,
    EndOfLine,
    DeleteChar,
    BackwardDeleteChar,
    KillLine,
    Yank,
    TransposeChars,
    UpcaseWord,
    DowncaseWord,
    CapitalizeWord,
    Undo,
    HistorySearch,
    Complete,
    AcceptSuggestion,
    FuzzyFiles,
    HistoryPrev,
    HistoryNext,
}

enum Outcome {
    Continue,
    Submit,
    Abort,
    HistorySearch,
    Complete,
    FuzzyFiles,
}

const DEFAULT_BINDINGS: &[(&str, &str)] = &[
    ("enter", "accept_line"),
    ("ctrl-c", "abort"),
    ("ctrl-l", "clear_screen"),
    ("ctrl-a", "beginning_of_line"),
    ("home", "beginning_of_line"),
    ("ctrl-e", "end_of_line"),
    ("end", "end_of_line"),
    ("left", "backward_char"),
    ("right", "forward_char"),
    ("up", "history_prev"),
    ("down", "history_next"),
    ("backspace", "backward_delete_char"),
    ("delete", "delete_char"),
    ("tab", "complete"),
    ("ctrl-r", "history_search"),
    ("ctrl-k", "kill_line"),
    ("ctrl-y", "yank"),
    ("ctrl-_", "undo"),
    ("ctrl-t", "fuzzy_files"),
];

fn build_bindings(shell: &Shell) -> HashMap<BoundKey, Action> {
    let mut bindings = HashMap::new();
    for (key, action) in DEFAULT_BINDINGS {
        if let (Some(key), Some(action)) = (parse_bound(key), parse_action(action)) {
            bindings.insert(key, action);
        }
    }
    for (key, action) in shell.keybinds() {
        if let (Some(key), Some(action)) = (parse_bound(key), parse_action(action)) {
            bindings.insert(key, action);
        }
    }
    bindings
}

fn parse_action(name: &str) -> Option<Action> {
    Some(match name {
        "accept_line" => Action::AcceptLine,
        "abort" => Action::Abort,
        "clear_screen" => Action::ClearScreen,
        "backward_char" => Action::BackwardChar,
        "forward_char" => Action::ForwardChar,
        "backward_word" => Action::BackwardWord,
        "forward_word" => Action::ForwardWord,
        "beginning_of_line" => Action::BeginningOfLine,
        "end_of_line" => Action::EndOfLine,
        "delete_char" => Action::DeleteChar,
        "backward_delete_char" => Action::BackwardDeleteChar,
        "kill_line" => Action::KillLine,
        "yank" => Action::Yank,
        "transpose_chars" => Action::TransposeChars,
        "upcase_word" => Action::UpcaseWord,
        "downcase_word" => Action::DowncaseWord,
        "capitalize_word" => Action::CapitalizeWord,
        "undo" => Action::Undo,
        "history_search" => Action::HistorySearch,
        "complete" => Action::Complete,
        "accept_suggestion" => Action::AcceptSuggestion,
        "fuzzy_files" => Action::FuzzyFiles,
        "history_prev" => Action::HistoryPrev,
        "history_next" => Action::HistoryNext,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BoundKey {
    Char(char),
    Ctrl(char),
    Alt(char),
    Tab,
    ShiftTab,
    Enter,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Backspace,
}

fn bound_key(key: Key) -> Option<BoundKey> {
    Some(match key {
        Key::Char(char) => BoundKey::Char(char),
        Key::Ctrl(char) => BoundKey::Ctrl(char.to_ascii_lowercase()),
        Key::Alt(char) => BoundKey::Alt(char.to_ascii_lowercase()),
        Key::Tab => BoundKey::Tab,
        Key::ShiftTab => BoundKey::ShiftTab,
        Key::Enter => BoundKey::Enter,
        Key::Escape => BoundKey::Escape,
        Key::Up => BoundKey::Up,
        Key::Down => BoundKey::Down,
        Key::Left => BoundKey::Left,
        Key::Right => BoundKey::Right,
        Key::Home => BoundKey::Home,
        Key::End => BoundKey::End,
        Key::Delete => BoundKey::Delete,
        Key::Backspace => BoundKey::Backspace,
        Key::Unknown | Key::PasteStart | Key::PasteEnd => return None,
    })
}

fn parse_bound(name: &str) -> Option<BoundKey> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "tab" => return Some(BoundKey::Tab),
        "shift-tab" => return Some(BoundKey::ShiftTab),
        "enter" => return Some(BoundKey::Enter),
        "escape" => return Some(BoundKey::Escape),
        "up" => return Some(BoundKey::Up),
        "down" => return Some(BoundKey::Down),
        "left" => return Some(BoundKey::Left),
        "right" => return Some(BoundKey::Right),
        "home" => return Some(BoundKey::Home),
        "end" => return Some(BoundKey::End),
        "delete" => return Some(BoundKey::Delete),
        "backspace" => return Some(BoundKey::Backspace),
        _ => {}
    }

    if let Some(rest) = strip_prefix_any(name, &["ctrl-", "ctrl+", "c-", "c+"]) {
        let mut chars = rest.chars();
        let char = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        return Some(BoundKey::Ctrl(char.to_ascii_lowercase()));
    }
    if let Some(rest) = strip_prefix_any(name, &["alt-", "alt+", "m-", "m+"]) {
        let mut chars = rest.chars();
        let char = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        return Some(BoundKey::Alt(char.to_ascii_lowercase()));
    }

    let mut chars = name.chars();
    let char = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(BoundKey::Char(char))
}

fn strip_prefix_any<'a>(name: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    for prefix in prefixes {
        if name.len() > prefix.len()
            && let Some(head) = name.get(..prefix.len())
            && head.eq_ignore_ascii_case(prefix)
        {
            return name.get(prefix.len()..);
        }
    }
    None
}

fn push_undo(undo: &mut Vec<(Vec<char>, usize)>, buffer: &[char], cursor: usize) {
    if undo.len() >= 100 {
        undo.remove(0);
    }
    undo.push((buffer.to_vec(), cursor));
}

fn is_word_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || char == '_'
}

fn backward_word(buffer: &[char], cursor: usize) -> usize {
    let mut index = cursor.min(buffer.len());
    while index > 0 && !is_word_char(buffer[index - 1]) {
        index -= 1;
    }
    while index > 0 && is_word_char(buffer[index - 1]) {
        index -= 1;
    }
    index
}

fn forward_word(buffer: &[char], cursor: usize) -> usize {
    let mut index = cursor.min(buffer.len());
    while index < buffer.len() && !is_word_char(buffer[index]) {
        index += 1;
    }
    while index < buffer.len() && is_word_char(buffer[index]) {
        index += 1;
    }
    index
}

fn current_word(buffer: &[char], cursor: usize) -> (usize, usize) {
    let mut start = cursor.min(buffer.len());
    while start < buffer.len() && !is_word_char(buffer[start]) {
        start += 1;
    }
    let mut end = start;
    while end < buffer.len() && is_word_char(buffer[end]) {
        end += 1;
    }
    (start, end)
}

fn history_search(
    prompt_width: usize,
    prompt_last: &str,
    shell: &Shell,
    buffer: &mut Vec<char>,
    cursor: &mut usize,
) -> std::io::Result<bool> {
    let entries = shell.history_entries();
    if entries.is_empty() {
        return Ok(false);
    }

    let columns = term::width().max(1);
    let mut query = String::new();
    let mut offset = 0;
    let mut shown: Option<String> = None;
    let mut line;
    let mut submit = false;

    term::write_stdout("\r\n")?;
    loop {
        if let Some(found) = find_history(entries, &query, offset) {
            shown = Some(found);
        }
        let display = shown.as_deref().unwrap_or("");
        line = format!("(reverse-i-search) '{query}': {display}");
        term::write_stdout(&format!("\r{line}\x1b[K"))?;

        match term::read_key()? {
            Key::Enter => {
                if let Some(found) = &shown {
                    *buffer = found.chars().collect();
                    *cursor = buffer.len();
                    submit = true;
                }
                break;
            }
            Key::Ctrl('g') | Key::Ctrl('c') | Key::Escape => break,
            Key::Ctrl('r') | Key::Up => {
                offset += 1;
                if find_history(entries, &query, offset).is_none() {
                    offset -= 1;
                }
            }
            Key::Down => {
                offset = offset.saturating_sub(1);
            }
            Key::Backspace => {
                query.pop();
                offset = 0;
            }
            Key::Char(char) => {
                query.push(char);
                offset = 0;
            }
            _ => {
                if let Some(found) = &shown {
                    *buffer = found.chars().collect();
                    *cursor = buffer.len();
                }
                break;
            }
        }
    }

    clear_search_line(&line, columns)?;
    let input_rows = position(buffer, buffer.len(), columns, prompt_width).0 + 1;
    if input_rows > 0 {
        term::write_stdout(&format!("\x1b[{input_rows}A"))?;
    }
    redraw_input(prompt_last, prompt_width, buffer, *cursor)?;
    Ok(submit)
}

fn clear_search_line(line: &str, columns: usize) -> std::io::Result<()> {
    let rows = line.chars().count().div_ceil(columns).max(1);
    let mut out = String::new();
    if rows > 1 {
        out.push_str(&format!("\x1b[{}A", rows - 1));
    }
    out.push_str("\r\x1b[J");
    term::write_stdout(&out)
}

fn redraw_input(
    prompt_last: &str,
    prompt_width: usize,
    buffer: &[char],
    cursor: usize,
) -> std::io::Result<()> {
    let columns = term::width().max(1);
    let (cursor_row, cursor_column) = position(buffer, cursor, columns, prompt_width);
    let (end_row, _) = position(buffer, buffer.len(), columns, prompt_width);

    let mut out = String::from("\r\x1b[J");
    out.push_str(prompt_last);
    for char in buffer {
        if *char == '\n' {
            out.push_str("\r\n");
        } else {
            out.push(*char);
        }
    }

    if end_row > cursor_row {
        out.push_str(&format!("\x1b[{}A", end_row - cursor_row));
    }
    out.push('\r');
    if cursor_column > 0 {
        out.push_str(&format!("\x1b[{cursor_column}C"));
    }
    term::write_stdout(&out)
}

#[allow(clippy::too_many_arguments)]
fn vi_command(
    char: char,
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    vi_normal: &mut bool,
    count: &mut usize,
    pending: &mut Option<char>,
    replace: &mut bool,
    undo: &mut Vec<(Vec<char>, usize)>,
    shell: &Shell,
    editor: &mut Editor,
) {
    if *replace {
        if *cursor < buffer.len() {
            push_undo(undo, buffer, *cursor);
            let times = (*count).max(1);
            for _ in 0..times {
                if *cursor < buffer.len() {
                    buffer[*cursor] = char;
                    *cursor += 1;
                }
            }
        }
        *replace = false;
        *count = 0;
        return;
    }

    if char.is_ascii_digit() && !(char == '0' && *count == 0) {
        *count = count
            .saturating_mul(10)
            .saturating_add((char as u8 - b'0') as usize)
            .min(9999);
        return;
    }

    let times = (*count).max(1);

    if let Some(operator) = *pending {
        *pending = None;
        match char {
            'd' if operator == 'd' => editor.vi_delete_lines(buffer, cursor, times, undo),
            'c' if operator == 'c' => {
                editor.vi_delete_lines(buffer, cursor, times, undo);
                *vi_normal = false;
            }
            'w' => {
                let end = if operator == 'c' {
                    (0..times).fold(*cursor, |index, _| vi_word_end(buffer, index))
                } else {
                    (0..times).fold(*cursor, |index, _| vi_word_forward(buffer, index))
                };
                editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
                if operator == 'c' {
                    *vi_normal = false;
                }
            }
            'b' => {
                let start = (0..times).fold(*cursor, |index, _| vi_word_backward(buffer, index));
                editor.vi_kill_range(buffer, cursor, start, *cursor, undo);
            }
            'e' => {
                let end = (0..times).fold(*cursor, |index, _| vi_word_end(buffer, index));
                editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
            }
            '$' => {
                let end = line_end(buffer, *cursor);
                editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
            }
            '0' => {
                let start = line_start(buffer, *cursor);
                editor.vi_kill_range(buffer, cursor, start, *cursor, undo);
            }
            _ => {}
        }
        *count = 0;
        return;
    }

    match char {
        'i' => *vi_normal = false,
        'a' => {
            *cursor = (*cursor + 1).min(buffer.len());
            *vi_normal = false;
        }
        'I' => {
            *cursor = line_start(buffer, *cursor);
            *vi_normal = false;
        }
        'A' => {
            *cursor = line_end(buffer, *cursor);
            *vi_normal = false;
        }
        'o' => {
            push_undo(undo, buffer, *cursor);
            let end = line_end(buffer, *cursor);
            buffer.insert(end, '\n');
            *cursor = end + 1;
            *vi_normal = false;
        }
        'O' => {
            push_undo(undo, buffer, *cursor);
            let start = line_start(buffer, *cursor);
            buffer.insert(start, '\n');
            *cursor = start;
            *vi_normal = false;
        }
        'h' => *cursor = cursor.saturating_sub(times),
        'l' => *cursor = cursor.saturating_add(times).min(buffer.len()),
        '0' => *cursor = line_start(buffer, *cursor),
        '$' => *cursor = vi_line_end(buffer, *cursor),
        'w' => {
            for _ in 0..times {
                *cursor = vi_word_forward(buffer, *cursor);
            }
        }
        'b' => {
            for _ in 0..times {
                *cursor = vi_word_backward(buffer, *cursor);
            }
        }
        'e' => {
            for _ in 0..times {
                *cursor = vi_word_end(buffer, *cursor).saturating_sub(1).max(*cursor);
            }
        }
        'j' => {
            for _ in 0..times {
                editor.recall_older(buffer, cursor, shell);
                *cursor = cursor.saturating_sub(1);
            }
        }
        'k' => {
            for _ in 0..times {
                editor.recall_newer(buffer, cursor, shell);
                *cursor = cursor.saturating_sub(1);
            }
        }
        'd' | 'c' => {
            *pending = Some(char);
            return;
        }
        'x' => {
            let end = (*cursor + times).min(buffer.len());
            editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
        }
        'D' => {
            let end = line_end(buffer, *cursor);
            editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
        }
        'C' => {
            let end = line_end(buffer, *cursor);
            editor.vi_kill_range(buffer, cursor, *cursor, end, undo);
            *vi_normal = false;
        }
        'p' => editor.vi_paste(buffer, cursor, false, undo),
        'P' => editor.vi_paste(buffer, cursor, true, undo),
        'u' => {
            if let Some((previous, position)) = undo.pop() {
                *buffer = previous;
                *cursor = position.min(buffer.len());
            }
        }
        'r' => {
            *replace = true;
            return;
        }
        _ => {}
    }
    *count = 0;
}

fn vi_word_forward(buffer: &[char], cursor: usize) -> usize {
    let mut index = cursor.min(buffer.len());
    while index < buffer.len() && is_word_char(buffer[index]) {
        index += 1;
    }
    while index < buffer.len() && !is_word_char(buffer[index]) && buffer[index] != '\n' {
        index += 1;
    }
    index
}

fn vi_word_backward(buffer: &[char], cursor: usize) -> usize {
    let mut index = cursor.min(buffer.len());
    while index > 0 && !is_word_char(buffer[index - 1]) && buffer[index - 1] != '\n' {
        index -= 1;
    }
    while index > 0 && is_word_char(buffer[index - 1]) {
        index -= 1;
    }
    index
}

fn vi_line_end(buffer: &[char], cursor: usize) -> usize {
    let end = line_end(buffer, cursor);
    if end > line_start(buffer, cursor) {
        end - 1
    } else {
        end
    }
}

fn vi_word_end(buffer: &[char], cursor: usize) -> usize {
    let mut index = cursor.min(buffer.len());
    while index < buffer.len() && !is_word_char(buffer[index]) && buffer[index] != '\n' {
        index += 1;
    }
    while index < buffer.len() && is_word_char(buffer[index]) {
        index += 1;
    }
    index
}

impl Editor {
    fn vi_kill_range(
        &mut self,
        buffer: &mut Vec<char>,
        cursor: &mut usize,
        start: usize,
        end: usize,
        undo: &mut Vec<(Vec<char>, usize)>,
    ) {
        if start >= end || end > buffer.len() {
            return;
        }
        push_undo(undo, buffer, *cursor);
        self.kill = buffer[start..end].iter().collect();
        buffer.drain(start..end);
        if *cursor >= end {
            *cursor -= end - start;
        } else if *cursor > start {
            *cursor = start;
        }
    }

    fn vi_delete_lines(
        &mut self,
        buffer: &mut Vec<char>,
        cursor: &mut usize,
        count: usize,
        undo: &mut Vec<(Vec<char>, usize)>,
    ) {
        let start = line_start(buffer, *cursor);
        let mut end = start;
        for _ in 0..count {
            end = line_end(buffer, end);
            if end < buffer.len() && buffer[end] == '\n' {
                end += 1;
            }
        }
        self.vi_kill_range(buffer, cursor, start, end, undo);
        *cursor = (*cursor).min(buffer.len());
    }

    fn vi_paste(
        &mut self,
        buffer: &mut Vec<char>,
        cursor: &mut usize,
        before: bool,
        undo: &mut Vec<(Vec<char>, usize)>,
    ) {
        if self.kill.is_empty() {
            return;
        }
        push_undo(undo, buffer, *cursor);
        let at = if before {
            *cursor
        } else {
            (*cursor + 1).min(buffer.len())
        };
        let text: Vec<char> = self.kill.chars().collect();
        buffer.splice(at..at, text.iter().copied());
        *cursor = at + text.len();
    }
}

fn find_suggestion(entries: &[String], buffer: &[char], cursor: usize) -> Option<String> {
    if buffer.is_empty() || cursor != buffer.len() {
        return None;
    }
    let prefix: String = buffer.iter().collect();
    entries
        .iter()
        .rev()
        .find(|entry| entry.len() > prefix.len() && entry.starts_with(&prefix))
        .map(|entry| entry[prefix.len()..].to_string())
}

fn autosuggest(shell: &Shell, buffer: &[char], cursor: usize, prompt_width: usize) -> Option<String> {
    if shell.setting("autosuggest") == Some("off") {
        return None;
    }
    let suffix = find_suggestion(shell.history_entries(), buffer, cursor)?;
    if suffix.contains('\n') || suffix.contains('\t') {
        return None;
    }
    let columns = term::width().max(1);
    let (_, column) = position(buffer, cursor, columns, prompt_width);
    if column + suffix.chars().count() >= columns {
        return None;
    }
    Some(suffix)
}

fn position(buffer: &[char], index: usize, columns: usize, prompt_width: usize) -> (usize, usize) {
    let columns = columns.max(1);
    let mut row = 0usize;
    let mut column = 0usize;
    let mut pending = false;

    for _ in 0..prompt_width {
        if pending {
            row += 1;
            column = 0;
            pending = false;
        }
        if column + 1 >= columns {
            column = columns - 1;
            pending = true;
        } else {
            column += 1;
        }
    }

    for char in buffer.iter().take(index) {
        if *char == '\n' {
            row += 1;
            column = 0;
            pending = false;
            continue;
        }
        if pending {
            row += 1;
            column = 0;
            pending = false;
        }
        if column + 1 >= columns {
            column = columns - 1;
            pending = true;
        } else {
            column += 1;
        }
    }

    (row, column)
}
fn line_start(buffer: &[char], cursor: usize) -> usize {
    buffer[..cursor.min(buffer.len())]
        .iter()
        .rposition(|char| *char == '\n')
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn line_end(buffer: &[char], cursor: usize) -> usize {
    buffer[cursor.min(buffer.len())..]
        .iter()
        .position(|char| *char == '\n')
        .map(|index| cursor.min(buffer.len()) + index)
        .unwrap_or(buffer.len())
}

fn move_line(buffer: &[char], cursor: usize, up: bool) -> usize {
    let start = line_start(buffer, cursor);
    let end = line_end(buffer, cursor);
    let column = cursor.saturating_sub(start);

    if up {
        if start == 0 {
            return cursor;
        }
        let previous_end = start - 1;
        let previous_start = line_start(buffer, previous_end);
        previous_start + column.min(previous_end - previous_start)
    } else {
        if end >= buffer.len() {
            return cursor;
        }
        let next_start = end + 1;
        let next_end = line_end(buffer, next_start);
        next_start + column.min(next_end - next_start)
    }
}
fn find_history(entries: &[String], query: &str, offset: usize) -> Option<String> {
    entries
        .iter()
        .rev()
        .filter(|entry| entry.contains(query))
        .nth(offset)
        .cloned()
}

fn expand_abbreviation(
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    shell: &Shell,
    separator: &str,
) -> bool {
    if *cursor != buffer.len() {
        return false;
    }

    let text: String = buffer.iter().collect();
    let start = text.len() - text.trim_start().len();
    let token = text[start..].trim_end();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return false;
    }

    let Some(expansion) = shell.abbreviation(token) else {
        return false;
    };
    let separator = if expansion.ends_with(' ') { "" } else { separator };
    let expanded = format!("{}{expansion}{separator}", &text[..start]);
    *buffer = expanded.chars().collect();
    *cursor = buffer.len();
    true
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

fn fuzzy_files(shell: &Shell) -> std::io::Result<Option<String>> {
    let files = collect_files(&shell.cwd, 500);
    let mut query = String::new();
    let mut selected = 0usize;

    loop {
        let mut matches: Vec<(u32, String)> = files
            .iter()
            .filter_map(|path| {
                fuzzy_score(path, &query).map(|score| (score + recency_bonus(shell, path), path.clone()))
            })
            .collect();
        matches.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));

        let columns = term::width().max(10);
        let shown = matches.len().min(10);
        if selected >= shown {
            selected = shown.saturating_sub(1);
        }

        let mut screen = String::from("\x1b[2J\x1b[H");
        screen.push_str(&format!("fuzzy> {query}\r\n"));
        for (index, (_, path)) in matches.iter().take(shown).enumerate() {
            let marker = if index == selected { "> " } else { "  " };
            let available = columns.saturating_sub(marker.chars().count() + 1);
            let text: String = path.chars().take(available).collect();
            screen.push_str(marker);
            screen.push_str(&text);
            screen.push_str("\r\n");
        }
        if matches.len() > shown {
            screen.push_str(&format!("  (+{} more)\r\n", matches.len() - shown));
        }
        if files.len() >= 500 {
            screen.push_str("  (file list capped)\r\n");
        }
        term::write_stdout(&screen)?;

        match term::read_key()? {
            Key::Enter => return Ok(matches.get(selected).map(|(_, path)| path.clone())),
            Key::Escape | Key::Ctrl('c') => return Ok(None),
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => {
                selected = (selected + 1).min(shown.saturating_sub(1));
            }
            Key::Backspace => {
                query.pop();
                selected = 0;
            }
            Key::Ctrl('u') => {
                query.clear();
                selected = 0;
            }
            Key::Char(char) => {
                query.push(char);
                selected = 0;
            }
            _ => {}
        }
    }
}

fn recency_bonus(shell: &Shell, path: &str) -> u32 {
    let recent = shell
        .history_entries()
        .iter()
        .rev()
        .take(50)
        .any(|entry| entry.split_whitespace().any(|word| word == path));
    if recent { 200 } else { 0 }
}

fn fuzzy_score(text: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }

    let mut score = 0u32;
    let mut cursor = 0usize;
    let mut last: Option<usize> = None;
    let mut first = None;

    for needle in query.chars() {
        let found = text[cursor..].char_indices().find(|(_, char)| *char == needle)?;
        let position = cursor + found.0;
        if first.is_none() {
            first = Some(position);
        }
        score += if last.is_some_and(|last| last + 1 == position) {
            10
        } else {
            2
        };
        last = Some(position);
        cursor = position + needle.len_utf8();
    }

    let start = first.unwrap_or(0) as u32;
    Some(score.saturating_sub(start).saturating_sub(text.len() as u32 / 8))
}

fn collect_files(root: &std::path::Path, cap: usize) -> Vec<String> {
    let mut files = Vec::new();
    collect_files_in(root, root, cap, 0, &mut files);
    files
}

fn collect_files_in(
    root: &std::path::Path,
    dir: &std::path::Path,
    cap: usize,
    depth: usize,
    files: &mut Vec<String>,
) {
    if files.len() >= cap || depth > 8 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        if files.len() >= cap {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || matches!(name.as_str(), "target" | "node_modules") {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files_in(root, &path, cap, depth + 1, files);
        } else if let Ok(relative) = path.strip_prefix(root) {
            files.push(relative.to_string_lossy().into_owned());
        }
    }
}

fn render_menu(shell: &Shell, menu: &CompletionMenu, columns: usize) -> (String, usize) {
    const MAX_ROWS: usize = 8;
    let total = menu.candidates.len();
    let shown = total.min(MAX_ROWS);
    let start = menu
        .selected
        .saturating_sub(shown / 2)
        .min(total.saturating_sub(shown));
    let end = (start + shown).min(total);
    let width = columns.saturating_sub(1).max(1);
    let mut rows = 0;
    let mut out = String::new();

    let mut push_row = |out: &mut String, line: String| {
        if rows > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&line);
        rows += 1;
    };

    if start > 0 {
        push_row(&mut out, format!("  (+{start} above)"));
    }
    for (index, candidate) in menu.candidates[start..end].iter().enumerate() {
        let index = start + index;
        let marker = if index == menu.selected { "> " } else { "  " };
        let line: String = format!("{marker}{candidate}")
            .chars()
            .take(width)
            .collect();
        if index == menu.selected {
            push_row(&mut out, prompt::painted(shell, "selection", &line));
        } else {
            push_row(&mut out, line);
        }
    }
    if end < total {
        push_row(&mut out, format!("  (+{} more)", total - end));
    }

    (out, rows)
}

struct CompletionMenu {
    start: usize,
    candidates: Vec<String>,
    selected: usize,
    escape: bool,
}

fn complete_menu(buffer: &mut Vec<char>, cursor: &mut usize, shell: &Shell) -> Option<CompletionMenu> {
    let mut start = *cursor;
    while start > 0 && !buffer[start - 1].is_whitespace() {
        start -= 1;
    }

    let word: String = buffer[start..*cursor].iter().collect();
    if word.is_empty() {
        return None;
    }

    let before: String = buffer[..start].iter().collect();
    let command_position = before
        .chars()
        .rev()
        .find(|char| !char.is_whitespace())
        .map(|char| matches!(char, '|' | ';' | '&' | '(' | '{'))
        .unwrap_or(true);

    let mut escape = false;
    let mut candidates = if word.starts_with('$') {
        variable_candidates(&word, shell)
    } else if let Some(candidates) = git_branch_candidates(&word, &before, &shell.cwd) {
        candidates
    } else if word.contains('/') || !command_position {
        escape = true;
        file_candidates(&word, shell)
    } else {
        command_candidates(&word, shell)
    };

    candidates.sort();
    candidates.dedup();

    let first = candidates.first()?.clone();

    let common = candidates
        .iter()
        .fold(first.clone(), |prefix, candidate| common_prefix(&prefix, candidate));

    if candidates.len() == 1 {
        let candidate = &candidates[0];
        let suffix = &candidate[word.len()..];
        let trailing = if candidate.ends_with('/') {
            ""
        } else if std::fs::metadata(candidate).map(|meta| meta.is_dir()).unwrap_or(false) {
            "/"
        } else if candidate.starts_with('$') {
            ""
        } else {
            " "
        };
        let inserted = if escape {
            escape_completion(suffix)
        } else {
            suffix.to_string()
        };
        insert_text(buffer, cursor, &format!("{inserted}{trailing}"));
        return None;
    }

    if common.len() > word.len() {
        let suffix = &common[word.len()..];
        let inserted = if escape {
            escape_completion(suffix)
        } else {
            suffix.to_string()
        };
        insert_text(buffer, cursor, &inserted);
    }

    if term::width() < 20 {
        return None;
    }

    Some(CompletionMenu {
        start,
        candidates,
        selected: 0,
        escape,
    })
}

fn escape_completion(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, char) in text.chars().enumerate() {
        if index == 0 && char == '~' {
            out.push(char);
            continue;
        }
        if char.is_whitespace()
            || matches!(
                char,
                '\\' | '\''
                    | '"'
                    | '`'
                    | '$'
                    | '&'
                    | ';'
                    | '|'
                    | '<'
                    | '>'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '?'
                    | '!'
                    | '#'
                    | '~'
            )
        {
            out.push('\\');
        }
        out.push(char);
    }
    out
}

fn insert_text(buffer: &mut Vec<char>, cursor: &mut usize, text: &str) {
    for (offset, char) in text.chars().enumerate() {
        buffer.insert(*cursor + offset, char);
    }
    *cursor += text.chars().count();
}

fn insert_selection(
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    menu: &CompletionMenu,
) {
    let Some(candidate) = menu.candidates.get(menu.selected) else {
        return;
    };
    let end = (*cursor).min(buffer.len());
    buffer.drain(menu.start..end);
    *cursor = menu.start;
    let text = if menu.escape {
        escape_completion(candidate)
    } else {
        candidate.clone()
    };
    insert_text(buffer, cursor, &text);
}

fn variable_candidates(word: &str, shell: &Shell) -> Vec<String> {
    let mut names = shell.variable_names();
    names.sort();
    names.dedup();

    match word.strip_prefix("${") {
        Some(prefix) => names
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("${{{name}}}"))
            .collect(),
        None => {
            let prefix = word.trim_start_matches('$');
            names
                .into_iter()
                .filter(|name| name.starts_with(prefix))
                .map(|name| format!("${name}"))
                .collect()
        }
    }
}

fn git_branch_candidates(word: &str, before: &str, cwd: &std::path::Path) -> Option<Vec<String>> {
    let words: Vec<&str> = before.split_whitespace().collect();
    if words.first().copied() != Some("git") {
        return None;
    }
    let subcommand = words.get(1).copied()?;
    let wanted = match subcommand {
        "checkout" | "switch" | "merge" | "rebase" => true,
        "branch" => words.iter().any(|word| *word == "-d" || *word == "-D"),
        _ => false,
    };
    if !wanted {
        return None;
    }

    let root = git_root(cwd)?;
    let mut branches = Vec::new();
    let heads = root.join(".git/refs/heads");
    collect_branch_dir(&heads, &heads, &mut branches);
    if let Ok(packed) = std::fs::read_to_string(root.join(".git/packed-refs")) {
        for line in packed.lines() {
            let Some((_, reference)) = line.split_once(' ') else {
                continue;
            };
            if let Some(name) = reference.strip_prefix("refs/heads/") {
                branches.push(name.to_string());
            }
        }
    }
    branches.retain(|branch| branch.starts_with(word));
    Some(branches)
}

fn git_root(start: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn collect_branch_dir(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_branch_dir(root, &path, out);
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_string_lossy().into_owned());
        }
    }
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

fn file_candidates(word: &str, shell: &Shell) -> Vec<String> {
    let home = shell.var("HOME");
    let lookup = match word.strip_prefix('~') {
        Some(rest) => format!("{home}{rest}"),
        None => word.to_string(),
    };

    let (directory, base) = match lookup.rfind('/') {
        Some(index) => (&lookup[..index], &lookup[index + 1..]),
        None => (".", lookup.as_str()),
    };
    let display_end = word.rfind('/').map(|index| index + 1).unwrap_or(0);
    let display = word[..display_end].to_string();

    let directory = if directory.is_empty() { "/" } else { directory };
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with(base) || (name.starts_with('.') && !base.starts_with('.')) {
                return None;
            }
            let suffix = if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                "/"
            } else {
                ""
            };
            Some(if display.is_empty() {
                format!("{name}{suffix}")
            } else {
                format!("{display}{name}{suffix}")
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

#[cfg(test)]
mod tests {
    use super::{BoundKey, line_end, line_start, move_line, parse_bound, position};

    fn buffer(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn multibyte_single_character_bindings_parse() {
        assert_eq!(parse_bound("界"), Some(BoundKey::Char('界')));
        assert_eq!(parse_bound("m-界"), Some(BoundKey::Alt('界')));
    }

    #[test]
    fn position_accounts_for_newlines_and_wrapping() {
        let text = buffer("ab\ncd");
        assert_eq!(position(&text, 0, 10, 4), (0, 4));
        assert_eq!(position(&text, 2, 10, 4), (0, 6));
        assert_eq!(position(&text, 3, 10, 4), (1, 0));
        assert_eq!(position(&text, 5, 10, 4), (1, 2));

        let wide = buffer("abcdefgh");
        assert_eq!(position(&wide, 4, 4, 0), (0, 3));
        assert_eq!(position(&wide, 5, 4, 0), (1, 1));
        assert_eq!(position(&wide, 8, 4, 0), (1, 3));
    }

    #[test]
    fn line_bounds_stay_inside_the_current_line() {
        let text = buffer("one\ntwo\nthree");
        assert_eq!(line_start(&text, 5), 4);
        assert_eq!(line_end(&text, 5), 7);
        assert_eq!(line_start(&text, 0), 0);
        assert_eq!(line_end(&text, text.len()), text.len());
    }

    #[test]
    fn move_line_keeps_the_column_and_clamps() {
        let text = buffer("one\ntwo\nthree");
        assert_eq!(move_line(&text, 8, true), 4);
        assert_eq!(move_line(&text, 3, false), 7);
        assert_eq!(move_line(&text, 1, true), 1);
        assert_eq!(move_line(&text, text.len(), false), text.len());
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::find_suggestion;

    fn entries() -> Vec<String> {
        vec![
            "echo alpha".to_string(),
            "ls".to_string(),
            "echo beta".to_string(),
        ]
    }

    #[test]
    fn finds_the_newest_longer_prefix_match() {
        let buffer: Vec<char> = "echo".chars().collect();
        assert_eq!(find_suggestion(&entries(), &buffer, 4), Some(" beta".to_string()));

        let buffer: Vec<char> = "echo beta".chars().collect();
        assert_eq!(find_suggestion(&entries(), &buffer, 9), None);
    }

    #[test]
    fn ignores_mid_line_and_empty_buffers() {
        let buffer: Vec<char> = "echo".chars().collect();
        assert_eq!(find_suggestion(&entries(), &buffer, 2), None);
        assert_eq!(find_suggestion(&entries(), &[], 0), None);
        assert_eq!(find_suggestion(&entries(), &"zzz".chars().collect::<Vec<_>>(), 3), None);
    }
}


#[cfg(test)]
mod fuzzy_tests {
    use super::fuzzy_score;

    #[test]
    fn ranks_contiguous_and_shorter_matches_higher() {
        let beta = fuzzy_score("beta.txt", "bet").expect("beta");
        let alphabet = fuzzy_score("alphabet.txt", "bet").expect("alphabet");
        assert!(beta > alphabet, "{beta} <= {alphabet}");
        assert!(fuzzy_score("src/editor.rs", "edr").is_some());
        assert!(fuzzy_score("alpha", "zzz").is_none());
    }
}

#[cfg(test)]
mod action_tests {
    use super::{Action, Editor, Outcome, Shell};

    fn apply(action: Action, text: &str, cursor: usize) -> (String, usize, Outcome) {
        apply_with_suggestion(action, text, cursor, None)
    }

    fn apply_with_suggestion(
        action: Action,
        text: &str,
        cursor: usize,
        suggestion: Option<&str>,
    ) -> (String, usize, Outcome) {
        let mut editor = Editor::new();
        let shell = Shell::new();
        let mut buffer: Vec<char> = text.chars().collect();
        let mut cursor = cursor;
        let mut undo = Vec::new();
        let outcome = editor
            .apply_action(
                action,
                &mut buffer,
                &mut cursor,
                suggestion,
                "",
                &shell,
                &mut undo,
            )
            .unwrap();
        (buffer.iter().collect(), cursor, outcome)
    }

    #[test]
    fn character_and_word_motions() {
        assert_eq!(apply(Action::BackwardChar, "abc", 2).1, 1);
        assert_eq!(apply(Action::ForwardChar, "abc", 1).1, 2);
        assert_eq!(apply(Action::BackwardWord, "echo one two", 12).1, 9);
        assert_eq!(apply(Action::ForwardWord, "echo one two", 0).1, 4);
        assert_eq!(apply(Action::BeginningOfLine, "one\ntwo", 6).1, 4);
        assert_eq!(apply(Action::EndOfLine, "one\ntwo", 4).1, 7);
    }

    #[test]
    fn deletion_kill_and_yank() {
        assert_eq!(apply(Action::DeleteChar, "abc", 1).0, "ac");
        assert_eq!(apply(Action::BackwardDeleteChar, "abc", 1).0, "bc");

        let mut editor = Editor::new();
        let shell = Shell::new();
        let mut buffer: Vec<char> = "one two".chars().collect();
        let mut cursor = 3;
        let mut undo = Vec::new();
        editor
            .apply_action(
                Action::KillLine,
                &mut buffer,
                &mut cursor,
                None,
                "",
                &shell,
                &mut undo,
            )
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "one");
        cursor = buffer.len();
        editor
            .apply_action(
                Action::Yank,
                &mut buffer,
                &mut cursor,
                None,
                "",
                &shell,
                &mut undo,
            )
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "one two");
    }

    #[test]
    fn transpose_and_case_conversion() {
        assert_eq!(apply(Action::TransposeChars, "abc", 2).0, "bac");
        assert_eq!(apply(Action::TransposeChars, "abc", 3).0, "acb");
        assert_eq!(apply(Action::UpcaseWord, "one two", 0).0, "ONE two");
        assert_eq!(apply(Action::DowncaseWord, "ONE TWO", 0).0, "one TWO");
        assert_eq!(apply(Action::CapitalizeWord, "oNE two", 0).0, "One two");
    }

    #[test]
    fn undo_restores_the_last_edit() {
        let mut editor = Editor::new();
        let shell = Shell::new();
        let mut buffer: Vec<char> = "abc".chars().collect();
        let mut cursor = 1;
        let mut undo = Vec::new();
        editor
            .apply_action(
                Action::DeleteChar,
                &mut buffer,
                &mut cursor,
                None,
                "",
                &shell,
                &mut undo,
            )
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "ac");
        editor
            .apply_action(
                Action::Undo,
                &mut buffer,
                &mut cursor,
                None,
                "",
                &shell,
                &mut undo,
            )
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "abc");
    }

    #[test]
    fn suggestion_actions_only_insert_when_at_the_end() {
        assert_eq!(
            apply_with_suggestion(Action::AcceptSuggestion, "echo", 4, Some(" ok")).0,
            "echo ok"
        );
        assert_eq!(
            apply_with_suggestion(Action::ForwardChar, "echo", 4, Some(" ok")).0,
            "echo ok"
        );
        assert_eq!(apply_with_suggestion(Action::AcceptSuggestion, "echo", 2, Some(" ok")).0, "echo");
    }

    #[test]
    fn submit_and_abort_outcomes() {
        assert!(matches!(apply(Action::AcceptLine, "", 0).2, Outcome::Submit));
        assert!(matches!(apply(Action::Abort, "", 0).2, Outcome::Abort));
    }
}
