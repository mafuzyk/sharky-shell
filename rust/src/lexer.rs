#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Literal(String),
    Single(String),
    Escaped(String),
    Double(Vec<Part>),
    Variable(String),
    Parameter(Box<Parameter>),
    CmdSub(String),
    ProcessSub(String, bool),
    Arithmetic(String),
    Tilde,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub operator: Option<ParamOp>,
    pub argument: Option<String>,
    pub index: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ParamOp {
    Length,
    Default,
    Assign,
    Alternate,
    Error,
    TrimPrefixShort,
    TrimPrefixLong,
    TrimSuffixShort,
    TrimSuffixLong,
    Substitute,
    SubstituteAll,
    Substring,
    UpperFirst,
    UpperAll,
    LowerFirst,
    LowerAll,
    Indirect,
    Names,
    Keys,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Word(Word),
    Semi,
    AndIf,
    OrIf,
    Pipe,
    Amp,
    RedirOut(Option<i32>),
    RedirAppend(Option<i32>),
    RedirIn(Option<i32>),
    RedirDup(i32, i32),
    RedirBoth,
    RedirBothAppend,
    RedirHereString(Option<i32>),
    RedirHere {
        fd: Option<i32>,
        strip: bool,
        delimiter: String,
        quoted: bool,
        body: String,
    },
    LParen,
    RParen,
    LBrace,
    RBrace,
    TestStart,
    TestEnd,
    Arithmetic(String),
    Newline,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub pos: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub message: String,
    pub pos: usize,
    pub incomplete: bool,
}

impl LexError {
    fn new(message: impl Into<String>, pos: usize, incomplete: bool) -> Self {
        LexError {
            message: message.into(),
            pos,
            incomplete,
        }
    }
}

pub fn tokenize(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).tokenize()
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    tokens: Vec<Token>,
    test: bool,
    pending_heredocs: Vec<usize>,
    here_awaiting: Option<usize>,
}

