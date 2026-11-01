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

    fn take_word(&mut self) -> Result<Word, ParseError> {
        match self.kind() {
            TokenKind::Word(word) => {
                let word = word.clone();
                self.advance();
                Ok(word)
            }
            _ => Err(self.unexpected()),
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

            if background {
                self.skip_newlines();
            } else if matches!(self.kind(), TokenKind::Semi | TokenKind::Newline) {
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
        let mut timed = None;
        if self.at_word("time") {
            self.advance();
            let posix = if self.at_word("-p") {
                self.advance();
                true
            } else {
                false
            };
            timed = Some(posix);

            if matches!(
                self.kind(),
                TokenKind::Eof
                    | TokenKind::Newline
                    | TokenKind::Semi
                    | TokenKind::Amp
                    | TokenKind::Pipe
                    | TokenKind::AndIf
                    | TokenKind::OrIf
            ) {
                return Ok(Pipeline {
                    commands: vec![Command::Simple(Simple::default())],
                    timed,
                    negate: false,
                });
            }
        }

        let negate = if self.at_word("!") {
            self.advance();
            true
        } else {
            false
        };

        let mut commands = vec![self.parse_pipeline_command()?];
        while matches!(self.kind(), TokenKind::Pipe) {
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_pipeline_command()?);
        }

        Ok(Pipeline {
            commands,
            timed,
            negate,
        })
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
        if literal(&word).is_none() || as_assignment(&word).is_some() {
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
        let name = literal(&self.take_word()?).unwrap_or_default();
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
            if background {
                self.skip_newlines();
            } else if matches!(self.kind(), TokenKind::Semi | TokenKind::Newline) {
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
            if condition.entries.is_empty() {
                return Err(ParseError::new("syntax error: missing condition after 'if'", false));
            }
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
        if let TokenKind::Arithmetic(source) = self.kind() {
            let source = source.clone();
            self.advance();
            let parts: Vec<&str> = source.split(';').collect();
            if parts.len() != 3 {
                return Err(ParseError::new(
                    "syntax error: malformed arithmetic for",
                    false,
                ));
            }
            let init = parts[0].trim().to_string();
            let condition = parts[1].trim().to_string();
            let step = parts[2].trim().to_string();
            self.skip_separators();
            self.expect_word("do")?;
            let body = self.parse_list(&["done"], false)?;
            self.expect_word("done")?;
            return Ok(Command::ForArith {
                init,
                condition,
                step,
                body,
            });
        }

        let name = literal(&self.take_word()?).unwrap_or_default();
        if name.is_empty() {
            return Err(self.unexpected());
        }

        self.skip_newlines();
        let mut words = Vec::new();
        if self.at_word("in") {
            self.advance();
            while matches!(self.kind(), TokenKind::Word(_)) {
                words.push(self.take_word()?);
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
        if condition.entries.is_empty() {
            return Err(ParseError::new("syntax error: missing loop condition", false));
        }
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
        let word = self.take_word()?;
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
                patterns.push(self.take_word()?);
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

            if let Some((terminator, count)) = self.case_terminator() {
                for _ in 0..count {
                    self.advance();
                }
                arms.push((patterns, body, terminator));
                self.skip_separators();
            } else if self.at_word("esac") {
                arms.push((patterns, body, CaseTerm::Break));
            } else {
                return Err(self.unexpected());
            }
        }

        self.expect_word("esac")?;
        Ok(Command::Case { word, arms })
    }

    fn case_terminator(&self) -> Option<(CaseTerm, usize)> {
        if !matches!(self.kind(), TokenKind::Semi) {
            return None;
        }
        let next = self.tokens.get(self.pos + 1).map(|token| &token.kind);
        let third = self.tokens.get(self.pos + 2).map(|token| &token.kind);
        if matches!(next, Some(TokenKind::Semi)) && matches!(third, Some(TokenKind::Amp)) {
            Some((CaseTerm::Continue, 3))
        } else if matches!(next, Some(TokenKind::Amp)) {
            Some((CaseTerm::Fallthrough, 2))
        } else if matches!(next, Some(TokenKind::Semi)) {
            Some((CaseTerm::Break, 2))
        } else {
            None
        }
    }

    fn parse_case_body(&mut self) -> Result<List, ParseError> {
        let mut entries = Vec::new();
        self.skip_newlines();

        loop {
            if self.at_word("esac")
                || self.case_terminator().is_some()
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

            if background {
                self.skip_newlines();
            } else if matches!(self.kind(), TokenKind::Semi) {
                if self.case_terminator().is_some() {
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
                TokenKind::Word(_) => {
                    let word = self.take_word()?;
                    if simple.words.is_empty()
                        && let Some(mut assignment) = as_assignment(&word)
                    {
                        if matches!(self.kind(), TokenKind::LParen) {
                            assignment.value =
                                AssignmentValue::Array(self.parse_array_elements()?);
                        }
                        simple.assignments.push(assignment);
                    } else if simple
                        .words
                        .first()
                        .and_then(literal)
                        .is_some_and(|word| word == "declare")
                        && let Some(mut assignment) = as_assignment(&word)
                        && matches!(self.kind(), TokenKind::LParen)
                    {
                        assignment.value =
                            AssignmentValue::Array(self.parse_array_elements()?);
                        simple.words.push(Word {
                            parts: vec![Part::Literal(assignment.name.clone())],
                        });
                        simple.assignments.push(assignment);
                    } else {
                        simple.words.push(word);
                    }
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

    fn parse_array_elements(&mut self) -> Result<Vec<Word>, ParseError> {
        self.advance();
        let mut elements = Vec::new();

        loop {
            match self.kind() {
                TokenKind::RParen => {
                    self.advance();
                    return Ok(elements);
                }
                TokenKind::Word(_) => elements.push(self.take_word()?),
                TokenKind::Newline => {
                    self.advance();
                }
                TokenKind::Eof => {
                    return Err(ParseError::new("unclosed array assignment", true));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn redirect(&mut self) -> Result<Option<Redirect>, ParseError> {
        let (fd, op) = match self.kind() {
            TokenKind::RedirOut(fd) => (fd.unwrap_or(1), RedirectOp::Out),
            TokenKind::RedirAppend(fd) => (fd.unwrap_or(1), RedirectOp::Append),
            TokenKind::RedirIn(fd) => (fd.unwrap_or(0), RedirectOp::In),
            TokenKind::RedirHereString(fd) => (fd.unwrap_or(0), RedirectOp::HereString),
            TokenKind::RedirBoth => (1, RedirectOp::Both { append: false }),
            TokenKind::RedirBothAppend => (1, RedirectOp::Both { append: true }),
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
        let incomplete = matches!(self.kind(), TokenKind::Eof);
        let found = match self.kind() {
            TokenKind::Eof => "end of input".to_string(),
            TokenKind::Word(word) => format!("'{}'", literal(&word).unwrap_or_default()),
            other => format!("{other:?}"),
        };
        ParseError::new(format!("syntax error: unexpected {found}"), incomplete)
    }
}

fn as_assignment(word: &Word) -> Option<Assignment> {
    let Part::Literal(text) = word.parts.first()? else {
        return None;
    };
    let (prefix, rest) = text.split_once('=')?;

    let (name, index, append) = if let Some(open) = prefix.find('[') {
        if !prefix.ends_with(']') {
            return None;
        }
        let name = &prefix[..open];
        let index = &prefix[open + 1..prefix.len() - 1];
        if !is_name(name) || index.is_empty() {
            return None;
        }
        (name.to_string(), Some(index.to_string()), false)
    } else if let Some(name) = prefix.strip_suffix('+') {
        (name.to_string(), None, true)
    } else {
        (prefix.to_string(), None, false)
    };

    if !is_name(&name) {
        return None;
    }

    let mut parts = Vec::new();
    if !rest.is_empty() {
        parts.push(Part::Literal(rest.to_string()));
    }
    parts.extend(word.parts[1..].iter().cloned());
    Some(Assignment {
        name,
        index,
        append,
        value: AssignmentValue::Word(Word { parts }),
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
