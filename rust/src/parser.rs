use crate::ast::*;
use crate::lexer::{self, Part, Token, TokenKind, Word};

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
    pub incomplete: bool,
    pub line: usize,
    pub col: usize,
    pub snippet: Option<String>,
}

impl ParseError {
    fn new(message: impl Into<String>, incomplete: bool) -> Self {
        ParseError {
            message: message.into(),
            incomplete,
            line: 0,
            col: 0,
            snippet: None,
        }
    }

    fn with_location(mut self, line: usize, col: usize, snippet: Option<String>) -> Self {
        self.line = line;
        self.col = col;
        self.snippet = snippet;
        self
    }

    pub fn format_diagnostic(&self, prefix: &str) -> String {
        let loc = if self.line > 0 && self.col > 0 {
            format!(":{}:{}", self.line, self.col)
        } else if self.line > 0 {
            format!(":{}", self.line)
        } else {
            String::new()
        };
        let mut out = format!("{prefix}{loc}: {}", self.message);
        if let Some(snippet) = &self.snippet {
            out.push('\n');
            out.push_str(snippet);
        }
        out
    }
}

pub fn parse(source: &str) -> Result<Program, ParseError> {
    Parser::from_source(source)?.parse()
}

/// Reads a source the way the interactive shell feeds its evaluator: one
/// complete top-level command per call.
///
/// Bash parses and executes `eval` one input unit at a time, so a syntax error
/// only prevents the unit that contains it from running: everything above has
/// already happened. The unit boundary is a newline, matching bash, which
/// discards a whole physical line (`echo a; done` runs neither `echo a` nor
/// `done`) but keeps the lines before it.
pub struct Units {
    parser: Parser,
    finished: bool,
}

impl Units {
    pub fn new(source: &str) -> Result<Self, ParseError> {
        Ok(Units {
            parser: Parser::from_source(source)?,
            finished: false,
        })
    }

    /// The next complete statement, `None` at the end of the source, or the
    /// syntax error that ends parsing.
    pub fn next_unit(&mut self) -> Option<Result<Program, ParseError>> {
        if self.finished {
            return None;
        }
        match self.parser.parse_unit() {
            Ok(None) => {
                self.finished = true;
                None
            }
            Ok(Some(list)) => Some(Ok(Program { lines: vec![list] })),
            Err(err) => {
                self.finished = true;
                Some(Err(err))
            }
        }
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    line_starts: Vec<usize>,
    lines: Vec<String>,
    source_chars_len: usize,
}

impl Parser {
    fn from_source(source: &str) -> Result<Self, ParseError> {
        let line_starts: Vec<usize> = source
            .chars()
            .enumerate()
            .filter(|(_, char)| *char == '\n')
            .map(|(index, _)| index + 1)
            .collect();
        let lines: Vec<String> = source
            .split('\n')
            .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
            .collect();
        let source_chars_len = source.chars().count();
        let tokens = lexer::tokenize(source).map_err(|err| {
            let (line, col, snippet) = location_for_pos(&line_starts, &lines, err.pos);
            ParseError::new(format!("syntax error: {}", err.message), err.incomplete)
                .with_location(line, col, snippet)
        })?;
        Ok(Parser {
            tokens,
            pos: 0,
            line_starts,
            lines,
            source_chars_len,
        })
    }

    fn kind(&self) -> TokenKind {
        self.tokens
            .get(self.pos)
            .map(|token| token.kind.clone())
            .unwrap_or(TokenKind::Eof)
    }

    fn current_pos(&self) -> usize {
        self.tokens
            .get(self.pos)
            .map(|token| token.pos)
            .unwrap_or(self.source_chars_len)
    }

    fn current_line(&self) -> usize {
        let pos = self.current_pos();
        self.line_starts.partition_point(|start| *start <= pos) + 1
    }

    fn error(&self, message: impl Into<String>, incomplete: bool) -> ParseError {
        let (line, col, snippet) =
            location_for_pos(&self.line_starts, &self.lines, self.current_pos());
        ParseError::new(message, incomplete).with_location(line, col, snippet)
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
            lines.push(self.parse_line()?);
            if matches!(self.kind(), TokenKind::Newline | TokenKind::Semi) {
                self.skip_separators();
            } else if !matches!(self.kind(), TokenKind::Eof) {
                return Err(self.unexpected());
            }
        }