impl Lexer {
    fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            tokens: Vec::new(),
            test: false,
            pending_heredocs: Vec::new(),
            here_awaiting: None,
        }
    }

    fn tokenize(mut self) -> Result<Vec<Token>, LexError> {
        loop {
            self.skip_blanks();
            let Some(char) = self.current() else {
                self.push(TokenKind::Eof);
                return Ok(self.tokens);
            };

            let start = self.pos;

            if self.test {
                if char == ']' && self.peek() == Some(']') {
                    self.push(TokenKind::TestEnd);
                    self.pos += 2;
                    self.test = false;
                    continue;
                }
                if char.is_whitespace() {
                    self.pos += 1;
                    continue;
                }
                let word = self.scan_test_word()?;
                self.push_at(TokenKind::Word(word), start);
                continue;
            }

            if char == '['
                && self.peek() == Some('[')
                && delimiter_after(self.chars.get(self.pos + 2).copied())
            {
                self.push(TokenKind::TestStart);
                self.pos += 2;
                self.test = true;
                continue;
            }

            match char {
                '\n' => {
                    self.push(TokenKind::Newline);
                    self.pos += 1;
                    self.read_heredoc_bodies()?;
                }
                '#' => self.skip_comment(),
                ';' => {
                    self.push(TokenKind::Semi);
                    self.pos += 1;
                }
                '&' => {
                    if self.peek() == Some('&') {
                        self.push(TokenKind::AndIf);
                        self.pos += 2;
                    } else if self.peek() == Some('>') {
                        if self.chars.get(self.pos + 2) == Some(&'>') {
                            self.push(TokenKind::RedirBothAppend);
                            self.pos += 3;
                        } else {
                            self.push(TokenKind::RedirBoth);
                            self.pos += 2;
                        }
                    } else {
                        self.push(TokenKind::Amp);
                        self.pos += 1;
                    }
                }
                '|' => {
                    if self.peek() == Some('|') {
                        self.push(TokenKind::OrIf);
                        self.pos += 2;
                    } else {
                        self.push(TokenKind::Pipe);
                        self.pos += 1;
                    }
                }
                ' ' | '\t' => self.pos += 1,
                '(' if self.peek() == Some('(') => self.scan_arithmetic_command()?,
                '(' => {
                    self.push(TokenKind::LParen);
                    self.pos += 1;
                }
                ')' => {
                    self.push(TokenKind::RParen);
                    self.pos += 1;
                }
                '{' if delimiter_after(self.peek()) => {
                    self.push(TokenKind::LBrace);
                    self.pos += 1;
                }
                '}' if delimiter_after(self.peek()) => {
                    self.push(TokenKind::RBrace);
                    self.pos += 1;
                }
                '>' if self.peek() == Some('(') => self.scan_process_word()?,
                '<' if self.peek() == Some('(') => self.scan_process_word()?,
                '>' => self.scan_output_redirect(None)?,
                '<' => self.scan_input_redirect(None)?,
                char if char.is_ascii_digit() => self.scan_number_redirect()?,
                _ => {
                    if is_word_break(char) {
                        return Err(LexError::new(
                            format!("unsupported syntax: '{char}'"),
                            start,
                            false,
                        ));
                    }
                    if let Some(index) = self.here_awaiting.take() {
                        let word = self.scan_word()?;
                        let delimiter = word_text(&word);
                        let quoted = word
                            .parts
                            .iter()
                            .any(|part| !matches!(part, Part::Literal(_)));
                        if let TokenKind::RedirHere {
                            delimiter: slot,
                            quoted: quoted_slot,
                            ..
                        } = &mut self.tokens[index].kind
                        {
                            *slot = delimiter;
                            *quoted_slot = quoted;
                        }
                        continue;
                    }
                    let word = self.scan_word()?;
                    self.tokens.push(Token {
                        kind: TokenKind::Word(word),
                        pos: start,
                    });
                }
            }
        }
    }

    fn current(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn push(&mut self, kind: TokenKind) {
        let pos = self.pos;
        self.tokens.push(Token { kind, pos });
    }

    fn skip_blanks(&mut self) {
        while matches!(self.current(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        while let Some(char) = self.current() {
            if char == '\n' {
                break;
            }
            self.pos += 1;
        }
    }

    fn at_word_end(&self) -> bool {
        match self.current() {
            None => true,
            Some(char) => is_word_break(char),
        }
    }

    fn scan_arithmetic_command(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        self.pos += 2;
        let content_start = self.pos;
        let mut depth: usize = 0;

        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '(' => {
                    depth += 1;
                    self.pos += 1;
                }
                ')' if depth == 0 && self.chars.get(self.pos + 1) == Some(&')') => {
                    let source: String = self.chars[content_start..self.pos].iter().collect();
                    self.pos += 2;
                    self.push_at(TokenKind::Arithmetic(source.trim().to_string()), start);
                    return Ok(());
                }
                ')' => {
                    depth = depth.saturating_sub(1);
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }

        Err(LexError::new("unclosed arithmetic command", start, true))
    }

    fn scan_test_word(&mut self) -> Result<Word, LexError> {
        let mut parts = Vec::new();

        while let Some(char) = self.current() {
            if char.is_whitespace() {
                break;
            }
            match char {
                '\'' => parts.push(Part::Single(self.scan_single()?)),
                '"' => parts.push(Part::Double(self.scan_double()?)),
                '\\' => self.scan_escape(&mut parts)?,
                '$' => parts.push(self.scan_dollar()?),
                '?' | '*' | '+' | '@' | '!' if self.peek() == Some('(') => {
                    let start = self.pos;
                    let text = self.scan_extglob(start)?;
                    push_text(&mut parts, &text);
                }
                _ => {
                    push_literal(&mut parts, char);
                    self.pos += 1;
                }
            }
        }

        Ok(Word { parts })
    }

    fn scan_word(&mut self) -> Result<Word, LexError> {
        self.scan_word_with_parts(Vec::new())
    }

    fn scan_process_word(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        let part = self.scan_process_substitution(start)?;
        let word = self.scan_word_with_parts(vec![part])?;
        self.push_at(TokenKind::Word(word), start);
        Ok(())
    }

    fn scan_word_with_parts(&mut self, mut parts: Vec<Part>) -> Result<Word, LexError> {
        let start = self.pos;

        while !self.at_word_end() {
            let Some(char) = self.current() else { break };
            match char {
                '\'' => parts.push(Part::Single(self.scan_single()?)),
                '"' => parts.push(Part::Double(self.scan_double()?)),
                '\\' => self.scan_escape(&mut parts)?,
                '$' => parts.push(self.scan_dollar()?),
                '?' | '*' | '+' | '@' | '!' if self.peek() == Some('(') => {
                    let start = self.pos;
                    let text = self.scan_extglob(start)?;
                    push_text(&mut parts, &text);
                }
                '~' if parts.is_empty() && matches!(self.peek(), None | Some('/')) =>
                {
                    parts.push(Part::Tilde);
                    self.pos += 1;
                }
                _ => {
                    push_literal(&mut parts, char);
                    self.pos += 1;
                }
            }
        }

        debug_assert!(
            self.pos > start || !parts.is_empty(),
            "scan_word must consume input"
        );
        Ok(Word { parts })
    }

    fn scan_ansi_c(&mut self, start: usize) -> Result<Part, LexError> {
        self.pos += 1;
        let mut text = String::new();

        loop {
            match self.current() {
                None => return Err(LexError::new("unclosed $' string", start, true)),
                Some('\'') => {
                    self.pos += 1;
                    return Ok(Part::Single(text));
                }
                Some('\\') => {
                    self.pos += 1;
                    let escaped = self.scan_ansi_escape();
                    if escaped.starts_with('\0') {
                        while let Some(char) = self.current() {
                            self.pos += 1;
                            if char == '\\' {
                                self.pos += 1;
                                continue;
                            }
                            if char == '\'' {
                                return Ok(Part::Single(text));
                            }
                        }
                        return Err(LexError::new("unclosed $' string", start, true));
                    }
                    text.push_str(&escaped);
                }
                Some(char) => {
                    text.push(char);
                    self.pos += 1;
                }
            }
        }
    }

    fn scan_ansi_escape(&mut self) -> String {
        let Some(char) = self.current() else {
            return "\\".to_string();
        };
        self.pos += 1;

        match char {
            'n' => "\n".to_string(),
            't' => "\t".to_string(),
            'r' => "\r".to_string(),
            'a' => "\u{7}".to_string(),
            'b' => "\u{8}".to_string(),
            'f' => "\u{c}".to_string(),
            'v' => "\u{b}".to_string(),
            'e' | 'E' => "\u{1b}".to_string(),
            '\\' => "\\".to_string(),
            '\'' => "'".to_string(),
            '"' => "\"".to_string(),
            '?' => "?".to_string(),
            'x' => self.hex_escape(2, "\\x"),
            'u' => self.hex_escape(4, "\\u"),
            'U' => self.hex_escape(8, "\\U"),
            '0'..='7' => self.octal_escape(char),
            other => format!("\\{other}"),
        }
    }

    fn hex_escape(&mut self, max: usize, fallback: &str) -> String {
        let mut digits = String::new();
        while digits.len() < max {
            match self.current() {
                Some(char) if char.is_ascii_hexdigit() => {
                    digits.push(char);
                    self.pos += 1;
                }
                _ => break,
            }
        }
        if digits.is_empty() {
            return fallback.to_string();
        }
        let value = u32::from_str_radix(&digits, 16).unwrap_or(0);
        char::from_u32(value)
            .map(String::from)
            .unwrap_or_else(|| fallback.to_string())
    }

    fn octal_escape(&mut self, first: char) -> String {
        let mut digits = String::from(first);
        while digits.len() < 3 {
            match self.current() {
                Some(char @ '0'..='7') => {
                    digits.push(char);
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let value = u32::from_str_radix(&digits, 8).unwrap_or(0);
        char::from_u32(value).map(String::from).unwrap_or_default()
    }

    fn scan_process_substitution(&mut self, start: usize) -> Result<Part, LexError> {
        let input = self.current() == Some('<');
        self.pos += 2;
        let content_start = self.pos;
        let mut index = self.pos;
        let mut depth = 1;
        let mut state = Quote::Normal;
        let mut cases: Vec<CasePhase> = Vec::new();
        let mut command_start = true;
        let mut pattern_depth = 0usize;
        let mut pattern_group = false;
        let mut arm_pending = false;
        let mut word = String::new();

        while index < self.chars.len() {
            let char = self.chars[index];
            match state {
                Quote::Single => {
                    if char == '\'' {
                        state = Quote::Normal;
                    }
                    index += 1;
                }
                Quote::Double => match char {
                    '\\' => index += 2,
                    '"' => {
                        state = Quote::Normal;
                        index += 1;
                    }
                    _ => index += 1,
                },
                Quote::Normal => {
                    match char {
                        '\\' => index += 2,
                        '\'' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            if cases.last() == Some(&CasePhase::PatternStart) {
                                set_pattern(&mut cases, &mut pattern_depth, &mut pattern_group);
                            }
                            state = Quote::Single;
                            index += 1;
                        }
                        '"' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            if cases.last() == Some(&CasePhase::PatternStart) {
                                set_pattern(&mut cases, &mut pattern_depth, &mut pattern_group);
                            }
                            state = Quote::Double;
                            index += 1;
                        }
                        '(' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            match cases.last().copied() {
                                Some(CasePhase::PatternStart) => {
                                    let extglob = index > 0
                                        && matches!(
                                            self.chars[index - 1],
                                            '?' | '*' | '+' | '@' | '!'
                                        );
                                    set_pattern(
                                        &mut cases,
                                        &mut pattern_depth,
                                        &mut pattern_group,
                                    );
                                    pattern_depth = 1;
                                    pattern_group = !extglob;
                                }
                                Some(CasePhase::Pattern) => pattern_depth += 1,
                                _ => depth += 1,
                            }
                            index += 1;
                        }
                        ')' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            if cases.last() == Some(&CasePhase::Pattern) {
                                if pattern_depth > 0 {
                                    pattern_depth -= 1;
                                    if pattern_depth == 0 && pattern_group {
                                        set_body(&mut cases);
                                        pattern_group = false;
                                    }
                                } else {
                                    set_body(&mut cases);
                                }
                                index += 1;
                                continue;
                            }
                            depth -= 1;
                            if depth == 0 {
                                let source: String =
                                    self.chars[content_start..index].iter().collect();
                                self.pos = index + 1;
                                return Ok(Part::ProcessSub(source, input));
                            }
                            index += 1;
                        }
                        ';' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            if self.chars.get(index + 1) == Some(&';') {
                                index += 2;
                                if self.chars.get(index) == Some(&'&') {
                                    index += 1;
                                }
                                set_pattern_start(&mut cases);
                                arm_pending = true;
                                command_start = true;
                            } else if self.chars.get(index + 1) == Some(&'&') {
                                index += 2;
                                set_pattern_start(&mut cases);
                                arm_pending = true;
                                command_start = true;
                            } else {
                                command_start = true;
                                index += 1;
                            }
                        }
                        '&' | '|' | '\n' | '{' => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            command_start = true;
                            index += 1;
                        }
                        char if char.is_alphanumeric() || char == '_' => {
                            word.push(char);
                            index += 1;
                        }
                        char if char.is_whitespace() => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            index += 1;
                        }
                        _ => {
                            flush_scan_word(
                                &mut word,
                                &mut cases,
                                &mut command_start,
                                &mut pattern_depth,
                                &mut pattern_group,
                                &mut arm_pending,
                            );
                            index += 1;
                        }
                    }
                }
            }
        }

        Err(LexError::new("unclosed process substitution", start, true))
    }

    fn scan_extglob(&mut self, start: usize) -> Result<String, LexError> {
        let mut text = String::new();
        text.push(self.current().unwrap_or('?'));
        self.pos += 1;
        text.push('(');
        self.pos += 1;
        let mut depth = 1usize;
        let mut quote = None;

        while let Some(char) = self.current() {
            if let Some(delimiter) = quote {
                text.push(char);
                self.pos += 1;
                if char == delimiter {
                    quote = None;
                }
                continue;
            }
            match char {
                '\\' => {
                    text.push(char);
                    self.pos += 1;
                    if let Some(next) = self.current() {
                        text.push(next);
                        self.pos += 1;
                    }
                }
                '\'' | '"' => {
                    quote = Some(char);
                    text.push(char);
                    self.pos += 1;
                }
                '(' => {
                    text.push(char);
                    depth += 1;
                    self.pos += 1;
                }
                ')' => {
                    text.push(char);
                    depth -= 1;
                    self.pos += 1;
                    if depth == 0 {
                        return Ok(text);
                    }
                }
                _ => {
                    text.push(char);
                    self.pos += 1;
                }
            }
        }

        Err(LexError::new("unclosed extglob", start, true))
    }

    fn scan_single(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        self.pos += 1;
        let mut text = String::new();

        loop {
            match self.current() {
                None => {
                    return Err(LexError::new("unclosed single quote", start, true));
                }
                Some('\'') => {
                    self.pos += 1;
                    return Ok(text);
                }
                Some(char) => {
                    text.push(char);
                    self.pos += 1;
                }
            }
        }
    }

    fn scan_double(&mut self) -> Result<Vec<Part>, LexError> {
        let start = self.pos;
        self.pos += 1;
        let mut parts = Vec::new();

        loop {
            match self.current() {
                None => {
                    return Err(LexError::new("unclosed double quote", start, true));
                }
                Some('"') => {
                    self.pos += 1;
                    return Ok(parts);
                }
                Some('\\') => match self.peek() {
                    None => {
                        return Err(LexError::new("unclosed double quote", start, true));
                    }
                    Some('\n') => self.pos += 2,
                    Some(next @ ('$' | '`' | '"' | '\\')) => {
                        push_literal(&mut parts, next);
                        self.pos += 2;
                    }
                    Some(_) => {
                        push_literal(&mut parts, '\\');
                        self.pos += 1;
                    }
                },
                Some('$') if self.peek() == Some('\'') => {
                    push_literal(&mut parts, '$');
                    self.pos += 1;
                }
                Some('$') => parts.push(self.scan_dollar()?),
                Some(char) => {
                    push_literal(&mut parts, char);
                    self.pos += 1;
                }
            }
        }
    }

    fn scan_escape(&mut self, parts: &mut Vec<Part>) -> Result<(), LexError> {
        let start = self.pos;
        self.pos += 1;

        match self.current() {
            None => Err(LexError::new("trailing backslash", start, true)),
            Some('\n') => {
                self.pos += 1;
                Ok(())
            }
            Some(char) => {
                parts.push(Part::Escaped(char.to_string()));
                self.pos += 1;
                Ok(())
            }
        }
    }

    fn scan_output_redirect(&mut self, fd: Option<i32>) -> Result<(), LexError> {
        let start = self.pos;
        self.pos += 1;

        match self.current() {
            Some('>') => {
                self.pos += 1;
                self.push_at(TokenKind::RedirAppend(fd), start);
            }
            Some('&') => {
                self.pos += 1;
                let mut digits = String::new();
                while let Some(char) = self.current() {
                    if char.is_ascii_digit() {
                        digits.push(char);
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                let Ok(target) = digits.parse() else {
                    return Err(LexError::new("unsupported syntax: '>&'", start, false));
                };
                self.push_at(TokenKind::RedirDup(fd.unwrap_or(1), target), start);
            }
            _ => self.push_at(TokenKind::RedirOut(fd), start),
        }

        Ok(())
    }

    fn scan_input_redirect(&mut self, fd: Option<i32>) -> Result<(), LexError> {
        let start = self.pos;
        self.pos += 1;

        if self.current() == Some('<') {
            if self.peek() == Some('<') {
                self.pos += 2;
                self.push_at(TokenKind::RedirHereString(fd), start);
                return Ok(());
            }
            let strip = self.peek() == Some('-');
            self.pos += if strip { 2 } else { 1 };
            let index = self.tokens.len();
            self.push_at(
                TokenKind::RedirHere {
                    fd,
                    strip,
                    delimiter: String::new(),
                    quoted: false,
                    body: String::new(),
                },
                start,
            );
            self.pending_heredocs.push(index);
            self.here_awaiting = Some(index);
            return Ok(());
        }

        self.push_at(TokenKind::RedirIn(Some(fd.unwrap_or(0))), start);
        Ok(())
    }

    fn read_heredoc_bodies(&mut self) -> Result<(), LexError> {
        let pending = std::mem::take(&mut self.pending_heredocs);

        for index in pending {
            let (strip, delimiter) = match &self.tokens.get(index).map(|token| &token.kind) {
                Some(TokenKind::RedirHere {
                    strip, delimiter, ..
                }) => (*strip, delimiter.clone()),
                _ => continue,
            };

            let mut body = String::new();
            loop {
                if self.pos >= self.chars.len() {
                    return Err(LexError::new("unclosed heredoc", self.pos, true));
                }

                let mut end = self.pos;
                while end < self.chars.len() && self.chars[end] != '\n' {
                    end += 1;
                }
                let line: String = self.chars[self.pos..end].iter().collect();
                let candidate = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };

                self.pos = end;
                if candidate == delimiter {
                    if self.pos < self.chars.len() {
                        self.pos += 1;
                    }
                    break;
                }

                body.push_str(candidate);
                body.push('\n');
                if self.pos < self.chars.len() {
                    self.pos += 1;
                }
            }

            if let TokenKind::RedirHere { body: slot, .. } = &mut self.tokens[index].kind {
                *slot = body;
            }
        }

        Ok(())
    }

    fn scan_number_redirect(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        let mut fd: i32 = 0;

        while let Some(char) = self.current() {
            let Some(digit) = char.to_digit(10) else { break };
            fd = fd.saturating_mul(10).saturating_add(digit as i32);
            self.pos += 1;
        }

        match self.current() {
            Some('>') => self.scan_output_redirect(Some(fd)),
            Some('<') => self.scan_input_redirect(Some(fd)),
            _ => {
                self.pos = start;
                let word = self.scan_word()?;
                self.push_at(TokenKind::Word(word), start);
                Ok(())
            }
        }
    }

    fn push_at(&mut self, kind: TokenKind, pos: usize) {
        self.tokens.push(Token { kind, pos });
    }

    fn scan_dollar(&mut self) -> Result<Part, LexError> {
        let start = self.pos;
        self.pos += 1;

        match self.current() {
            Some('(') => {
                if self.peek() == Some('(') {
                    self.scan_arithmetic(start)
                } else {
                    self.scan_command_substitution(start)
                }
            }
            Some('{') => match self.scan_braced_parameter() {
                Some(parameter) => Ok(Part::Parameter(Box::new(parameter))),
                None => Err(LexError::new("unclosed parameter expansion", start, true)),
            },
            Some('\'') => self.scan_ansi_c(start),
            Some(char @ ('?' | '$' | '!' | '#' | '@' | '*')) => {
                self.pos += 1;
                Ok(Part::Variable(char.to_string()))
            }
            Some(char) if char.is_ascii_digit() => {
                let mut name = String::new();
                while let Some(char) = self.current() {
                    if !char.is_ascii_digit() {
                        break;
                    }
                    name.push(char);
                    self.pos += 1;
                }
                Ok(Part::Variable(name))
            }
            Some(char) if is_name_start(char) => {
                let mut name = String::new();
                while let Some(char) = self.current() {
                    if !is_name_char(char) {
                        break;
                    }
                    name.push(char);
                    self.pos += 1;
                }
                Ok(Part::Variable(name))
            }
            _ => Ok(Part::Literal("$".to_string())),
        }
    }

    fn scan_braced_parameter(&mut self) -> Option<Parameter> {
        self.pos += 1;
        let content_start = self.pos;
        let mut depth = 0;
        let mut index = self.pos;

        while index < self.chars.len() {
            match self.chars[index] {
                '\\' => index += 2,
                '$' if self.chars.get(index + 1) == Some(&'{') => {
                    depth += 1;
                    index += 2;
                }
                '}' if depth == 0 => {
                    let content: String = self.chars[content_start..index].iter().collect();
                    self.pos = index + 1;
                    return Some(parse_parameter(&content));
                }
                '}' => {
                    depth -= 1;
                    index += 1;
                }
                _ => index += 1,
            }
        }

        None
    }

    fn scan_arithmetic(&mut self, start: usize) -> Result<Part, LexError> {
        self.pos += 2;
        let content_start = self.pos;
        let mut depth: usize = 0;

        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '(' => {
                    depth += 1;
                    self.pos += 1;
                }
                ')' if depth == 0 && self.chars.get(self.pos + 1) == Some(&')') => {
                    let source: String = self.chars[content_start..self.pos].iter().collect();
                    self.pos += 2;
                    return Ok(Part::Arithmetic(source.trim().to_string()));
                }
                ')' if depth == 0 => {
                    return Err(LexError::new("unexpected ')' in arithmetic", self.pos, false));
                }
                ')' => {
                    depth -= 1;
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }

        Err(LexError::new("unclosed arithmetic expansion", start, true))
    }

    fn scan_command_substitution(&mut self, start: usize) -> Result<Part, LexError> {
        let content_start = self.pos + 1;
        let mut index = content_start;
        let mut depth = 1;
        let mut state = Quote::Normal;

        while index < self.chars.len() {
            let char = self.chars[index];

            match state {
                Quote::Single => {
                    if char == '\'' {
                        state = Quote::Normal;
                    }
                    index += 1;
                }
                Quote::Double => match char {
                    '\\' => index += 2,
                    '"' => {
                        state = Quote::Normal;
                        index += 1;
                    }
                    _ => index += 1,
                },
                Quote::Normal => match char {
                    '\\' => index += 2,
                    '\'' => {
                        state = Quote::Single;
                        index += 1;
                    }
                    '"' => {
                        state = Quote::Double;
                        index += 1;
                    }
                    '(' => {
                        depth += 1;
                        index += 1;
                    }
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            let source: String = self.chars[content_start..index].iter().collect();
                            self.pos = index + 1;
                            return Ok(Part::CmdSub(source));
                        }
                        index += 1;
                    }
                    _ => index += 1,
                },
            }
        }

        Err(LexError::new("unclosed command substitution", start, true))
    }
}

