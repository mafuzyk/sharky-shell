use crate::ast::*;
use crate::lexer::{self, Part, Token, TokenKind, Word};

pub struct ParseError {
    pub message: String,
    #[allow(dead_code)]
    pub incomplete: bool,
}

impl ParseError {
    fn new(message: impl Into<String>, incomplete: bool) -> Self {
        ParseError {
            message: message.into(),
            incomplete,
        }
    }
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    let tokens = lexer::tokenize(source).map_err(|err| {
        ParseError::new(format!("syntax error: {}", err.message), err.incomplete)
    })?;
    Parser { tokens, pos: 0 }.parse()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn kind(&self) -> TokenKind {
        self.tokens
            .get(self.pos)
            .map(|token| token.kind.clone())
            .unwrap_or(TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let token = self
            .tokens
            .get(self.pos)
            .cloned()
            .unwrap_or(Token {
                kind: TokenKind::Eof,
                pos: 0,
            });
        self.pos += 1;
        token
    }

    fn at_word(&self, text: &str) -> bool {
        match self.kind() {
            TokenKind::Word(word) => literal(&word).as_deref() == Some(text),
            _ => false,
        }
    }

    fn word(&mut self) -> Word {
        match self.advance().kind {
            TokenKind::Word(word) => word,
            _ => unreachable!("word() called on a non-word token"),
        }
    }

    fn skip_newlines(&mut self) {
        while matches!(self.kind(), TokenKind::Newline) {
            self.advance();
        }
    }

    fn skip_separators(&mut self) {
        while matches!(self.kind(), TokenKind::Newline | TokenKind::Semi) {
            self.advance();
        }
    }

    fn parse(mut self) -> Result<Program, ParseError> {
        let mut lines = Vec::new();
        self.skip_newlines();

        while !matches!(self.kind(), TokenKind::Eof) {
            lines.push(self.parse_list(&[], false)?);
            if matches!(self.kind(), TokenKind::Newline | TokenKind::Semi) {
                self.skip_separators();
            } else if !matches!(self.kind(), TokenKind::Eof) {
                return Err(self.unexpected());
            }
        }

        Ok(Program { lines })
    }

    fn terminated(&self, keywords: &[&str], rbrace: bool) -> bool {
        matches!(self.kind(), TokenKind::Eof)
            || keywords.iter().any(|keyword| self.at_word(keyword))
            || (rbrace && matches!(self.kind(), TokenKind::RBrace))
    }

    fn parse_list(&mut self, keywords: &[&str], rbrace: bool) -> Result<List, ParseError> {
        let mut entries = Vec::new();
        self.skip_separators();

        while !self.terminated(keywords, rbrace) {
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry { background, and_or });

            if matches!(self.kind(), TokenKind::Semi | TokenKind::Newline) {
                self.skip_separators();
            } else if !self.terminated(keywords, rbrace) {
                return Err(self.unexpected());
            }
        }

        Ok(List { entries })
    }

    fn parse_and_or(&mut self) -> Result<AndOr, ParseError> {
        let first = self.parse_pipeline()?;
        let mut rest = Vec::new();

        while matches!(self.kind(), TokenKind::AndIf | TokenKind::OrIf) {
            let is_and = matches!(self.kind(), TokenKind::AndIf);
            self.advance();
            self.skip_newlines();
            rest.push((is_and, self.parse_pipeline()?));
        }

        Ok(AndOr { first, rest })
    }

