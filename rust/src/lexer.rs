#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Literal(String),
    Single(String),
    Escaped(String),
    Double(Vec<Part>),
    Variable(String),
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

    fn scan_dollar(&mut self) -> Part {
        let start = self.pos;
        self.pos += 1;

        match self.current() {
            Some('{') => {
                self.pos += 1;
                let mut name = String::new();
                while let Some(char) = self.current() {
                    if char == '}' {
                        self.pos += 1;
                        return Part::Variable(name);
                    }
                    name.push(char);
                    self.pos += 1;
                }
                self.pos = start;
                Part::Literal("$".to_string())
            }
            Some(char @ ('?' | '$' | '!' | '#' | '@' | '*')) => {
                self.pos += 1;
                Part::Variable(char.to_string())
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
}

fn push_literal(parts: &mut Vec<Part>, char: char) {
    if let Some(Part::Literal(text)) = parts.last_mut() {
        text.push(char);
    } else {
        parts.push(Part::Literal(char.to_string()));
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