enum Quote {
    Normal,
    Single,
    Double,
}

fn parse_parameter(content: &str) -> Parameter {
    if let Some(rest) = content.strip_prefix('#') {
        let (name, index, after) = split_name_index(rest);
        if after.is_empty() && !name.is_empty() && (index.is_some() || is_parameter_name(&name)) {
            return Parameter {
                name,
                operator: Some(ParamOp::Length),
                argument: None,
                index,
            };
        }
    }

    if let Some(rest) = content.strip_prefix('!') {
        if let Some(prefix) = rest.strip_suffix('*') {
            return Parameter {
                name: prefix.to_string(),
                operator: Some(ParamOp::Names),
                argument: Some("*".to_string()),
                index: None,
            };
        }
        if let Some(prefix) = rest.strip_suffix('@') {
            return Parameter {
                name: prefix.to_string(),
                operator: Some(ParamOp::Names),
                argument: Some("@".to_string()),
                index: None,
            };
        }

        let (name, index, after) = split_name_index(rest);
        if after.is_empty() && !name.is_empty() {
            if index.is_some() {
                return Parameter {
                    name,
                    operator: Some(ParamOp::Keys),
                    argument: None,
                    index,
                };
            }
            return Parameter {
                name,
                operator: Some(ParamOp::Indirect),
                argument: None,
                index: None,
            };
        }
    }

    let (name, index, rest) = split_name_index(content);
    if rest.is_empty() {
        return Parameter {
            name,
            operator: None,
            argument: None,
            index,
        };
    }

    const OPERATORS: &[(&str, ParamOp)] = &[
        ("##", ParamOp::TrimPrefixLong),
        ("%%", ParamOp::TrimSuffixLong),
        ("^^", ParamOp::UpperAll),
        (",,", ParamOp::LowerAll),
        (":-", ParamOp::Default),
        (":=", ParamOp::Assign),
        (":?", ParamOp::Error),
        (":+", ParamOp::Alternate),
        ("//", ParamOp::SubstituteAll),
        ("^", ParamOp::UpperFirst),
        (",", ParamOp::LowerFirst),
        ("#", ParamOp::TrimPrefixShort),
        ("%", ParamOp::TrimSuffixShort),
        ("/", ParamOp::Substitute),
        (":", ParamOp::Substring),
    ];

    for (operator, kind) in OPERATORS {
        if let Some(argument) = rest.strip_prefix(operator) {
            return Parameter {
                name,
                operator: Some(*kind),
                argument: Some(argument.to_string()),
                index,
            };
        }
    }

    Parameter {
        name: content.to_string(),
        operator: None,
        argument: None,
        index: None,
    }
}