    fn parse_pipeline(&mut self) -> Result<Pipeline, ParseError> {
        if self.at_word("!") {
            self.advance();
            let command = self.parse_pipeline_command()?;
            return Ok(Pipeline {
                commands: vec![Command::Not(Box::new(command))],
            });
        }

        let mut commands = vec![self.parse_pipeline_command()?];
        while matches!(self.kind(), TokenKind::Pipe) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_pipeline_command()?);
        }

        Ok(Pipeline { commands })
    }

    fn parse_pipeline_command(&mut self) -> Result<Command, ParseError> {
        if self.at_word("!") {
            self.advance();
            let command = self.parse_pipeline_command()?;
            return Ok(Command::Not(Box::new(command)));
        }
        self.parse_command()
    }

    fn parse_command(&mut self) -> Result<Command, ParseError> {
        let command = if self.at_word("if") {
            self.parse_if()?
        } else if self.at_word("for") {
            self.parse_for()?
        } else if self.at_word("while") {
            self.parse_while(false)?
        } else if self.at_word("until") {
            self.parse_while(true)?
        } else if self.at_word("case") {
            self.parse_case()?
        } else if self.at_word("function") {
            self.parse_function_keyword()?
        } else if matches!(self.kind(), TokenKind::TestStart) {
            self.parse_test()?
        } else if let TokenKind::Arithmetic(source) = self.kind() {
            self.advance();
            Command::Arithmetic(source)
        } else if matches!(self.kind(), TokenKind::LBrace) {
            Command::Group(self.parse_group()?)
        } else if matches!(self.kind(), TokenKind::LParen) {
            Command::Subshell(self.parse_subshell()?)
        } else if self.function_definition() {
            self.parse_function_definition()?
        } else {
            Command::Simple(self.parse_simple()?)
        };

        let mut redirects = Vec::new();
        while let Some(redirect) = self.redirect()? {
            redirects.push(redirect);
        }

        if redirects.is_empty() {
            Ok(command)
        } else {
            Ok(Command::Redirected {
                command: Box::new(command),
                redirects,
            })
        }
    }

    fn parse_test(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let mut words = Vec::new();

        loop {
            match self.kind() {
                TokenKind::Word(word) => {
                    words.push(word);
                    self.advance();
                }
                TokenKind::TestEnd => {
                    self.advance();
                    return Ok(Command::Test(words));
                }
                TokenKind::Eof => return Err(ParseError::new("unclosed [[", true)),
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn function_definition(&self) -> bool {
        let TokenKind::Word(word) = self.kind() else {
            return false;
        };
        if literal(&word).is_none() {
            return false;
        }
        matches!(
            self.tokens.get(self.pos + 1).map(|token| &token.kind),
            Some(TokenKind::LParen)
        ) && matches!(
            self.tokens.get(self.pos + 2).map(|token| &token.kind),
            Some(TokenKind::RParen)
        )
    }

    fn parse_function_definition(&mut self) -> Result<Command, ParseError> {
        let name = literal(&self.word()).unwrap_or_default();
        self.advance();
        self.advance();
        self.skip_newlines();
        let body = self.parse_function_body()?;
        Ok(Command::Function { name, body })
    }

    fn parse_function_keyword(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let TokenKind::Word(word) = self.kind() else {
            return Err(self.unexpected());
        };
        let name = literal(&word).ok_or_else(|| self.unexpected())?;
        self.advance();

        if matches!(self.kind(), TokenKind::LParen)
            && matches!(
                self.tokens.get(self.pos + 1).map(|token| &token.kind),
                Some(TokenKind::RParen)
            )
        {
            self.advance();
            self.advance();
        }

        self.skip_newlines();
        let body = self.parse_function_body()?;
        Ok(Command::Function { name, body })
    }

    fn parse_function_body(&mut self) -> Result<List, ParseError> {
        if !matches!(self.kind(), TokenKind::LBrace) {
            return Err(self.unexpected());
        }
        self.parse_group()
    }

    fn parse_group(&mut self) -> Result<List, ParseError> {
        self.advance();
        let list = self.parse_list(&[], true)?;
        if !matches!(self.kind(), TokenKind::RBrace) {
            return Err(self.unexpected());
        }
        self.advance();
        Ok(list)
    }

    fn parse_subshell(&mut self) -> Result<List, ParseError> {
        self.advance();
        let list = self.parse_list_before_paren()?;
        if !matches!(self.kind(), TokenKind::RParen) {
            return Err(self.unexpected());
        }
        self.advance();
        Ok(list)
    }

    fn parse_list_before_paren(&mut self) -> Result<List, ParseError> {
        let mut entries = Vec::new();
        self.skip_separators();

        while !matches!(self.kind(), TokenKind::RParen | TokenKind::Eof) {
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry { background, and_or });
            if matches!(self.kind(), TokenKind::Semi | TokenKind::Newline) {
                self.skip_separators();
            } else if !matches!(self.kind(), TokenKind::RParen | TokenKind::Eof) {
                return Err(self.unexpected());
            }
        }

        Ok(List { entries })
    }

    fn parse_if(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let mut branches = Vec::new();

        loop {
            let condition = self.parse_list(&["then"], false)?;
            self.expect_word("then")?;
            let body = self.parse_list(&["elif", "else", "fi"], false)?;
            branches.push((condition, body));
            if !self.at_word("elif") {
                break;
            }
            self.advance();
        }

        let else_body = if self.at_word("else") {
            self.advance();
            Some(self.parse_list(&["fi"], false)?)
        } else {
            None
        };

        self.expect_word("fi")?;
        Ok(Command::If {
            branches,
            else_body,
        })
    }

    fn parse_for(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let name = literal(&self.word()).unwrap_or_default();
        if name.is_empty() {
            return Err(self.unexpected());
        }

        self.skip_newlines();
        let mut words = Vec::new();
        if self.at_word("in") {
            self.advance();
            while matches!(self.kind(), TokenKind::Word(_)) {
                words.push(self.word());
            }
        }

        self.skip_separators();
        self.expect_word("do")?;
        let body = self.parse_list(&["done"], false)?;
        self.expect_word("done")?;
        Ok(Command::For { name, words, body })
    }

    fn parse_while(&mut self, until: bool) -> Result<Command, ParseError> {
        self.advance();
        let condition = self.parse_list(&["do"], false)?;
        self.expect_word("do")?;
        let body = self.parse_list(&["done"], false)?;
        self.expect_word("done")?;
        Ok(Command::While {
            condition,
            body,
            until,
        })
    }

    fn parse_case(&mut self) -> Result<Command, ParseError> {
        self.advance();
        let word = self.word();
        self.skip_newlines();
        self.expect_word("in")?;
        self.skip_separators();
        let mut arms = Vec::new();

        while !self.at_word("esac") {
            if matches!(self.kind(), TokenKind::Eof) {
                return Err(ParseError::new("unclosed case", true));
            }

            if matches!(self.kind(), TokenKind::LParen) {
                self.advance();
            }

            let mut patterns = Vec::new();
            loop {
                patterns.push(self.word());
                if !matches!(self.kind(), TokenKind::Pipe) {
                    break;
                }
                self.advance();
                self.skip_newlines();
            }

            if !matches!(self.kind(), TokenKind::RParen) {
                return Err(self.unexpected());
            }
            self.advance();

            let body = self.parse_case_body()?;
            arms.push((patterns, body));

            if self.at_double_semi() {
                self.advance();
                self.advance();
                self.skip_separators();
            } else if !self.at_word("esac") {
                return Err(self.unexpected());
            }
        }

        self.expect_word("esac")?;
        Ok(Command::Case { word, arms })
    }

    fn at_double_semi(&self) -> bool {
        matches!(self.kind(), TokenKind::Semi)
            && matches!(
                self.tokens.get(self.pos + 1).map(|token| &token.kind),
                Some(TokenKind::Semi)
            )
    }

    fn parse_case_body(&mut self) -> Result<List, ParseError> {
        let mut entries = Vec::new();
        self.skip_newlines();

        loop {
            if self.at_word("esac")
                || self.at_double_semi()
                || matches!(self.kind(), TokenKind::Eof)
            {
                break;
            }

            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry { background, and_or });

            if matches!(self.kind(), TokenKind::Semi) {
                if self.at_double_semi() {
                    break;
                }
                self.advance();
                self.skip_newlines();
            } else if matches!(self.kind(), TokenKind::Newline) {
                self.skip_newlines();
            } else {
                break;
            }
        }

        Ok(List { entries })
    }

    fn expect_word(&mut self, text: &str) -> Result<(), ParseError> {
        if self.at_word(text) {
            self.advance();
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn parse_simple(&mut self) -> Result<Simple, ParseError> {
        let mut simple = Simple::default();

        loop {
            match self.kind() {
                TokenKind::Word(word) => {
                    if simple.words.is_empty()
                        && let Some(assignment) = as_assignment(&word)
                    {
                        simple.assignments.push(assignment);
                    } else {
                        simple.words.push(word);
                    }
                    self.advance();
                }
                _ => match self.redirect()? {
                    Some(redirect) => simple.redirects.push(redirect),
                    None => break,
                },
            }
        }

        if simple.words.is_empty() && simple.assignments.is_empty() && simple.redirects.is_empty() {
            return Err(self.unexpected());
        }

        Ok(simple)
    }

    fn redirect(&mut self) -> Result<Option<Redirect>, ParseError> {
        let (fd, op) = match self.kind() {
            TokenKind::RedirOut(fd) => (fd.unwrap_or(1), RedirectOp::Out),
            TokenKind::RedirAppend(fd) => (fd.unwrap_or(1), RedirectOp::Append),
            TokenKind::RedirIn(fd) => (fd.unwrap_or(0), RedirectOp::In),
            TokenKind::RedirDup(to, from) => {
                self.advance();
                return Ok(Some(Redirect {
                    fd: to,
                    op: RedirectOp::Dup(from),
                    target: None,
                }));
            }
            TokenKind::RedirHere {
                fd,
                body,
                quoted,
                ..
            } => {
                self.advance();
                return Ok(Some(Redirect {
                    fd: fd.unwrap_or(0),
                    op: RedirectOp::Here {
                        body,
                        expand: !quoted,
                    },
                    target: None,
                }));
            }
            _ => return Ok(None),
        };

        self.advance();
        let TokenKind::Word(word) = self.kind() else {
            return Err(ParseError::new("syntax error: redirect without a target", false));
        };
        self.advance();
        Ok(Some(Redirect {
            fd,
            op,
            target: Some(word),
        }))
    }

    fn unexpected(&self) -> ParseError {
        let found = match self.kind() {
            TokenKind::Eof => "end of input".to_string(),
            TokenKind::Word(word) => format!("'{}'", literal(&word).unwrap_or_default()),
            other => format!("{other:?}"),
        };
        ParseError::new(format!("syntax error: unexpected {found}"), false)
    }
}

fn as_assignment(word: &Word) -> Option<Assignment> {
    let Part::Literal(text) = word.parts.first()? else {
        return None;
    };
    let (name, rest) = text.split_once('=')?;
    if !is_name(name) {
        return None;
    }

    let mut parts = Vec::new();
    if !rest.is_empty() {
        parts.push(Part::Literal(rest.to_string()));
    }
    parts.extend(word.parts[1..].iter().cloned());
    Some(Assignment {
        name: name.to_string(),
        value: Word { parts },
    })
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}
