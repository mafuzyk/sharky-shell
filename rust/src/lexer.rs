#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Literal(String),
    Single(String),
    Escaped(String),
    Double(Vec<Part>),
    Variable(String),
    Parameter(Box<Parameter>),
    CmdSub(String),
    Arithmetic(String),
    Tilde,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parameter {
    pub name: String,
    pub operator: Option<ParamOp>,
    pub argument: Option<String>,
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
    LParen,
    RParen,
    LBrace,
    RBrace,
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
}

impl Lexer {
    fn new(source: &str) -> Self {
        Lexer {
            chars: source.chars().collect(),
            pos: 0,
            tokens: Vec::new(),
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
            match char {
                '\n' => {
                    self.push(TokenKind::Newline);
                    self.pos += 1;
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

    fn scan_word(&mut self) -> Result<Word, LexError> {
        let mut parts = Vec::new();
        let start = self.pos;

        while !self.at_word_end() {
            let Some(char) = self.current() else { break };
            match char {
                '\'' => parts.push(Part::Single(self.scan_single()?)),
                '"' => parts.push(Part::Double(self.scan_double()?)),
                '\\' => self.scan_escape(&mut parts)?,
                '$' => parts.push(self.scan_dollar()),
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

        debug_assert!(self.pos > start, "scan_word must consume input");
        Ok(Word { parts })
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
                Some('$') => parts.push(self.scan_dollar()),
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
            return Err(LexError::new("unsupported syntax: heredoc", start, false));
        }

        self.push_at(TokenKind::RedirIn(Some(fd.unwrap_or(0))), start);
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

    fn scan_dollar(&mut self) -> Part {
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
            Some('{') => match self.scan_braced_parameter(start) {
                Some(parameter) => Part::Parameter(Box::new(parameter)),
                None => Part::Literal("$".to_string()),
            },
            Some(char @ ('?' | '$' | '!' | '#' | '@' | '*')) => {
                self.pos += 1;
                Part::Variable(char.to_string())
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
                Part::Variable(name)
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
                Part::Variable(name)
            }
            _ => Part::Literal("$".to_string()),
        }
    }

    fn scan_braced_parameter(&mut self, start: usize) -> Option<Parameter> {
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

        self.pos = start;
        None
    }

    fn scan_arithmetic(&mut self, start: usize) -> Part {
        self.pos += 2;
        let content_start = self.pos;
        let mut depth = 0;

        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '(' => {
                    depth += 1;
                    self.pos += 1;
                }
                ')' if depth == 0 && self.chars.get(self.pos + 1) == Some(&')') => {
                    let source: String = self.chars[content_start..self.pos].iter().collect();
                    self.pos += 2;
                    return Part::Arithmetic(source.trim().to_string());
                }
                ')' => {
                    depth -= 1;
                    self.pos += 1;
                }
                _ => self.pos += 1,
            }
        }

        self.pos = start;
        Part::Literal("$((".to_string())
    }

    fn scan_command_substitution(&mut self, start: usize) -> Part {
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
                            return Part::CmdSub(source);
                        }
                        index += 1;
                    }
                    _ => index += 1,
                },
            }
        }

        self.pos = start;
        Part::Literal("$(".to_string())
    }
}

enum Quote {
    Normal,
    Single,
    Double,
}

fn parse_parameter(content: &str) -> Parameter {
    if let Some(rest) = content.strip_prefix('#')
        && !rest.is_empty()
        && rest.chars().all(|char| char.is_ascii_alphanumeric() || char == '_')
    {
        return Parameter {
            name: rest.to_string(),
            operator: Some(ParamOp::Length),
            argument: None,
        };
    }

    const OPERATORS: &[(&str, ParamOp)] = &[
        (":-", ParamOp::Default),
        (":=", ParamOp::Assign),
        (":?", ParamOp::Error),
        (":+", ParamOp::Alternate),
        ("##", ParamOp::TrimPrefixLong),
        ("%%", ParamOp::TrimSuffixLong),
        ("#", ParamOp::TrimPrefixShort),
        ("%", ParamOp::TrimSuffixShort),
    ];

    for (operator, kind) in OPERATORS {
        if let Some(index) = content.find(operator) {
            return Parameter {
                name: content[..index].to_string(),
                operator: Some(*kind),
                argument: Some(content[index + operator.len()..].to_string()),
            };
        }
    }

    Parameter {
        name: content.to_string(),
        operator: None,
        argument: None,
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