        Ok(Program { lines })
    }

    /// Parses one physical line: the commands joined by `;` on it. A newline or
    /// the end of the source ends the line, which is the input unit bash
    /// discards when one of its commands fails fatally.
    fn parse_line(&mut self) -> Result<List, ParseError> {
        let mut entries = Vec::new();
        loop {
            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry {
                line,
                background,
                and_or,
            });

            if background {
                if matches!(self.kind(), TokenKind::Newline | TokenKind::Eof) {
                    break;
                }
                // `a & b` keeps the next command on the same line.
                continue;
            }
            if matches!(self.kind(), TokenKind::Semi) {
                self.advance();
                continue;
            }
            break;
        }
        Ok(List { entries })
    }

    /// Parses one complete top-level statement. Blank leading separators are
    /// skipped, and the statement ends at the first top-level newline or at the
    /// end of the source; a compound command consumes its own newlines first.
    fn parse_unit(&mut self) -> Result<Option<List>, ParseError> {
        self.skip_separators();
        if matches!(self.kind(), TokenKind::Eof) {
            return Ok(None);
        }

        let mut entries = Vec::new();
        while !matches!(self.kind(), TokenKind::Eof) {
            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry {
                line,
                background,
                and_or,
            });

            if matches!(self.kind(), TokenKind::Newline) {
                self.advance();
                break;
            }
            if matches!(self.kind(), TokenKind::Semi) {
                self.advance();
                if matches!(self.kind(), TokenKind::Newline) {
                    self.advance();
                    break;
                }
                continue;
            }
            if !matches!(self.kind(), TokenKind::Eof) {
                return Err(self.unexpected());
            }
        }

        Ok(Some(List { entries }))
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
            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry {
                line,
                background,
                and_or,
            });

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
                    | TokenKind::PipeErr
                    | TokenKind::AndIf
                    | TokenKind::OrIf
            ) {
                return Ok(Pipeline {
                    commands: vec![Command::Simple(Simple::default())],
                    merge_stderr: Vec::new(),
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
        let mut merge_stderr = Vec::new();
        while matches!(self.kind(), TokenKind::Pipe | TokenKind::PipeErr) {
            merge_stderr.push(matches!(self.kind(), TokenKind::PipeErr));
            self.advance();
            self.skip_newlines();
            commands.push(self.parse_pipeline_command()?);
        }

        Ok(Pipeline {
            commands,
            merge_stderr,
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
        // A reserved word that cannot begin a command is bash's "syntax error
        // near unexpected token": `done`/`fi`/`then` above their opening
        // construct must not be run as commands.
        if let TokenKind::Word(word) = self.kind()
            && let Some(text) = literal(&word)
            && matches!(
                text.as_str(),
                "then" | "elif" | "else" | "fi" | "do" | "done" | "esac" | "in"
            )
        {
            return Err(self.error(format!("syntax error: unexpected '{text}'"), false));
        }

        let command = if self.at_word("coproc") {
            self.parse_coproc()?
        } else if let Some(compound) = self.parse_compound()? {
            compound
        } else if self.at_word("function") {
            self.parse_function_keyword()?
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

    fn parse_compound(&mut self) -> Result<Option<Command>, ParseError> {
        if self.at_word("if") {
            Ok(Some(self.parse_if()?))
        } else if self.at_word("for") {
            Ok(Some(self.parse_for()?))
        } else if self.at_word("while") {
            Ok(Some(self.parse_while(false)?))
        } else if self.at_word("until") {
            Ok(Some(self.parse_while(true)?))
        } else if self.at_word("case") {
            Ok(Some(self.parse_case()?))
        } else if matches!(self.kind(), TokenKind::TestStart) {
            Ok(Some(self.parse_test()?))
        } else if let TokenKind::Arithmetic(source) = self.kind() {
            self.advance();
            Ok(Some(Command::Arithmetic(source)))
        } else if matches!(self.kind(), TokenKind::LBrace) {
            Ok(Some(Command::Group(self.parse_group()?)))
        } else if matches!(self.kind(), TokenKind::LParen) {
            Ok(Some(Command::Subshell(self.parse_subshell()?)))
        } else {
            Ok(None)
        }
    }

    fn parse_coproc(&mut self) -> Result<Command, ParseError> {
        self.advance();
        if matches!(
            self.kind(),
            TokenKind::Eof | TokenKind::Newline | TokenKind::Semi | TokenKind::Amp
        ) {
            return Err(self.error("syntax error: coproc requires a command", true));
        }
        let next_starts_compound = self
            .tokens
            .get(self.pos + 1)
            .is_some_and(|token| kind_starts_compound(&token.kind));
        let name = if next_starts_compound
            && let TokenKind::Word(word) = self.kind()
            && let Some(text) = literal(&word)
            && is_name(&text)
        {
            self.advance();
            text
        } else {
            "COPROC".to_string()
        };
        let command = if let Some(compound) = self.parse_compound()? {
            compound
        } else {
            Command::Simple(self.parse_simple()?)
        };
        Ok(Command::Coproc {
            name,
            command: Box::new(command),
        })
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
                TokenKind::Eof => return Err(self.error("unclosed [[", true)),
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
        let line = self.current_line();
        let name = literal(&self.take_word()?).unwrap_or_default();
        self.advance();
        self.advance();
        self.skip_newlines();
        let body = self.parse_function_body()?;
        Ok(Command::Function { name, body, line })
    }

    fn parse_function_keyword(&mut self) -> Result<Command, ParseError> {
        let line = self.current_line();
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
        Ok(Command::Function { name, body, line })
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
            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry {
                line,
                background,
                and_or,
            });
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
                return Err(self.error("syntax error: missing condition after 'if'", false));
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
                return Err(self.error(
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
            return Err(self.error("syntax error: missing loop condition", false));
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
                return Err(self.error("unclosed case", true));
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

            let line = self.current_line();
            let and_or = self.parse_and_or()?;
            let background = matches!(self.kind(), TokenKind::Amp);
            if background {
                self.advance();
            }
            entries.push(Entry {
                line,
                background,
                and_or,
            });

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
        let mut simple = Simple {
            line: self.current_line(),
            ..Simple::default()
        };

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
                        .is_some_and(|word| {
                            matches!(word.as_str(), "declare" | "local" | "readonly" | "typeset")
                        })
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
                    return Err(self.error("unclosed array assignment", true));
                }
                _ => return Err(self.unexpected()),
            }
        }
    }

    fn redirect(&mut self) -> Result<Option<Redirect>, ParseError> {
        let (fd, op) = match self.kind() {
            TokenKind::RedirOut(fd) => (fd.unwrap_or(1), RedirectOp::Out),
            TokenKind::RedirOutForce(fd) => (fd.unwrap_or(1), RedirectOp::OutForce),
            TokenKind::RedirAppend(fd) => (fd.unwrap_or(1), RedirectOp::Append),
            TokenKind::RedirIn(fd) => (fd.unwrap_or(0), RedirectOp::In),
            TokenKind::RedirHereString(fd) => (fd.unwrap_or(0), RedirectOp::HereString),
            TokenKind::RedirBoth => (1, RedirectOp::Both { append: false }),
            TokenKind::RedirBothAppend => (1, RedirectOp::Both { append: true }),
            TokenKind::RedirDupWord(fd) => (fd, RedirectOp::DupWord),
            TokenKind::RedirDupInWord(fd) => (fd, RedirectOp::DupInWord),
            TokenKind::RedirDup(to, from) => {
                self.advance();
                return Ok(Some(Redirect {
                    fd: to,
                    op: RedirectOp::Dup(from),
                    target: None,
                }));
            }
            TokenKind::RedirDupIn(to, from) => {
                self.advance();
                return Ok(Some(Redirect {
                    fd: to,
                    op: RedirectOp::DupIn(from),
                    target: None,
                }));
            }
            TokenKind::RedirClose(fd, _input) => {
                self.advance();
                return Ok(Some(Redirect {
                    fd,
                    op: RedirectOp::Close,
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
            return Err(self.error("syntax error: redirect without a target", false));
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
        self.error(format!("syntax error: unexpected {found}"), incomplete)
    }
}

fn as_assignment(word: &Word) -> Option<Assignment> {
    let eq_part = word
        .parts
        .iter()
        .position(|part| matches!(part, Part::Literal(text) if text.contains('=')))?;
    let Part::Literal(eq_text) = &word.parts[eq_part] else {
        return None;
    };
    let eq = eq_text.find('=')?;

    let mut prefix_parts: Vec<Part> = word.parts[..eq_part].to_vec();
    if eq > 0 {
        prefix_parts.push(Part::Literal(eq_text[..eq].to_string()));
    }
    let (name, index, append) = split_assignment_prefix(&prefix_parts)?;

    let mut parts = Vec::new();
    if eq + 1 < eq_text.len() {
        parts.push(Part::Literal(eq_text[eq + 1..].to_string()));
    }
    parts.extend(word.parts[eq_part + 1..].iter().cloned());
    Some(Assignment {
        name,
        index,
        append,
        value: AssignmentValue::Word(lexer::expand_assignment_tildes(&Word { parts })),
    })
}

fn split_assignment_prefix(parts: &[Part]) -> Option<(String, Option<Word>, bool)> {
    let mut name = String::new();
    let mut index_parts: Vec<Part> = Vec::new();
    let mut in_index = false;
    let mut closed = false;
    let mut append = false;

    for (part_index, part) in parts.iter().enumerate() {
        let last_part = part_index + 1 == parts.len();
        match part {
            Part::Literal(text) => {
                let mut start = 0;
                while start < text.len() {
                    let rest = &text[start..];
                    if !in_index && !closed {
                        if let Some(open) = rest.find('[') {
                            name.push_str(&rest[..open]);
                            in_index = true;
                            start += open + 1;
                        } else if let Some(plus) = rest.find('+') {
                            if !rest[plus + 1..].is_empty() {
                                return None;
                            }
                            name.push_str(&rest[..plus]);
                            append = true;
                            start = text.len();
                        } else {
                            name.push_str(rest);
                            start = text.len();
                        }
                    } else if in_index {
                        if let Some(close) = rest.find(']') {
                            if close > 0 {
                                index_parts.push(Part::Literal(rest[..close].to_string()));
                            }
                            if !rest[close + 1..].is_empty() {
                                return None;
                            }
                            closed = true;
                            in_index = false;
                            start = text.len();
                        } else {
                            index_parts.push(Part::Literal(rest.to_string()));
                            start = text.len();
                        }
                    } else if rest == "+" && last_part {
                        append = true;
                        start = text.len();
                    } else {
                        return None;
                    }
                }
            }
            other if in_index => index_parts.push(other.clone()),
            _ => return None,
        }
    }

    if name.is_empty() || in_index || !is_name(&name) {
        return None;
    }
    let index = if index_parts.is_empty() {
        None
    } else {
        Some(Word { parts: index_parts })
    };
    Some((name, index, append))
}

fn location_for_pos(
    line_starts: &[usize],
    lines: &[String],
    pos: usize,
) -> (usize, usize, Option<String>) {
    let line_idx = line_starts.partition_point(|start| *start <= pos);
    let line = line_idx + 1;
    let line_start = line_idx
        .checked_sub(1)
        .and_then(|idx| line_starts.get(idx).copied())
        .unwrap_or(0);
    let col = pos.saturating_sub(line_start) + 1;
    let snippet = lines.get(line_idx).map(|src| {
        let caret_offset = col.saturating_sub(1).min(src.chars().count());
        format!("  | {src}\n  | {}^", " ".repeat(caret_offset))
    });
    (line, col, snippet)
}

fn kind_starts_compound(kind: &TokenKind) -> bool {
    match kind {
        TokenKind::TestStart | TokenKind::Arithmetic(_) | TokenKind::LBrace | TokenKind::LParen => {
            true
        }
        TokenKind::Word(word) => matches!(
            literal(word).as_deref(),
            Some("if" | "for" | "while" | "until" | "case")
        ),
        _ => false,
    }
}

fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|char| char.is_ascii_alphanumeric() || char == '_')
}