fn is_parameter_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if matches!(first, '?' | '$' | '!' | '#' | '@' | '*' | '-') {
        return chars.next().is_none();
    }
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}

fn split_name_index(text: &str) -> (String, Option<String>, &str) {
    let mut end = 0;
    let mut chars = text.char_indices().peekable();

    if let Some((_, first)) = chars.peek().copied() {
        if first.is_ascii_digit() {
            while let Some((index, char)) = chars.peek().copied() {
                if !char.is_ascii_digit() {
                    break;
                }
                end = index + char.len_utf8();
                chars.next();
            }
        } else if is_name_start(first) {
            while let Some((index, char)) = chars.peek().copied() {
                if !is_name_char(char) {
                    break;
                }
                end = index + char.len_utf8();
                chars.next();
            }
        } else if matches!(first, '?' | '$' | '!' | '#' | '@' | '*' | '-') {
            end = first.len_utf8();
        }
    }

    let name = text[..end].to_string();
    if name.is_empty() {
        return (name, None, text);
    }

    let rest = &text[end..];
    if let Some(inner) = rest.strip_prefix('[')
        && let Some(close) = inner.find(']')
    {
        let index = &inner[..close];
        if !index.is_empty() {
            return (name, Some(index.to_string()), &inner[close + 1..]);
        }
    }

    (name, None, rest)
}

