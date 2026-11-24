use std::collections::HashMap;
use std::path::Path;

use crate::highlight;
use crate::history::Entry as HistoryEntry;
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
    prefix_active: bool,
}

impl Editor {
    pub fn new() -> Self {
        Editor {
            index: 0,
            draft: String::new(),
            kill: String::new(),
            prefix_active: false,
        }
    }

    pub fn read_line(
        &mut self,
        raw_prompt: &str,
        input_marker: &str,
        rprompt: Option<&str>,
        shell: &mut Shell,
    ) -> std::io::Result<Line> {
        let Some(_raw) = term::RawMode::enable() else {
            return fallback_line();
        };
        let mut cursor_guard = CursorGuard::new(shell.setting("cursor"));
        cursor_guard.apply(false)?;

        let mut buffer: Vec<char> = Vec::new();
        let mut cursor = 0;
        self.index = shell.history_entries().len();
        self.draft.clear();
        self.prefix_active = false;

        let mut rendered_cols = term::width().max(1);
        let mut prompt = format_prompt(raw_prompt, input_marker, rendered_cols, shell);
        let mut prompt_width = last_line_width(&prompt);
        let mut prompt_last = prompt.rsplit('\n').next().unwrap_or("").to_string();
        let rprompt_clean = rprompt
            .map(|text| text.replace(['\r', '\n'], ""))
            .filter(|text| !text.is_empty());
        let rprompt_width = rprompt_clean.as_deref().map(last_line_width).unwrap_or(0);
        term::write_stdout(&prompt)?;

        let bindings = build_bindings(shell);
        let mut undo: Vec<(Vec<char>, usize)> = Vec::new();
        let mut menu: Option<CompletionMenu> = None;
        let mut vi_normal = false;
        let mut vi_count = 0usize;
        let mut vi_pending: Option<char> = None;
        let mut vi_replace = false;
        let mut last_row = prompt_width / rendered_cols;

        loop {
            cursor_guard.apply(vi_normal)?;
            // A fatal signal ends the shell with its EXIT action; there is no
            // line left to keep, so hand control back to the REPL, which runs
            // the action and leaves with 128+signal.
            if crate::signal::fatal_signal().is_some() {
                term::write_stdout("\r\n")?;
                return Ok(Line::Eof);
            }
            if crate::signal::take_winch_pending() {
                let new_cols = term::width().max(1);
                if new_cols != rendered_cols {
                    let upper_rows = prompt_upper_rows(&prompt, new_cols);
                    let (cursor_row_after_reflow, _) =
                        position(&buffer, cursor, new_cols, prompt_width);
                    let move_up = upper_rows.saturating_add(cursor_row_after_reflow);

                    let new_prompt = format_prompt(raw_prompt, input_marker, new_cols, shell);
                    let mut out = String::new();
                    if move_up > 0 {
                        out.push_str(&format!("\x1b[{move_up}A"));
                    }
                    out.push_str("\r\x1b[J");
                    out.push_str(&new_prompt);
                    term::write_stdout(&out)?;

                    prompt = new_prompt;
                    prompt_width = last_line_width(&prompt);
                    prompt_last = prompt.rsplit('\n').next().unwrap_or("").to_string();
                    rendered_cols = new_cols;
                    last_row = prompt_width / rendered_cols;
                }
            }
            // A background job finished while the prompt was idle. The notice
            // is printed above the input and the line is redrawn intact by the
            // render below, which is why the reaping happens here and not in
            // the signal handler.
            if crate::signal::take_child_pending() {
                let notices = shell.take_job_notices();
                if !notices.is_empty() {
                    let columns = term::width().max(1);
                    let prompt_row = prompt_width / columns;
                    let mut out = String::new();
                    if last_row > prompt_row {
                        out.push_str(&format!("\x1b[{}A", last_row - prompt_row));
                    }
                    out.push_str("\r\x1b[J");
                    for notice in &notices {
                        out.push_str(notice);
                        out.push_str("\r\n");
                    }
                    out.push_str(&prompt);
                    term::write_stdout(&out)?;
                    last_row = prompt_row;
                }
            }
            if menu.is_some() && term::width() < 20 {
                menu = None;
            }
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
            last_row = self.render_input(
                &buffer,
                cursor,
                prompt_width,
                rprompt_clean.as_deref(),
                rprompt_width,
                &ghost,
                &display,
                &menu_text,
                menu_rows,
                last_row,
            )?;

            let key = if shell.has_substitutions() {
                match term::read_key_timeout(200)? {
                    Some(key) => key,
                    None => {
                        shell.reap_substitutions_idle();
                        continue;
                    }
                }
            } else {
                term::read_key()?
            };

            if key == Key::PasteStart {
                let before = buffer.clone();
                let bytes = term::read_paste()?;
                insert_paste(&mut buffer, &mut cursor, &bytes, &mut undo);
                if buffer != before {
                    self.reset_history_nav(shell);
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
                        let before = buffer.clone();
                        if let Some(active) = menu.take() {
                            insert_selection(&mut buffer, &mut cursor, &active);
                        }
                        if buffer != before {
                            self.reset_history_nav(shell);
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
                        finish_submitted_line(
                            &prompt,
                            rendered_cols,
                            prompt_width,
                            &buffer,
                            cursor,
                            input_marker,
                            shell,
                        )?;
                        return Ok(Line::Content(buffer.iter().collect()));
                    }
                    Key::Backspace => {
                        let before = buffer.clone();
                        if cursor < buffer.len() {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor);
                        } else if cursor > 0 {
                            push_undo(&mut undo, &buffer, cursor);
                            buffer.remove(cursor - 1);
                            cursor -= 1;
                        }
                        if buffer != before {
                            self.reset_history_nav(shell);
                        }
                        vi_count = 0;
                        continue;
                    }
                    Key::Ctrl('r') => {
                        let before = buffer.clone();
                        if history_search(prompt_width, &prompt_last, shell, &mut buffer, &mut cursor)?
                        {
                            finish_submitted_line(
                                &prompt,
                                rendered_cols,
                                prompt_width,
                                &buffer,
                                cursor,
                                input_marker,
                                shell,
                            )?;
                            return Ok(Line::Content(buffer.iter().collect()));
                        }
                        if buffer != before {
                            self.reset_history_nav(shell);
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
                    Key::Up => {
                        cursor = move_line(&buffer, cursor, true);
                        vi_count = 0;
                        continue;
                    }
                    Key::Down => {
                        cursor = move_line(&buffer, cursor, false);
                        vi_count = 0;
                        continue;
                    }
                    Key::Char(char) => {
                        let before = buffer.clone();
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
                        if !matches!(char, 'j' | 'k') && buffer != before {
                            self.reset_history_nav(shell);
                        }
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
                    &prompt,
                    shell,
                    &mut undo,
                )? {
                    Outcome::Continue => continue,
                    Outcome::Submit => {
                        expand_abbreviation(&mut buffer, &mut cursor, shell, "");
                        finish_submitted_line(
                            &prompt,
                            rendered_cols,
                            prompt_width,
                            &buffer,
                            cursor,
                            input_marker,
                            shell,
                        )?;
                        return Ok(Line::Content(buffer.iter().collect()));
                    }
                    Outcome::Abort => {
                        term::write_stdout("^C\r\n")?;
                        return Ok(Line::Interrupted);
                    }
                    Outcome::HistorySearch => {
                        let before = buffer.clone();
                        if history_search(prompt_width, &prompt_last, shell, &mut buffer, &mut cursor)?
                        {
                            finish_submitted_line(
                                &prompt,
                                rendered_cols,
                                prompt_width,
                                &buffer,
                                cursor,
                                input_marker,
                                shell,
                            )?;
                            return Ok(Line::Content(buffer.iter().collect()));
                        }
                        if buffer != before {
                            self.reset_history_nav(shell);
                        }
                        continue;
                    }
                    Outcome::Complete => {
                        let before = buffer.clone();
                        menu = complete_menu(&mut buffer, &mut cursor, shell);
                        if buffer != before {
                            self.reset_history_nav(shell);
                        }
                        continue;
                    }
                    Outcome::FuzzyFiles => {
                        let before = buffer.clone();
                        term::enter_alternate_screen()?;
                        let selected = fuzzy_files(shell);
                        term::leave_alternate_screen()?;
                        if let Some(path) = selected? {
                            push_undo(&mut undo, &buffer, cursor);
                            insert_text(&mut buffer, &mut cursor, &escape_completion(&path));
                        }
                        if buffer != before {
                            self.reset_history_nav(shell);
                        }
                        continue;
                    }
                    Outcome::DirBack | Outcome::DirForward => {
                        menu = None;
                        let changed = if matches!(action, Action::DirBack) {
                            shell.dir_back()
                        } else {
                            shell.dir_forward()
                        };
                        if changed {
                            let raw_new = if input_marker.is_empty() {
                                prompt::render_continuation(shell)
                            } else {
                                prompt::render_raw(shell)
                            };
                            let upper_rows = prompt_upper_rows(&prompt, rendered_cols);
                            let (cursor_row, _) =
                                position(&buffer, cursor, rendered_cols, prompt_width);
                            let move_up = upper_rows.saturating_add(cursor_row);
                            let new_prompt =
                                format_prompt(&raw_new, input_marker, rendered_cols, shell);
                            let mut out = String::new();
                            if move_up > 0 {
                                out.push_str(&format!("\x1b[{move_up}A"));
                            }
                            out.push_str("\r\x1b[J");
                            out.push_str(&new_prompt);
                            term::write_stdout(&out)?;
                            prompt = new_prompt;
                            prompt_width = last_line_width(&prompt);
                            prompt_last = prompt.rsplit('\n').next().unwrap_or("").to_string();
                            last_row = prompt_width / rendered_cols;
                        }
                        continue;
                    }
                }
            }

            let before = buffer.clone();
            match key {
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
            if buffer != before {
                self.reset_history_nav(shell);
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
        let before = buffer.clone();
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
            Action::ForwardWord => {
                if *cursor == buffer.len()
                    && let Some(tail) = suggestion
                    && !tail.is_empty()
                {
                    let chars: Vec<char> = tail.chars().collect();
                    let take = forward_word(&chars, 0);
                    push_undo(undo, buffer, *cursor);
                    buffer.extend_from_slice(&chars[..take]);
                    *cursor = buffer.len();
                } else {
                    *cursor = forward_word(buffer, *cursor);
                }
            }
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
                if buffer.len() >= 2 && *cursor >= 1 {
                    push_undo(undo, buffer, *cursor);
                    if *cursor == buffer.len() {
                        buffer.swap(*cursor - 2, *cursor - 1);
                    } else {
                        buffer.swap(*cursor - 1, *cursor);
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
            Action::DirBack => return Ok(Outcome::DirBack),
            Action::DirForward => return Ok(Outcome::DirForward),
        }
        if !matches!(action, Action::HistoryPrev | Action::HistoryNext) && *buffer != before {
            self.reset_history_nav(shell);
        }
        Ok(Outcome::Continue)
    }

    fn reset_history_nav(&mut self, shell: &Shell) {
        self.prefix_active = false;
        self.index = shell.history_entries().len();
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
        if history.is_empty() || self.index == 0 {
            return;
        }
        let starting = self.index == history.len();
        if starting {
            self.draft = buffer.iter().collect();
            self.prefix_active = !self.draft.is_empty();
        }
        if self.prefix_active {
            let current: String = buffer.iter().collect();
            let mut next = self.index;
            while next > 0 {
                next -= 1;
                if history[next].starts_with(&self.draft) && history[next] != current {
                    self.index = next;
                    *buffer = history[next].chars().collect();
                    *cursor = buffer.len();
                    return;
                }
            }
            if starting {
                self.prefix_active = false;
            }
            return;
        }
        self.index -= 1;
        *buffer = history[self.index].chars().collect();
        *cursor = buffer.len();
    }

    fn recall_older(&mut self, buffer: &mut Vec<char>, cursor: &mut usize, shell: &Shell) {
        let history = shell.history_entries();
        if self.index >= history.len() {
            return;
        }
        if self.prefix_active {
            let current: String = buffer.iter().collect();
            let mut next = self.index + 1;
            while next < history.len() {
                if history[next].starts_with(&self.draft) && history[next] != current {
                    self.index = next;
                    *buffer = history[next].chars().collect();
                    *cursor = buffer.len();
                    return;
                }
                next += 1;
            }
            self.index = history.len();
            self.prefix_active = false;
            *buffer = self.draft.chars().collect();
            *cursor = buffer.len();
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
        rprompt: Option<&str>,
        rprompt_width: usize,
        ghost: &str,
        display: &str,
        menu_text: &str,
        menu_rows: usize,
        previous_row: usize,
    ) -> std::io::Result<usize> {
        let columns = term::width().max(1);
        let (cursor_row, cursor_column) = position(buffer, cursor, columns, prompt_width);
        let (end_row, _) = position(buffer, buffer.len(), columns, prompt_width);
        let prompt_row = prompt_width / columns;
        let start_column = prompt_width % columns;

        let mut out = String::new();
        if previous_row > prompt_row {
            out.push_str(&format!("\x1b[{}A", previous_row - prompt_row));
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

        if end_row == prompt_row
            && !buffer.contains(&'\n')
            && !buffer.contains(&'\t')
            && let Some(rprompt_text) = rprompt
        {
            let buf_width: usize = buffer.iter().map(|ch| char_width(*ch)).sum();
            let ghost_width = last_line_width(ghost);
            let input_end_col = start_column
                .saturating_add(buf_width)
                .saturating_add(ghost_width);
            if let Some(rprompt_col) = rprompt_column(columns, input_end_col, rprompt_width) {
                out.push('\r');
                if rprompt_col > 0 {
                    out.push_str(&format!("\x1b[{rprompt_col}C"));
                }
                out.push_str(rprompt_text);
                // Reset SGR and clear pending-wrap state on `prompt_row` without advancing lines.
                out.push_str("\x1b[0m\r");
            }
        }

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

        term::write_stdout(&out)?;
        Ok(cursor_row)
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
    DirBack,
    DirForward,
}

#[derive(Debug, Eq, PartialEq)]
enum Outcome {
    Continue,
    Submit,
    Abort,
    HistorySearch,
    Complete,
    FuzzyFiles,
    DirBack,
    DirForward,
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
    ("alt-left", "dir_back"),
    ("alt-right", "dir_forward"),
    ("alt-b", "backward_word"),
    ("alt-f", "forward_word"),
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
        "dir_back" => Action::DirBack,
        "dir_forward" => Action::DirForward,
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
    AltLeft,
    AltRight,
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
        Key::AltLeft => BoundKey::AltLeft,
        Key::AltRight => BoundKey::AltRight,
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
        "alt-left" | "alt+left" => return Some(BoundKey::AltLeft),
        "alt-right" | "alt+right" => return Some(BoundKey::AltRight),
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

struct CursorGuard {
    configured: Option<&'static str>,
    last_code: Option<&'static str>,
}

impl CursorGuard {
    fn new(setting: Option<&str>) -> Self {
        let configured = match setting {
            Some("block") => Some("\x1b[2 q"),
            Some("beam") => Some("\x1b[6 q"),
            Some("underline") => Some("\x1b[4 q"),
            _ => None,
        };
        Self {
            configured,
            last_code: None,
        }
    }

    fn apply(&mut self, vi_normal: bool) -> std::io::Result<()> {
        let Some(base) = self.configured else {
            return Ok(());
        };
        let target = if vi_normal { "\x1b[2 q" } else { base };
        if self.last_code != Some(target) {
            term::write_stdout(target)?;
            self.last_code = Some(target);
        }
        Ok(())
    }
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        if self.last_code.is_some() {
            let _ = term::write_stdout("\x1b[0 q");
        }
    }
}

fn finish_submitted_line(
    prompt: &str,
    rendered_cols: usize,
    prompt_width: usize,
    buffer: &[char],
    cursor: usize,
    input_marker: &str,
    shell: &Shell,
) -> std::io::Result<()> {
    if shell.setting("transient_prompt") == Some("on")
        && !input_marker.is_empty()
        && !buffer.is_empty()
    {
        let upper_rows = prompt_upper_rows(prompt, rendered_cols);
        let (cursor_row, _) = position(buffer, cursor, rendered_cols, prompt_width);
        let move_up = upper_rows.saturating_add(cursor_row);
        let mut out = String::new();
        if move_up > 0 {
            out.push_str(&format!("\x1b[{move_up}A"));
        }
        out.push_str("\r\x1b[J");
        out.push_str(&prompt::render_transient(shell));
        out.push_str(&highlight::render(shell, buffer));
        out.push_str("\r\n");
        return term::write_stdout(&out);
    }
    term::write_stdout("\r\n")
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
    let entries = shell.history_records();
    if entries.is_empty() {
        return Ok(false);
    }
    let now = unix_micros();

    let columns = term::width().max(1);
    let mut query = String::new();
    let mut offset = 0;
    let mut shown: Option<String> = None;
    let mut line;
    let mut submit = false;

    term::write_stdout("\r\n")?;
    loop {
        if let Some(found) = find_history(&entries, &query, offset, now) {
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
                if find_history(&entries, &query, offset, now).is_none() {
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
            let start = *cursor;
            for offset in 0..times {
                if start + offset < buffer.len() {
                    buffer[start + offset] = char;
                }
            }
            *cursor = (start + times - 1).min(buffer.len() - 1);
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
                *cursor = vi_next_word_end(buffer, *cursor);
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
        'p' => {
            for _ in 0..times {
                editor.vi_paste(buffer, cursor, false, undo);
            }
        }
        'P' => {
            for _ in 0..times {
                editor.vi_paste(buffer, cursor, true, undo);
            }
        }
        'u' => {
            for _ in 0..times {
                let Some((previous, position)) = undo.pop() else {
                    break;
                };
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

fn vi_next_word_end(buffer: &[char], cursor: usize) -> usize {
    let mut index = (cursor + 1).min(buffer.len());
    while index < buffer.len() && !is_word_char(buffer[index]) && buffer[index] != '\n' {
        index += 1;
    }
    while index < buffer.len() && is_word_char(buffer[index]) {
        index += 1;
    }
    index.saturating_sub(1)
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
        *cursor = at + text.len().saturating_sub(1);
    }
}

/// The word the cursor sits in: the run of non-whitespace characters that ends at
/// the cursor. An empty slice means the cursor follows whitespace, where there is
/// no prefix to complete yet.
fn token_under_cursor(buffer: &[char], cursor: usize) -> &[char] {
    let mut start = cursor;
    while start > 0 && !buffer[start - 1].is_whitespace() {
        start -= 1;
    }
    &buffer[start..cursor]
}

/// The rest of `command` from the end of the word that `token` prefixes, so the
/// ghost completes the token and then keeps the line the history record ran. A
/// token that already is the word still yields that line's remainder, and an entry
/// that ends at the token has nothing to add.
///
/// A `true` match compares the typed casing; `false` folds ASCII case, which the
/// caller reaches only when nothing matched exactly.
fn complete_token(command: &str, token: &str, exact: bool) -> Option<String> {
    let chars: Vec<char> = command.chars().collect();
    let width = token.chars().count();
    let mut start = 0;
    for end in 0..=chars.len() {
        if end < chars.len() && !chars[end].is_whitespace() {
            continue;
        }
        if end > start {
            let word = &chars[start..end];
            let head: String = word.iter().take(width).collect();
            let matches = if exact {
                head == token
            } else {
                head.eq_ignore_ascii_case(token)
            };
            if matches {
                let suffix: String = chars[start + width..].iter().collect();
                if !suffix.is_empty() {
                    return Some(suffix);
                }
            }
        }
        start = end + 1;
    }
    None
}

/// Whether every path a record named still resolves. Relative paths resolve against
/// the directory the command ran in, not today's, and a record that named nothing
/// (legacy, or a command whose output path did not exist yet) is always eligible.
#[cfg(test)]
fn paths_exist(record: &HistoryEntry) -> bool {
    paths_exist_borrowed(record.cwd.as_deref(), &record.paths)
}

fn paths_exist_borrowed(cwd: Option<&str>, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    let base = cwd.map(Path::new);
    paths.iter().all(|path| {
        let path = Path::new(path);
        match base {
            Some(base) if !path.is_absolute() => base.join(path).exists(),
            _ => path.exists(),
        }
    })
}

#[cfg(test)]
fn find_suggestion(records: &[HistoryEntry], buffer: &[char], cursor: usize) -> Option<String> {
    if cursor != buffer.len() {
        return None;
    }
    let token = token_under_cursor(buffer, cursor);
    if token.is_empty() {
        return None;
    }
    let token: String = token.iter().collect();
    // Chronological, newest first: check token prefix match in memory first, and
    // only stat the filesystem when a candidate actually matches.
    for exact in [true, false] {
        for record in records.iter().rev() {
            if let Some(suffix) = complete_token(&record.command, &token, exact)
                && paths_exist(record)
            {
                return Some(suffix);
            }
        }
    }
    None
}

fn find_suggestion_shell(shell: &Shell, buffer: &[char], cursor: usize) -> Option<String> {
    if cursor != buffer.len() {
        return None;
    }
    let token = token_under_cursor(buffer, cursor);
    if token.is_empty() {
        return None;
    }
    let token: String = token.iter().collect();
    let entries = shell.history_entries();
    for exact in [true, false] {
        for command in entries.iter().rev() {
            if let Some(suffix) = complete_token(command, &token, exact) {
                let (cwd, paths) = shell.history_entry_paths(command);
                if paths_exist_borrowed(cwd, paths) {
                    return Some(suffix);
                }
            }
        }
    }
    None
}

fn autosuggest(shell: &Shell, buffer: &[char], cursor: usize, prompt_width: usize) -> Option<String> {
    if shell.setting("autosuggest") == Some("off") {
        return None;
    }
    let suffix = find_suggestion_shell(shell, buffer, cursor)?;
    if suffix.contains('\n') || suffix.contains('\t') {
        return None;
    }
    let columns = term::width().max(1);
    let (_, column) = position(buffer, cursor, columns, prompt_width);
    let suffix_width: usize = suffix.chars().map(char_width).sum();
    if column + suffix_width >= columns {
        return None;
    }
    Some(suffix)
}

fn position(buffer: &[char], index: usize, columns: usize, prompt_width: usize) -> (usize, usize) {
    let columns = columns.max(1);
    let mut row = 0usize;
    let mut column = 0usize;
    let mut pending = false;

    let advance = |width: usize, row: &mut usize, column: &mut usize, pending: &mut bool| {
        if width == 0 {
            return;
        }
        if *pending {
            *row += 1;
            *column = 0;
            *pending = false;
        }
        if *column + width > columns {
            *row += 1;
            *column = 0;
        }
        *column += width;
        if *column >= columns {
            *column = columns - 1;
            *pending = true;
        }
    };

    for _ in 0..prompt_width {
        advance(1, &mut row, &mut column, &mut pending);
    }

    for char in buffer.iter().take(index) {
        if *char == '\n' {
            row += 1;
            column = 0;
            pending = false;
            continue;
        }
        let width = if *char == '\t' {
            let stop = (column / 8 + 1) * 8;
            stop - column
        } else {
            char_width(*char)
        };
        advance(width, &mut row, &mut column, &mut pending);
    }

    (row, column)
}

fn char_width(char: char) -> usize {
    let code = char as u32;
    if is_zero_width(code) {
        0
    } else if is_wide(code) {
        2
    } else {
        1
    }
}

fn is_zero_width(code: u32) -> bool {
    matches!(code, 0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff
        | 0x20d0..=0x20ff | 0xfe20..=0xfe2f | 0xfe00..=0xfe0f | 0x200b..=0x200f)
}

fn is_wide(code: u32) -> bool {
    matches!(code, 0x1100..=0x115f | 0x2e80..=0xa4cf | 0xac00..=0xd7a3
        | 0xf900..=0xfaff | 0xfe10..=0xfe19 | 0xfe30..=0xfe6f
        | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff | 0x20000..=0x3fffd)
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

fn display_column(buffer: &[char], start: usize, end: usize) -> usize {
    let mut column = 0;
    for char in &buffer[start.min(buffer.len())..end.min(buffer.len())] {
        if *char == '\t' {
            column = (column / 8 + 1) * 8;
        } else {
            column += char_width(*char);
        }
    }
    column
}

fn index_at_column(buffer: &[char], start: usize, end: usize, target: usize) -> usize {
    let mut column = 0;
    let mut index = start;
    while index < end {
        let width = if buffer[index] == '\t' {
            8 - column % 8
        } else {
            char_width(buffer[index])
        };
        if column + width > target {
            break;
        }
        column += width;
        index += 1;
    }
    index
}

fn move_line(buffer: &[char], cursor: usize, up: bool) -> usize {
    let start = line_start(buffer, cursor);
    let end = line_end(buffer, cursor);
    let column = display_column(buffer, start, cursor);

    if up {
        if start == 0 {
            return cursor;
        }
        let previous_end = start - 1;
        let previous_start = line_start(buffer, previous_end);
        index_at_column(buffer, previous_start, previous_end, column)
    } else {
        if end >= buffer.len() {
            return cursor;
        }
        let next_start = end + 1;
        let next_end = line_end(buffer, next_start);
        index_at_column(buffer, next_start, next_end, column)
    }
}
const DAY_MICROS: i64 = 86_400_000_000;
const FRECENCY_MONTH_MICROS: i64 = 30 * DAY_MICROS;

fn unix_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_micros() as i64)
        .unwrap_or(0)
}

/// Conservative frecency: frequency dominates and age only separates comparable
/// counts, so a command used once yesterday does not bury one used ten times last
/// month. Weight is 4 within a day, 2 within a month and 1 beyond; a legacy record
/// carries no timestamp and always takes the old weight. Integer arithmetic only,
/// with a timestamp ahead of the clock treated as fresh.
fn frecency_score(hits: u64, when: Option<i64>, now: i64) -> u64 {
    let weight = match when {
        Some(when) => {
            let age = now.saturating_sub(when).max(0);
            if age <= DAY_MICROS {
                4
            } else if age <= FRECENCY_MONTH_MICROS {
                2
            } else {
                1
            }
        }
        None => 1,
    };
    hits.max(1).saturating_mul(weight)
}

/// The recency key the ranking compares: the timestamp, oldest when absent, then
/// the file position in the view, which is already chronological.
fn recency_key(entry: &HistoryEntry, index: usize) -> (i64, usize) {
    (entry.when.unwrap_or(i64::MIN), index)
}

/// The Ctrl-R candidate list in display order. An empty query keeps the view order,
/// newest first, so plain Ctrl-R still walks history chronologically. A non-empty
/// query matches by substring; the newest entry that starts with it is pinned first
/// so hammering Ctrl-R still finds what was just typed, and every other match
/// follows by frecency, then recency, then file position.
fn search_order<'a>(records: &'a [HistoryEntry], query: &str, now: i64) -> Vec<&'a HistoryEntry> {
    if query.is_empty() {
        return records.iter().rev().collect();
    }

    let mut matched: Vec<(usize, &HistoryEntry)> = records
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.command.contains(query))
        .collect();

    let pinned = matched
        .iter()
        .filter(|(_, entry)| entry.command.starts_with(query))
        .max_by_key(|(index, entry)| recency_key(entry, *index))
        .map(|(index, _)| *index);

    let mut ordered = Vec::with_capacity(matched.len());
    if let Some(pinned) = pinned
        && let Some(position) = matched.iter().position(|(index, _)| *index == pinned)
    {
        ordered.push(matched.remove(position).1);
    }
    matched.sort_by(|(left_index, left), (right_index, right)| {
        frecency_score(right.hits, right.when, now)
            .cmp(&frecency_score(left.hits, left.when, now))
            .then_with(|| recency_key(right, *right_index).cmp(&recency_key(left, *left_index)))
    });
    ordered.extend(matched.into_iter().map(|(_, entry)| entry));
    ordered
}

fn find_history(records: &[HistoryEntry], query: &str, offset: usize, now: i64) -> Option<String> {
    search_order(records, query, now)
        .get(offset)
        .map(|entry| entry.command.clone())
}

fn is_command_wrapper(word: &str) -> bool {
    matches!(
        word,
        "sudo" | "command" | "builtin" | "exec" | "time" | "nohup" | "env"
    )
}

fn in_command_position(prefix: &[char]) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    let mut at_cmd_pos = true;
    let mut current = String::new();

    for &ch in prefix {
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if escaped {
            escaped = false;
            if !in_double {
                current.push(ch);
            }
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            ';' | '|' | '&' | '(' | '{' | '\n' => {
                current.clear();
                at_cmd_pos = true;
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    if at_cmd_pos {
                        at_cmd_pos = is_command_wrapper(&current);
                    }
                    current.clear();
                }
            }
            other => current.push(other),
        }
    }

    if in_single || in_double || escaped {
        return false;
    }
    if !current.is_empty() {
        at_cmd_pos && is_command_wrapper(&current)
    } else {
        at_cmd_pos
    }
}

fn expand_abbreviation(
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    shell: &Shell,
    separator: &str,
) -> bool {
    if *cursor < buffer.len() && !buffer[*cursor].is_whitespace() {
        return false;
    }

    let mut start = *cursor;
    while start > 0
        && !buffer[start - 1].is_whitespace()
        && !matches!(buffer[start - 1], ';' | '|' | '&' | '(' | '{')
    {
        start -= 1;
    }
    if start == *cursor {
        return false;
    }

    let token: String = buffer[start..*cursor].iter().collect();
    let Some(expansion) = shell.abbreviation(&token) else {
        return false;
    };
    if !in_command_position(&buffer[..start]) {
        return false;
    }

    let separator = if expansion.ends_with(' ') { "" } else { separator };
    let mut replacement: Vec<char> = expansion.chars().collect();
    replacement.extend(separator.chars());
    let next_cursor = start + replacement.len();
    buffer.splice(start..*cursor, replacement);
    *cursor = next_cursor;
    true
}

fn rprompt_column(columns: usize, input_end_col: usize, rprompt_width: usize) -> Option<usize> {
    if rprompt_width == 0 || columns == 0 {
        return None;
    }
    let needed = input_end_col.checked_add(2)?.checked_add(rprompt_width)?;
    (needed <= columns).then_some(columns - rprompt_width)
}

fn format_prompt(raw_prompt: &str, input_marker: &str, columns: usize, shell: &Shell) -> String {
    let mut prompt = prompt::expand_for_width(raw_prompt, columns, shell);
    prompt.push_str(input_marker);
    prompt
}

fn prompt_upper_rows(prompt: &str, columns: usize) -> usize {
    let columns = columns.max(1);
    let mut lines = prompt.split('\n');
    let _ = lines.next_back();
    lines
        .map(|line| {
            let width = visual_width(line);
            if width == 0 {
                1
            } else {
                (width - 1) / columns + 1
            }
        })
        .sum()
}

fn last_line_width(prompt: &str) -> usize {
    visual_width(prompt.rsplit('\n').next().unwrap_or(""))
}

pub(crate) fn visual_width(line: &str) -> usize {
    let mut width = 0;
    let mut chars = line.chars().peekable();

    while let Some(char) = chars.next() {
        if char != '\x1b' {
            if char == '\t' {
                width = (width / 8 + 1) * 8;
            } else {
                width += char_width(char);
            }
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
            let text = take_columns(path, available);
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
    let max_cand_len = menu.candidates[start..end]
        .iter()
        .map(|(cand, _)| cand.chars().count())
        .max()
        .unwrap_or(0)
        .min(28);
    for (index, (candidate, desc)) in menu.candidates[start..end].iter().enumerate() {
        let index = start + index;
        let marker = if index == menu.selected { "> " } else { "  " };
        let raw_line = match desc {
            Some(desc) if !desc.is_empty() => {
                format!("{marker}{candidate:<pad$}  {desc}", pad = max_cand_len)
            }
            _ => format!("{marker}{candidate}"),
        };
        let line = take_columns(&raw_line, width);
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
    candidates: Vec<(String, Option<String>)>,
    selected: usize,
    escape: bool,
}

fn segment_command_name(before: &str) -> Option<&str> {
    let seg_start = before
        .rfind(['|', ';', '&', '(', '{', '\n'])
        .map(|index| index + 1)
        .unwrap_or(0);
    before[seg_start..].split_whitespace().next()
}

fn describe_candidate(candidate: &str, command_position: bool) -> Option<String> {
    if candidate.ends_with('/') {
        return Some(String::from("dir"));
    }
    if candidate.starts_with('$') {
        return Some(String::from("var"));
    }
    if command_position {
        return crate::help::builtin_summary(candidate).map(str::to_string);
    }
    None
}

fn complete_menu(
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    shell: &mut Shell,
) -> Option<CompletionMenu> {
    let mut start = *cursor;
    while start > 0 && !buffer[start - 1].is_whitespace() {
        start -= 1;
    }

    let word: String = buffer[start..*cursor].iter().collect();
    let before: String = buffer[..start].iter().collect();
    let command_position = before
        .chars()
        .rev()
        .find(|char| !matches!(char, ' ' | '\t'))
        .map(|char| matches!(char, '|' | ';' | '&' | '(' | '{' | '\n'))
        .unwrap_or(true);
    if word.is_empty() && command_position {
        return None;
    }

    let full_line: String = buffer.iter().collect();
    let dirs_only = !command_position && matches!(segment_command_name(&before), Some("cd" | "pushd"));
    let mut escape = false;
    let mut candidates: Vec<(String, Option<String>)> = if word.starts_with('$') {
        variable_candidates(&word, shell)
            .into_iter()
            .map(|c| {
                let desc = describe_candidate(&c, false);
                (c, desc)
            })
            .collect()
    } else if !command_position
        && let Some(custom) = shell.complete_command_candidates(&full_line, *cursor)
    {
        escape = true;
        custom
    } else if let Some(candidates) = git_branch_candidates(&word, &before, &shell.cwd) {
        candidates
            .into_iter()
            .map(|c| (c, Some(String::from("branch"))))
            .collect()
    } else if word.contains('/') || !command_position {
        escape = true;
        file_candidates(&word, shell, dirs_only)
            .into_iter()
            .map(|c| {
                let desc = describe_candidate(&c, false);
                (c, desc)
            })
            .collect()
    } else {
        escape = true;
        command_candidates(&word, shell)
            .into_iter()
            .map(|c| {
                let desc = describe_candidate(&c, true);
                (c, desc)
            })
            .collect()
    };

    candidates.sort_by(|a, b| a.0.cmp(&b.0));
    candidates.dedup_by(|a, b| a.0 == b.0);

    let first = candidates.first()?.0.clone();

    let common = candidates
        .iter()
        .fold(first.clone(), |prefix, (candidate, _)| {
            common_prefix(&prefix, candidate)
        });

    if candidates.len() == 1 {
        let candidate = &candidates[0].0;
        let trailing = if candidate.ends_with('/') {
            ""
        } else if std::fs::metadata(shell.cwd.join(candidate))
            .map(|meta| meta.is_dir())
            .unwrap_or(false)
        {
            "/"
        } else if candidate.starts_with('$') {
            ""
        } else {
            " "
        };
        let replacement = if escape {
            escape_completion(candidate)
        } else {
            candidate.clone()
        };
        buffer.drain(start..*cursor);
        *cursor = start;
        insert_text(buffer, cursor, &format!("{replacement}{trailing}"));
        return None;
    }

    if common.chars().count() >= word.chars().count() && common != word {
        let replacement = if escape {
            escape_completion(&common)
        } else {
            common.clone()
        };
        buffer.drain(start..*cursor);
        *cursor = start;
        insert_text(buffer, cursor, &replacement);
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
    if text.contains('\n') || text.contains('\r') {
        let mut out = String::from("'");
        for char in text.chars() {
            if char == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(char);
            }
        }
        out.push('\'');
        return out;
    }
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

fn insert_paste(
    buffer: &mut Vec<char>,
    cursor: &mut usize,
    bytes: &[u8],
    undo: &mut Vec<(Vec<char>, usize)>,
) {
    push_undo(undo, buffer, *cursor);
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n").replace('\r', "\n");
    for char in text.chars() {
        match char {
            '\u{7f}' | '\u{8}' => {
                if *cursor > 0 {
                    buffer.remove(*cursor - 1);
                    *cursor -= 1;
                }
            }
            char => {
                buffer.insert(*cursor, char);
                *cursor += 1;
            }
        }
    }
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
    let Some((candidate, _)) = menu.candidates.get(menu.selected) else {
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

    // Only the segment after the last `$` is a variable; earlier text is a prefix.
    let (head, segment) = word.split_at(word.rfind('$').unwrap_or(0));

    match segment.strip_prefix("${") {
        Some(prefix) => names
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| format!("{head}${{{name}}}"))
            .collect(),
        None => {
            let prefix = segment.trim_start_matches('$');
            names
                .into_iter()
                .filter(|name| name.starts_with(prefix))
                .map(|name| format!("{head}${name}"))
                .collect()
        }
    }
}

fn git_branch_candidates(word: &str, before: &str, cwd: &std::path::Path) -> Option<Vec<String>> {
    let seg_start = before
        .rfind(['|', ';', '&', '(', '{', '\n'])
        .map(|index| index + 1)
        .unwrap_or(0);
    let words: Vec<&str> = before[seg_start..].split_whitespace().collect();
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
    let word_lower = word.to_lowercase();
    let exact: Vec<String> = branches
        .iter()
        .filter(|branch| branch.starts_with(word))
        .cloned()
        .collect();
    if !exact.is_empty() || word.is_empty() {
        return Some(exact);
    }
    Some(
        branches
            .into_iter()
            .filter(|branch| branch.to_lowercase().starts_with(&word_lower))
            .collect(),
    )
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
    let word_lower = word.to_lowercase();
    let mut exact = Vec::new();
    let mut ci = Vec::new();

    for name in shell.command_words() {
        if name.starts_with(word) {
            exact.push(name);
        } else if !word.is_empty() && exact.is_empty() && name.to_lowercase().starts_with(&word_lower) {
            ci.push(name);
        }
    }

    let mut seen_dirs = std::collections::HashSet::new();
    for dir in shell.path_dirs() {
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_exact = name.starts_with(word);
            let is_ci = !is_exact
                && !word.is_empty()
                && exact.is_empty()
                && name.to_lowercase().starts_with(&word_lower);
            if !is_exact && !is_ci {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            use std::os::unix::fs::PermissionsExt;
            if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
                if is_exact {
                    exact.push(name);
                } else {
                    ci.push(name);
                }
            }
        }
    }

    if exact.is_empty() { ci } else { exact }
}

fn file_candidates(word: &str, shell: &Shell, dirs_only: bool) -> Vec<String> {
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
    let dir_path = if std::path::Path::new(directory).is_absolute() {
        std::path::PathBuf::from(directory)
    } else {
        shell.cwd.join(directory)
    };
    let Ok(entries) = std::fs::read_dir(dir_path) else {
        return Vec::new();
    };

    let base_lower = base.to_lowercase();
    let mut exact = Vec::new();
    let mut ci = Vec::new();

    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && !base.starts_with('.') {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map(|kind| kind.is_dir() || (kind.is_symlink() && entry.path().is_dir()))
            .unwrap_or(false);
        if dirs_only && !is_dir {
            continue;
        }
        let is_exact = name.starts_with(base);
        let is_ci = !is_exact
            && !base.is_empty()
            && exact.is_empty()
            && name.to_lowercase().starts_with(&base_lower);
        if !is_exact && !is_ci {
            continue;
        }
        let suffix = if is_dir { "/" } else { "" };
        let item = if display.is_empty() {
            format!("{name}{suffix}")
        } else {
            format!("{display}{name}{suffix}")
        };
        if is_exact {
            exact.push(item);
        } else {
            ci.push(item);
        }
    }

    if exact.is_empty() { ci } else { exact }
}


fn take_columns(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for char in text.chars() {
        let char_width = char_width(char);
        if used + char_width > width {
            break;
        }
        out.push(char);
        used += char_width;
    }
    out
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
    use super::{
        BoundKey, DAY_MICROS, line_end, line_start, move_line, parse_bound, position, search_order,
    };
    use crate::history::{Entry, Origin};

    fn buffer(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn record(command: &str, when: Option<i64>, hits: u64) -> Entry {
        Entry {
            command: command.to_string(),
            when,
            origin: Origin::Loaded,
            cwd: None,
            paths: Vec::new(),
            hits,
        }
    }

    fn order(entries: &[Entry], query: &str, now: i64) -> Vec<String> {
        search_order(entries, query, now)
            .iter()
            .map(|entry| entry.command.clone())
            .collect()
    }

    #[test]
    fn frecency_score_weights_frequency_and_age() {
        let now = 100 * DAY_MICROS;

        assert_eq!(super::frecency_score(3, Some(now), now), 12);
        assert_eq!(super::frecency_score(3, Some(now - DAY_MICROS), now), 12);
        assert_eq!(super::frecency_score(3, Some(now - DAY_MICROS - 1), now), 6);
        assert_eq!(
            super::frecency_score(3, Some(now - 30 * DAY_MICROS), now),
            6
        );
        assert_eq!(
            super::frecency_score(3, Some(now - 30 * DAY_MICROS - 1), now),
            3
        );
        assert_eq!(super::frecency_score(3, None, now), 3);
        // A timestamp ahead of the clock is at least as fresh as now, never negative.
        assert_eq!(super::frecency_score(3, Some(now + DAY_MICROS), now), 12);
    }

    #[test]
    fn search_order_pins_the_newest_prefix_match() {
        let now = 100 * DAY_MICROS;
        let entries = [
            record("deploy new", Some(now), 1),
            record("deploy old", Some(now - 60 * DAY_MICROS), 20),
            record("run deploy now", Some(now - 60 * DAY_MICROS), 50),
        ];

        assert_eq!(
            order(&entries, "deploy", now),
            ["deploy new", "run deploy now", "deploy old"]
        );
    }

    #[test]
    fn search_order_ranks_frequency_before_recency() {
        let now = 100 * DAY_MICROS;
        let entries = [
            record("git log --all", Some(now - 60 * DAY_MICROS), 10),
            record("other log", Some(now), 1),
        ];

        assert_eq!(order(&entries, "log", now), ["git log --all", "other log"]);
    }

    #[test]
    fn search_order_keeps_chronology_for_an_empty_query() {
        let now = 100 * DAY_MICROS;
        let entries = [
            record("printf second", Some(now - 60 * DAY_MICROS), 99),
            record("printf first", Some(now), 1),
        ];

        assert_eq!(order(&entries, "", now), ["printf first", "printf second"]);
    }

    #[test]
    fn search_order_treats_a_missing_when_as_old() {
        let now = 100 * DAY_MICROS;
        let entries = [
            record("recent log", Some(now), 1),
            record("legacy log", None, 1),
        ];

        assert_eq!(order(&entries, "log", now), ["recent log", "legacy log"]);
    }

    #[test]
    fn search_order_tie_breaks_by_newer_when_then_position() {
        let now = 100 * DAY_MICROS;
        let entries = [
            record("same stamp first", Some(now), 2),
            record("same age older", Some(now - 10 * DAY_MICROS), 4),
            record("same age newer", Some(now - 2 * DAY_MICROS), 4),
            record("same stamp last", Some(now), 2),
        ];

        assert_eq!(
            order(&entries, "same", now),
            [
                "same stamp last",
                "same stamp first",
                "same age newer",
                "same age older",
            ]
        );
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

        let glyph = buffer("界");
        assert_eq!(position(&glyph, 1, 10, 4), (0, 6));

        let combining = buffer("e\u{301}");
        assert_eq!(position(&combining, 2, 10, 4), (0, 5));

        let tab = buffer("\t");
        assert_eq!(position(&tab, 1, 20, 0), (0, 8));

        let edge = buffer("abcdefghi界");
        assert_eq!(position(&edge, edge.len(), 10, 0), (1, 2));
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

        let wide = buffer("界xyz\nabc");
        assert_eq!(move_line(&wide, wide.len(), true), 2);
    }
}

#[cfg(test)]
mod suggestion_tests {
    use std::path::{Path, PathBuf};

    use super::find_suggestion;
    use crate::history::{Entry, Origin};

    fn entry(command: &str) -> Entry {
        Entry {
            command: command.to_string(),
            when: None,
            origin: Origin::Loaded,
            cwd: None,
            paths: Vec::new(),
            hits: 1,
        }
    }

    fn located(command: &str, cwd: &Path, paths: &[&str]) -> Entry {
        Entry {
            command: command.to_string(),
            when: None,
            origin: Origin::Loaded,
            cwd: Some(cwd.to_string_lossy().into_owned()),
            paths: paths.iter().map(|path| path.to_string()).collect(),
            hits: 1,
        }
    }

    fn entries() -> Vec<Entry> {
        vec![entry("echo alpha"), entry("ls"), entry("echo beta")]
    }

    fn buffer(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sharky-suggest-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_the_newest_longer_prefix_match() {
        assert_eq!(
            find_suggestion(&entries(), &buffer("echo"), 4),
            Some(" beta".to_string())
        );
        assert_eq!(find_suggestion(&entries(), &buffer("echo beta"), 9), None);
    }

    #[test]
    fn ignores_mid_line_and_empty_buffers() {
        assert_eq!(find_suggestion(&entries(), &buffer("echo"), 2), None);
        assert_eq!(find_suggestion(&entries(), &[], 0), None);
        assert_eq!(find_suggestion(&entries(), &buffer("zzz"), 3), None);
    }

    #[test]
    fn suggests_a_token_prefix_before_the_cursor() {
        let history = vec![entry("echo hi"), entry("ls -la")];
        assert_eq!(
            find_suggestion(&history, &buffer("x = ech"), 7),
            Some("o hi".to_string())
        );
        // A complete token still offers the rest of the line the record ran.
        assert_eq!(
            find_suggestion(&history, &buffer("x = echo"), 8),
            Some(" hi".to_string())
        );
        // A trailing space starts a new token, which no prefix can complete yet.
        assert_eq!(find_suggestion(&history, &buffer("x = echo "), 9), None);
    }

    #[test]
    fn searches_chronologically_newest_first() {
        let history = vec![entry("echo alpha GHOSTOLD"), entry("echo alpha GHOSTNEW")];
        assert_eq!(
            find_suggestion(&history, &buffer("echo alpha GH"), 13),
            Some("OSTNEW".to_string())
        );
    }

    #[test]
    fn skips_a_suggestion_whose_path_is_gone() {
        let dir = scratch("gone");
        std::fs::write(dir.join("stale.txt"), b"x").unwrap();
        std::fs::write(dir.join("stale.yml"), b"x").unwrap();
        let history = vec![
            located("cat stale.yml", &dir, &["stale.yml"]),
            located("cat stale.txt", &dir, &["stale.txt"]),
        ];
        assert_eq!(
            find_suggestion(&history, &buffer("cat stale"), 9),
            Some(".txt".to_string())
        );
        std::fs::remove_file(dir.join("stale.txt")).unwrap();
        // Newest first, so the record naming the deleted file would win if it were
        // still eligible; the search has to fall back to the older valid one.
        assert_eq!(
            find_suggestion(&history, &buffer("cat stale"), 9),
            Some(".yml".to_string())
        );
        std::fs::remove_file(dir.join("stale.yml")).unwrap();
        assert_eq!(find_suggestion(&history, &buffer("cat stale"), 9), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolves_recorded_paths_against_the_record_cwd() {
        let dir = scratch("cwd");
        std::fs::write(dir.join("here.txt"), b"x").unwrap();
        let history = vec![located("cat here.txt", &dir, &["here.txt"])];
        // The file is nowhere near the process cwd, so only the record's own cwd
        // can make this candidate eligible.
        assert_eq!(
            find_suggestion(&history, &buffer("cat here"), 8),
            Some(".txt".to_string())
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefers_case_sensitive_over_case_insensitive() {
        let history = vec![entry("echo CASETHING"), entry("echo caseOther")];
        assert_eq!(
            find_suggestion(&history, &buffer("echo CASE"), 9),
            Some("THING".to_string())
        );
        // A case-insensitive match still answers when nothing matches exactly.
        assert_eq!(
            find_suggestion(&history, &buffer("echo Cas"), 8),
            Some("eOther".to_string())
        );
    }

    #[test]
    fn keeps_a_command_whose_output_path_does_not_exist() {
        // `mkdir out` named nothing that existed when it was recorded, which is
        // exactly why the command stays suggestible: the path it creates is not a
        // reason to hide it.
        let history = vec![entry("mkdir out")];
        assert_eq!(
            find_suggestion(&history, &buffer("mkdir ou"), 8),
            Some("t".to_string())
        );
    }

    #[test]
    fn legacy_entries_without_paths_stay_eligible() {
        let history = vec![entry("echo LEGACYWITNESS")];
        assert_eq!(
            find_suggestion(&history, &buffer("echo LEG"), 8),
            Some("ACYWITNESS".to_string())
        );
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
        assert_eq!(apply(Action::TransposeChars, "abc", 2).0, "acb");
        assert_eq!(apply(Action::TransposeChars, "abc", 3).0, "acb");
        assert_eq!(apply(Action::TransposeChars, "abc", 1).0, "bac");
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

    #[test]
    fn forward_word_accepts_one_suggestion_word_at_a_time() {
        assert_eq!(
            apply_with_suggestion(Action::ForwardWord, "echo", 4, Some(" alpha beta")),
            ("echo alpha".to_string(), 10, Outcome::Continue)
        );
        assert_eq!(
            apply_with_suggestion(Action::ForwardWord, "echo alpha", 10, Some(" beta")),
            ("echo alpha beta".to_string(), 15, Outcome::Continue)
        );
        assert_eq!(
            apply_with_suggestion(Action::ForwardWord, "echo alpha", 1, Some(" gamma")),
            ("echo alpha".to_string(), 4, Outcome::Continue)
        );
    }

    #[test]
    fn prefix_history_filters_and_restores_draft() {
        let mut editor = Editor::new();
        let mut shell = Shell::new();
        shell.prepare_history(true);
        shell.push_history("echo one");
        shell.push_history("ls -la");
        shell.push_history("echo two");

        let mut buffer: Vec<char> = "echo".chars().collect();
        let mut cursor = buffer.len();
        editor.reset_history_nav(&shell);
        let mut undo = Vec::new();

        editor
            .apply_action(Action::HistoryPrev, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "echo two");

        editor
            .apply_action(Action::HistoryPrev, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "echo one");

        editor
            .apply_action(Action::HistoryNext, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "echo two");

        editor
            .apply_action(Action::HistoryNext, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "echo");

        // Editing the recalled line clears prefix lock and resets index.
        editor
            .apply_action(Action::HistoryPrev, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        editor
            .apply_action(Action::BackwardDeleteChar, &mut buffer, &mut cursor, None, "", &shell, &mut undo)
            .unwrap();
        assert_eq!(buffer.iter().collect::<String>(), "echo tw");
        assert!(!editor.prefix_active);
        assert_eq!(editor.index, shell.history_entries().len());
    }

    #[test]
    fn expand_abbreviation_supports_multiple_command_positions() {
        use super::expand_abbreviation;

        let mut shell = Shell::new();
        assert_eq!(shell.run("abbr gs 'git status'"), 0);

        let try_expand = |line: &str, cursor_pos: usize, sep: &str| -> (bool, String, usize) {
            let mut buf: Vec<char> = line.chars().collect();
            let mut cur = cursor_pos;
            let changed = expand_abbreviation(&mut buf, &mut cur, &shell, sep);
            (changed, buf.iter().collect(), cur)
        };

        assert_eq!(try_expand("gs", 2, " "), (true, "git status ".to_string(), 11));
        assert_eq!(
            try_expand("echo a && gs", 12, " "),
            (true, "echo a && git status ".to_string(), 21)
        );
        assert_eq!(
            try_expand("echo a | gs", 11, ""),
            (true, "echo a | git status".to_string(), 19)
        );
        assert_eq!(
            try_expand("echo a; sudo gs", 15, ""),
            (true, "echo a; sudo git status".to_string(), 23)
        );
        assert_eq!(
            try_expand("(gs", 3, ""),
            (true, "(git status".to_string(), 11)
        );
        assert_eq!(
            try_expand("gs --short", 2, ""),
            (true, "git status --short".to_string(), 10)
        );
        assert_eq!(try_expand("echo gs", 7, " "), (false, "echo gs".to_string(), 7));
        assert_eq!(
            try_expand("echo 'a && ' gs", 15, " "),
            (false, "echo 'a && ' gs".to_string(), 15)
        );
        assert_eq!(
            try_expand("echo \"a && gs", 13, " "),
            (false, "echo \"a && gs".to_string(), 13)
        );
    }

    #[test]
    fn completion_covers_cd_dirs_only_empty_arg_and_case_insensitive() {
        use super::complete_menu;

        let dir = std::env::temp_dir().join(format!("sharky-comp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("SubDir")).unwrap();
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("README.md"), b"doc").unwrap();
        std::fs::write(dir.join("alpha.txt"), b"txt").unwrap();

        let mut shell = Shell::new();
        shell.cwd = dir.clone();

        let mut buf: Vec<char> = "cd ".chars().collect();
        let mut cur = buf.len();
        let menu = complete_menu(&mut buf, &mut cur, &mut shell).expect("cd empty word lists dirs");
        let names: Vec<String> = menu.candidates.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(names, vec!["SubDir/".to_string(), "assets/".to_string()]);

        let mut buf: Vec<char> = "cat ".chars().collect();
        let mut cur = buf.len();
        let menu = complete_menu(&mut buf, &mut cur, &mut shell).expect("cat empty word lists files and dirs");
        let names: Vec<String> = menu.candidates.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            names,
            vec![
                "README.md".to_string(),
                "SubDir/".to_string(),
                "alpha.txt".to_string(),
                "assets/".to_string()
            ]
        );

        let mut buf: Vec<char> = "cat re".chars().collect();
        let mut cur = buf.len();
        assert!(complete_menu(&mut buf, &mut cur, &mut shell).is_none());
        assert_eq!(buf.iter().collect::<String>(), "cat README.md ");

        let mut buf: Vec<char> = "cd sub".chars().collect();
        let mut cur = buf.len();
        assert!(complete_menu(&mut buf, &mut cur, &mut shell).is_none());
        assert_eq!(buf.iter().collect::<String>(), "cd SubDir/");

        let mut buf: Vec<char> = Vec::new();
        let mut cur = 0;
        assert!(complete_menu(&mut buf, &mut cur, &mut shell).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_branch_completion_handles_segments_and_case_fallback() {
        use super::complete_menu;

        let dir = std::env::temp_dir().join(format!("sharky-git-comp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git/refs/heads/feat")).unwrap();
        std::fs::write(dir.join(".git/refs/heads/main"), b"0000\n").unwrap();
        std::fs::write(dir.join(".git/refs/heads/feat/FishUX"), b"0000\n").unwrap();

        let mut shell = Shell::new();
        shell.cwd = dir.clone();

        let mut buf: Vec<char> = "git checkout ".chars().collect();
        let mut cur = buf.len();
        let menu = complete_menu(&mut buf, &mut cur, &mut shell).expect("git branch list");
        let names: Vec<String> = menu.candidates.iter().map(|(c, _)| c.clone()).collect();
        assert_eq!(
            names,
            vec!["feat/FishUX".to_string(), "main".to_string()]
        );

        let mut buf: Vec<char> = "true && git switch feat/fish".chars().collect();
        let mut cur = buf.len();
        assert!(complete_menu(&mut buf, &mut cur, &mut shell).is_none());
        assert_eq!(buf.iter().collect::<String>(), "true && git switch feat/FishUX ");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rprompt_column_respects_two_column_margin() {
        use super::rprompt_column;

        assert_eq!(rprompt_column(80, 10, 7), Some(73));
        assert_eq!(rprompt_column(20, 11, 7), Some(13));
        assert_eq!(rprompt_column(20, 12, 7), None);
        assert_eq!(rprompt_column(10, 9, 1), None);
        assert_eq!(rprompt_column(80, 10, 0), None);
    }

    #[test]
    fn prompt_upper_rows_accounts_for_reflow_width() {
        use super::prompt_upper_rows;

        assert_eq!(prompt_upper_rows("> ", 80), 0);
        let two_line = format!("\x1b[38;2;91;207;250m{}\x1b[0m\n\x1b]133;B\x1b\\> ", "x".repeat(100));
        assert_eq!(prompt_upper_rows(&two_line, 100), 1);
        assert_eq!(prompt_upper_rows(&two_line, 80), 2);
        assert_eq!(prompt_upper_rows(&two_line, 40), 3);
        assert_eq!(prompt_upper_rows(&two_line, 120), 1);
        assert_eq!(prompt_upper_rows("\n> ", 80), 1);
    }
}