pub fn heredoc_word(text: &str) -> Result<Word, LexError> {
    let mut lexer = Lexer::new(text);
    let mut parts = Vec::new();

    while let Some(char) = lexer.current() {
        match char {
            '$' if lexer.peek() == Some('\'') => {
                push_literal(&mut parts, '$');
                lexer.pos += 1;
            }
            '$' => parts.push(lexer.scan_dollar()?),
            '\\' => match lexer.peek() {
                Some(next @ ('$' | '`' | '\\')) => {
                    parts.push(Part::Escaped(next.to_string()));
                    lexer.pos += 2;
                }
                Some('\n') => lexer.pos += 2,
                _ => {
                    push_literal(&mut parts, '\\');
                    lexer.pos += 1;
                }
            },
            _ => {
                push_literal(&mut parts, char);
                lexer.pos += 1;
            }
        }
    }

    Ok(Word { parts })
}

fn word_text(word: &Word) -> String {
    let mut text = String::new();

    fn walk(parts: &[Part], out: &mut String) {
        for part in parts {
            match part {
                Part::Literal(value) | Part::Single(value) | Part::Escaped(value) => {
                    out.push_str(value)
                }
                Part::Double(inner) => walk(inner, out),
                _ => {}
            }
        }
    }

    walk(&word.parts, &mut text);
    text
}

fn push_text(parts: &mut Vec<Part>, text: &str) {
    if let Some(Part::Literal(existing)) = parts.last_mut() {
        existing.push_str(text);
    } else {
        parts.push(Part::Literal(text.to_string()));
    }
}

fn push_literal(parts: &mut Vec<Part>, char: char) {
    if let Some(Part::Literal(text)) = parts.last_mut() {
        text.push(char);
    } else {
        parts.push(Part::Literal(char.to_string()));
    }
}

fn delimiter_after(char: Option<char>) -> bool {
    match char {
        None => true,
        Some(char) => is_word_break(char),
    }
}

fn is_word_break(char: char) -> bool {
    matches!(char, ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>')
}

fn is_name_start(char: char) -> bool {
    char.is_ascii_alphabetic() || char == '_'
}

fn is_name_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || char == '_'
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CasePhase {
    Subject,
    PatternStart,
    Pattern,
    Body,
}

fn set_pattern(
    cases: &mut [CasePhase],
    pattern_depth: &mut usize,
    pattern_group: &mut bool,
) {
    if let Some(phase) = cases.last_mut() {
        *phase = CasePhase::Pattern;
    }
    *pattern_depth = 0;
    *pattern_group = false;
}

fn set_pattern_start(cases: &mut [CasePhase]) {
    if let Some(phase) = cases.last_mut() {
        *phase = CasePhase::PatternStart;
    }
}

fn set_body(cases: &mut [CasePhase]) {
    if let Some(phase) = cases.last_mut() {
        *phase = CasePhase::Body;
    }
}

fn flush_scan_word(
    word: &mut String,
    cases: &mut Vec<CasePhase>,
    command_start: &mut bool,
    pattern_depth: &mut usize,
    pattern_group: &mut bool,
    arm_pending: &mut bool,
) {
    if word.is_empty() {
        return;
    }
    let phase = cases.last().copied();
    match phase {
        None => {
            if *command_start {
                if word == "case" {
                    cases.push(CasePhase::Subject);
                }
                *command_start = false;
            }
        }
        Some(CasePhase::Subject) => {
            if word == "in" {
                cases.pop();
                cases.push(CasePhase::PatternStart);
                *command_start = true;
                *arm_pending = false;
            }
        }
        Some(CasePhase::PatternStart) => {
            if *arm_pending && word == "esac" {
                cases.pop();
                *arm_pending = false;
                *command_start = false;
            } else {
                set_pattern(cases, pattern_depth, pattern_group);
                *arm_pending = false;
            }
        }
        Some(CasePhase::Pattern) => {}
        Some(CasePhase::Body) => {
            if *command_start {
                if word == "esac" {
                    cases.pop();
                } else if word == "case" {
                    cases.push(CasePhase::Subject);
                }
                *command_start = false;
            }
        }
    }
    word.clear();
}
