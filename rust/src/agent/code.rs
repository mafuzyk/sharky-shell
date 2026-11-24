//! Native programmatic Code Mode micro-runtime (`op:"code"` and `sharky code`).
//!
//! Executes a bounded, deterministic JSON-value scripting language directly
//! inside the agent runtime, reusing the session's `Shell`, `supervisor`,
//! `attempt`, `parallel`, `workflow`, and `CaptureStore` machinery with zero
//! external dependencies.
//!
//! Every `type:"code"` response carries an unforgeable [`CodeAudit`] envelope
//! tracking child calls, non-zero failures, cancellations, VM steps, and stored
//! `capture_id` references so script transforms cannot conceal failing work.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use super::json::{
    CaptureResult, CodeAudit, CodeResult, ErrorCode, ExecResult, GroupState, Limits,
    ParallelResult, Response, StateResult, Termination, Value, WorkflowResult, parse_line,
};
use super::projection;
use super::protocol::{self, CodeRequest};
use super::supervisor;

const MAX_PARSE_DEPTH: usize = 32;
const MAX_EVAL_DEPTH: usize = 32;
const SERVICE_STRIDE: usize = 32;

/// Host capability surface provided by [`super::Session`] (or a unit-test mock).
pub(crate) trait CodeHost {
    fn limits(&self) -> Limits;
    fn call_exec(&mut self, request: &protocol::ExecRequest) -> Result<ExecResult, String>;
    fn call_parallel(
        &mut self,
        request: &protocol::ParallelRequest,
    ) -> Result<ParallelResult, String>;
    fn call_workflow(
        &mut self,
        request: &protocol::WorkflowRequest,
    ) -> Result<WorkflowResult, String>;
    fn call_state(&mut self, request: &protocol::StateRequest) -> StateResult;
    fn call_read_capture(
        &self,
        request: &protocol::ReadCaptureRequest,
    ) -> Result<CaptureResult, (ErrorCode, String)>;
}

/// Runs one validated [`CodeRequest`] and produces its single [`CodeResult`].
pub(crate) fn execute<H: CodeHost>(host: &mut H, request: &CodeRequest) -> CodeResult {
    let started = Instant::now();
    let mut audit = CodeAudit::default();
    let mut seen_captures = HashSet::new();

    let ast = match parse_program(&request.source) {
        Ok(stmts) => stmts,
        Err(message) => {
            let elapsed = started.elapsed();
            let mut result = CodeResult::new(request.id.clone());
            result.status = 2;
            result.duration_us = elapsed.as_micros().min(i64::MAX as u128) as i64;
            result.duration_ms = result.duration_us / 1000;
            result.error = Some(message);
            result.audit = audit;
            return result;
        }
    };

    let deadline = request
        .timeout_ms
        .map(|ms| started + Duration::from_millis(ms.max(0) as u64));
    supervisor::begin_request(&request.id, request.timeout_ms);

    let mut env = Env {
        host,
        request,
        deadline,
        audit: &mut audit,
        seen_captures: &mut seen_captures,
        scopes: vec![BTreeMap::from([(
            String::from("args"),
            RuntimeVal::Json(request.args.clone()),
        )])],
        depth: 0,
        steps_since_service: 0,
    };

    let outcome = env.eval_block(&ast);
    let supervised = supervisor::end_request();

    let elapsed = started.elapsed();
    let mut result = CodeResult::new(request.id.clone());
    result.duration_us = elapsed.as_micros().min(i64::MAX as u128) as i64;
    result.duration_ms = result.duration_us / 1000;

    match outcome {
        Ok(Flow::None(val)) | Ok(Flow::Return(val)) => {
            if let Some(super_outcome) = supervised {
                apply_supervision_outcome(&mut result, super_outcome);
            } else {
                match val.into_json() {
                    Ok(json) => result.value = json,
                    Err(msg) => {
                        result.status = 1;
                        result.error = Some(msg);
                    }
                }
            }
        }
        Ok(Flow::Break) => {
            result.status = 1;
            result.error = Some(String::from("break used outside a loop"));
        }
        Ok(Flow::Continue) => {
            result.status = 1;
            result.error = Some(String::from("continue used outside a loop"));
        }
        Err(Abort::Cancelled) => apply_supervision_outcome(&mut result, supervisor::Outcome::Cancelled),
        Err(Abort::Timeout) => apply_supervision_outcome(&mut result, supervisor::Outcome::Timeout),
        Err(Abort::Runtime(message)) => {
            if let Some(super_outcome) = supervised {
                apply_supervision_outcome(&mut result, super_outcome);
            } else {
                result.status = 1;
                result.error = Some(message);
            }
        }
    }

    result.audit = audit;
    result
}

fn apply_supervision_outcome(result: &mut CodeResult, outcome: supervisor::Outcome) {
    match outcome {
        supervisor::Outcome::Cancelled => {
            result.status = 143;
            result.termination = Termination::Cancelled;
            result.value = Value::Null;
        }
        supervisor::Outcome::Timeout => {
            result.status = 124;
            result.termination = Termination::Timeout;
            result.value = Value::Null;
        }
    }
}

// --------------------------------------------------------------------- Lexer

#[derive(Debug, Clone, PartialEq, Eq)]
enum TokenKind {
    Ident(String),
    Int(i64),
    Str(String),
    Let,
    If,
    Else,
    For,
    In,
    While,
    Break,
    Continue,
    Return,
    True,
    False,
    Null,
    Arrow,
    EqEq,
    NotEq,
    Le,
    Ge,
    AndAnd,
    OrOr,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Semi,
    Colon,
    Dot,
    Eq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Lt,
    Gt,
    Question,
    Eof,
}

#[derive(Debug, Clone)]
struct Token {
    kind: TokenKind,
    line: usize,
    col: usize,
}

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let mut line = 1usize;
    let mut col = 1usize;

    let err = |line: usize, col: usize, msg: &str| format!("syntax error at {line}:{col}: {msg}");

    while i < bytes.len() {
        let ch = bytes[i];
        match ch {
            b' ' | b'\t' | b'\r' => {
                i += 1;
                col += 1;
            }
            b'\n' => {
                i += 1;
                line += 1;
                col = 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                col += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                    col += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let start_line = line;
                let start_col = col;
                i += 2;
                col += 2;
                let mut closed = false;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        col += 2;
                        closed = true;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        line += 1;
                        col = 1;
                    } else {
                        col += 1;
                    }
                    i += 1;
                }
                if !closed {
                    return Err(err(start_line, start_col, "unterminated block comment"));
                }
            }
            b'"' | b'\'' => {
                let quote = ch;
                let start_line = line;
                let start_col = col;
                i += 1;
                col += 1;
                let mut text = String::new();
                let mut closed = false;
                while i < source.len() {
                    let slice = &source[i..];
                    let Some(c) = slice.chars().next() else {
                        break;
                    };
                    if c as u32 == u32::from(quote) {
                        i += c.len_utf8();
                        col += 1;
                        closed = true;
                        break;
                    }
                    if c == '\\' {
                        i += 1;
                        col += 1;
                        let Some(esc) = source[i..].chars().next() else {
                            return Err(err(start_line, start_col, "unterminated escape"));
                        };
                        i += esc.len_utf8();
                        col += 1;
                        match esc {
                            'n' => text.push('\n'),
                            'r' => text.push('\r'),
                            't' => text.push('\t'),
                            '\\' => text.push('\\'),
                            '"' => text.push('"'),
                            '\'' => text.push('\''),
                            '0' => text.push('\0'),
                            other => return Err(err(line, col, &format!("invalid escape \\{other}"))),
                        }
                    } else {
                        if c == '\n' {
                            line += 1;
                            col = 1;
                        } else {
                            col += 1;
                        }
                        text.push(c);
                        i += c.len_utf8();
                    }
                }
                if !closed {
                    return Err(err(start_line, start_col, "unterminated string literal"));
                }
                tokens.push(Token {
                    kind: TokenKind::Str(text),
                    line: start_line,
                    col: start_col,
                });
            }
            b'0'..=b'9' => {
                let start = i;
                let start_col = col;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                    col += 1;
                }
                let raw = &source[start..i];
                let num = raw
                    .parse::<i64>()
                    .map_err(|_| err(line, start_col, "integer literal out of range"))?;
                tokens.push(Token {
                    kind: TokenKind::Int(num),
                    line,
                    col: start_col,
                });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                let start = i;
                let start_col = col;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                    col += 1;
                }
                let word = &source[start..i];
                let kind = match word {
                    "let" => TokenKind::Let,
                    "if" => TokenKind::If,
                    "else" => TokenKind::Else,
                    "for" => TokenKind::For,
                    "in" => TokenKind::In,
                    "while" => TokenKind::While,
                    "break" => TokenKind::Break,
                    "continue" => TokenKind::Continue,
                    "return" => TokenKind::Return,
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    "null" => TokenKind::Null,
                    _ => TokenKind::Ident(word.to_string()),
                };
                tokens.push(Token {
                    kind,
                    line,
                    col: start_col,
                });
            }
            _ => {
                let start_col = col;
                let next = bytes.get(i + 1).copied();
                let (kind, len) = match (ch, next) {
                    (b'=', Some(b'>')) => (TokenKind::Arrow, 2),
                    (b'=', Some(b'=')) => (TokenKind::EqEq, 2),
                    (b'!', Some(b'=')) => (TokenKind::NotEq, 2),
                    (b'<', Some(b'=')) => (TokenKind::Le, 2),
                    (b'>', Some(b'=')) => (TokenKind::Ge, 2),
                    (b'&', Some(b'&')) => (TokenKind::AndAnd, 2),
                    (b'|', Some(b'|')) => (TokenKind::OrOr, 2),
                    (b'(', _) => (TokenKind::LParen, 1),
                    (b')', _) => (TokenKind::RParen, 1),
                    (b'[', _) => (TokenKind::LBracket, 1),
                    (b']', _) => (TokenKind::RBracket, 1),
                    (b'{', _) => (TokenKind::LBrace, 1),
                    (b'}', _) => (TokenKind::RBrace, 1),
                    (b',', _) => (TokenKind::Comma, 1),
                    (b';', _) => (TokenKind::Semi, 1),
                    (b':', _) => (TokenKind::Colon, 1),
                    (b'.', _) => (TokenKind::Dot, 1),
                    (b'=', _) => (TokenKind::Eq, 1),
                    (b'+', _) => (TokenKind::Plus, 1),
                    (b'-', _) => (TokenKind::Minus, 1),
                    (b'*', _) => (TokenKind::Star, 1),
                    (b'/', _) => (TokenKind::Slash, 1),
                    (b'%', _) => (TokenKind::Percent, 1),
                    (b'!', _) => (TokenKind::Bang, 1),
                    (b'<', _) => (TokenKind::Lt, 1),
                    (b'>', _) => (TokenKind::Gt, 1),
                    (b'?', _) => (TokenKind::Question, 1),
                    _ => return Err(err(line, col, &format!("unexpected character '{}'", ch as char))),
                };
                i += len;
                col += len;
                tokens.push(Token {
                    kind,
                    line,
                    col: start_col,
                });
            }
        }
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        line,
        col,
    });
    Ok(tokens)
}

// ----------------------------------------------------------------------- AST

#[derive(Debug, Clone)]
enum Stmt {
    Let(String, Expr),
    LetArray(Vec<String>, Expr),
    Assign(String, Expr),
    If {
        cond: Expr,
        then_branch: Vec<Stmt>,
        else_branch: Option<Vec<Stmt>>,
    },
    For {
        var: String,
        iter: Expr,
        body: Vec<Stmt>,
    },
    While {
        cond: Expr,
        body: Vec<Stmt>,
    },
    Break,
    Continue,
    Return(Option<Expr>),
    Expr(Expr),
}

#[derive(Debug, Clone)]
enum Expr {
    Literal(Value),
    Ident(String),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Ternary(Box<Expr>, Box<Expr>, Box<Expr>),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(String, Vec<Expr>),
    MethodCall(Box<Expr>, String, Vec<Expr>),
    Lambda(Vec<String>, LambdaBody),
}

#[derive(Debug, Clone)]
enum LambdaBody {
    Expr(Box<Expr>),
    Block(Vec<Stmt>),
}

#[derive(Debug, Clone, Copy)]
enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy)]
enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

// -------------------------------------------------------------------- Parser

fn parse_program(source: &str) -> Result<Vec<Stmt>, String> {
    let tokens = lex(source)?;
    let mut parser = Parser { tokens, pos: 0 };
    let mut stmts = Vec::new();
    while !matches!(parser.peek_kind(), TokenKind::Eof) {
        if matches!(parser.peek_kind(), TokenKind::Semi) {
            parser.pos += 1;
            continue;
        }
        stmts.push(parser.parse_stmt(0)?);
    }
    Ok(stmts)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.pos.min(self.tokens.len() - 1)]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn peek_next_kind(&self) -> &TokenKind {
        let next = (self.pos + 1).min(self.tokens.len() - 1);
        &self.tokens[next].kind
    }

    fn error_here(&self, msg: &str) -> String {
        let tok = self.peek();
        format!("syntax error at {}:{}: {msg}", tok.line, tok.col)
    }

    fn eat_semi(&mut self) {
        while matches!(self.peek_kind(), TokenKind::Semi) {
            self.pos += 1;
        }
    }

    fn expect_ident(&mut self, msg: &str) -> Result<String, String> {
        match self.peek_kind().clone() {
            TokenKind::Ident(name) => {
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.error_here(msg)),
        }
    }

    fn expect(&mut self, expected: TokenKind, msg: &str) -> Result<(), String> {
        if self.peek_kind() == &expected {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.error_here(msg))
        }
    }

    fn parse_block(&mut self, depth: usize) -> Result<Vec<Stmt>, String> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(self.error_here("maximum nesting depth exceeded"));
        }
        self.expect(TokenKind::LBrace, "expected '{'")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
            if matches!(self.peek_kind(), TokenKind::Semi) {
                self.pos += 1;
                continue;
            }
            stmts.push(self.parse_stmt(depth + 1)?);
        }
        self.expect(TokenKind::RBrace, "expected '}'")?;
        Ok(stmts)
    }

    fn parse_stmt(&mut self, depth: usize) -> Result<Stmt, String> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(self.error_here("maximum nesting depth exceeded"));
        }
        let stmt = match self.peek_kind().clone() {
            TokenKind::Let => {
                self.pos += 1;
                if matches!(self.peek_kind(), TokenKind::LBracket) {
                    self.pos += 1;
                    let mut names = Vec::new();
                    while !matches!(self.peek_kind(), TokenKind::RBracket | TokenKind::Eof) {
                        names.push(self.expect_ident("expected variable name in destructuring")?);
                        if matches!(self.peek_kind(), TokenKind::Comma) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                    self.expect(TokenKind::RBracket, "expected ']'")?;
                    self.expect(TokenKind::Eq, "expected '=' after let")?;
                    let rhs = self.parse_expr(depth + 1)?;
                    Stmt::LetArray(names, rhs)
                } else {
                    let name = self.expect_ident("expected variable name after let")?;
                    self.expect(TokenKind::Eq, "expected '=' after variable name")?;
                    let rhs = self.parse_expr(depth + 1)?;
                    Stmt::Let(name, rhs)
                }
            }
            TokenKind::If => {
                self.pos += 1;
                let cond = self.parse_expr(depth + 1)?;
                let then_branch = self.parse_block(depth + 1)?;
                let else_branch = if matches!(self.peek_kind(), TokenKind::Else) {
                    self.pos += 1;
                    if matches!(self.peek_kind(), TokenKind::If) {
                        Some(vec![self.parse_stmt(depth + 1)?])
                    } else {
                        Some(self.parse_block(depth + 1)?)
                    }
                } else {
                    None
                };
                Stmt::If {
                    cond,
                    then_branch,
                    else_branch,
                }
            }
            TokenKind::For => {
                self.pos += 1;
                let var = self.expect_ident("expected loop variable name after for")?;
                self.expect(TokenKind::In, "expected 'in' after loop variable")?;
                let iter = self.parse_expr(depth + 1)?;
                let body = self.parse_block(depth + 1)?;
                Stmt::For { var, iter, body }
            }
            TokenKind::While => {
                self.pos += 1;
                let cond = self.parse_expr(depth + 1)?;
                let body = self.parse_block(depth + 1)?;
                Stmt::While { cond, body }
            }
            TokenKind::Break => {
                self.pos += 1;
                Stmt::Break
            }
            TokenKind::Continue => {
                self.pos += 1;
                Stmt::Continue
            }
            TokenKind::Return => {
                self.pos += 1;
                if matches!(
                    self.peek_kind(),
                    TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
                ) {
                    Stmt::Return(None)
                } else {
                    Stmt::Return(Some(self.parse_expr(depth + 1)?))
                }
            }
            TokenKind::Ident(name) if matches!(self.peek_next_kind(), TokenKind::Eq) => {
                self.pos += 2;
                let rhs = self.parse_expr(depth + 1)?;
                Stmt::Assign(name, rhs)
            }
            _ => Stmt::Expr(self.parse_expr(depth + 1)?),
        };
        self.eat_semi();
        Ok(stmt)
    }

    fn parse_expr(&mut self, depth: usize) -> Result<Expr, String> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(self.error_here("maximum expression depth exceeded"));
        }
        let cond = self.parse_or(depth)?;
        if matches!(self.peek_kind(), TokenKind::Question) {
            self.pos += 1;
            let then_expr = self.parse_expr(depth + 1)?;
            self.expect(TokenKind::Colon, "expected ':' in ternary expression")?;
            let else_expr = self.parse_expr(depth + 1)?;
            return Ok(Expr::Ternary(
                Box::new(cond),
                Box::new(then_expr),
                Box::new(else_expr),
            ));
        }
        Ok(cond)
    }

    fn parse_or(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_and(depth)?;
        while matches!(self.peek_kind(), TokenKind::OrOr) {
            self.pos += 1;
            let rhs = self.parse_and(depth)?;
            lhs = Expr::Binary(BinOp::Or, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_eq(depth)?;
        while matches!(self.peek_kind(), TokenKind::AndAnd) {
            self.pos += 1;
            let rhs = self.parse_eq(depth)?;
            lhs = Expr::Binary(BinOp::And, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_cmp(depth)?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::NotEq => BinOp::Ne,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_cmp(depth)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_add(depth)?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_add(depth)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_add(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_mul(depth)?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_mul(depth)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self, depth: usize) -> Result<Expr, String> {
        let mut lhs = self.parse_unary(depth)?;
        loop {
            let op = match self.peek_kind() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_unary(depth)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self, depth: usize) -> Result<Expr, String> {
        match self.peek_kind() {
            TokenKind::Bang => {
                self.pos += 1;
                let expr = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Not, Box::new(expr)))
            }
            TokenKind::Minus => {
                self.pos += 1;
                let expr = self.parse_unary(depth + 1)?;
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(expr)))
            }
            _ => self.parse_postfix(depth),
        }
    }

    fn parse_postfix(&mut self, depth: usize) -> Result<Expr, String> {
        let mut expr = self.parse_primary(depth)?;
        loop {
            match self.peek_kind() {
                TokenKind::Dot => {
                    self.pos += 1;
                    let name = self.expect_ident("expected property or method name after '.'")?;
                    if matches!(self.peek_kind(), TokenKind::LParen) {
                        let args = self.parse_arg_list(depth + 1)?;
                        expr = Expr::MethodCall(Box::new(expr), name, args);
                    } else {
                        expr = Expr::Field(Box::new(expr), name);
                    }
                }
                TokenKind::LBracket => {
                    self.pos += 1;
                    let idx = self.parse_expr(depth + 1)?;
                    self.expect(TokenKind::RBracket, "expected ']'")?;
                    expr = Expr::Index(Box::new(expr), Box::new(idx));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn is_arrow_params(&self) -> bool {
        if !matches!(self.peek_kind(), TokenKind::LParen) {
            return false;
        }
        let mut idx = self.pos + 1;
        if idx < self.tokens.len() && matches!(self.tokens[idx].kind, TokenKind::RParen) {
            return idx + 1 < self.tokens.len()
                && matches!(self.tokens[idx + 1].kind, TokenKind::Arrow);
        }
        loop {
            if idx >= self.tokens.len() || !matches!(self.tokens[idx].kind, TokenKind::Ident(_)) {
                return false;
            }
            idx += 1;
            if idx >= self.tokens.len() {
                return false;
            }
            match self.tokens[idx].kind {
                TokenKind::Comma => idx += 1,
                TokenKind::RParen => {
                    return idx + 1 < self.tokens.len()
                        && matches!(self.tokens[idx + 1].kind, TokenKind::Arrow);
                }
                _ => return false,
            }
        }
    }

    fn parse_lambda_body(&mut self, depth: usize) -> Result<LambdaBody, String> {
        self.expect(TokenKind::Arrow, "expected '=>'")?;
        if matches!(self.peek_kind(), TokenKind::LBrace) {
            Ok(LambdaBody::Block(self.parse_block(depth + 1)?))
        } else {
            Ok(LambdaBody::Expr(Box::new(self.parse_expr(depth + 1)?)))
        }
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Expr, String> {
        if depth >= MAX_PARSE_DEPTH {
            return Err(self.error_here("maximum expression depth exceeded"));
        }
        match self.peek_kind().clone() {
            TokenKind::Null => {
                self.pos += 1;
                Ok(Expr::Literal(Value::Null))
            }
            TokenKind::True => {
                self.pos += 1;
                Ok(Expr::Literal(Value::Bool(true)))
            }
            TokenKind::False => {
                self.pos += 1;
                Ok(Expr::Literal(Value::Bool(false)))
            }
            TokenKind::Int(n) => {
                self.pos += 1;
                Ok(Expr::Literal(Value::Int(n)))
            }
            TokenKind::Str(s) => {
                self.pos += 1;
                Ok(Expr::Literal(Value::String(s)))
            }
            TokenKind::LParen if self.is_arrow_params() => {
                self.pos += 1;
                let mut params = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RParen) {
                    params.push(self.expect_ident("expected parameter name")?);
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(TokenKind::RParen, "expected ')'")?;
                let body = self.parse_lambda_body(depth)?;
                Ok(Expr::Lambda(params, body))
            }
            TokenKind::LParen => {
                self.pos += 1;
                let expr = self.parse_expr(depth + 1)?;
                self.expect(TokenKind::RParen, "expected ')'")?;
                Ok(expr)
            }
            TokenKind::LBracket => {
                self.pos += 1;
                let mut items = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RBracket | TokenKind::Eof) {
                    items.push(self.parse_expr(depth + 1)?);
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(TokenKind::RBracket, "expected ']'")?;
                Ok(Expr::Array(items))
            }
            TokenKind::LBrace => {
                self.pos += 1;
                let mut entries = Vec::new();
                while !matches!(self.peek_kind(), TokenKind::RBrace | TokenKind::Eof) {
                    let (key, shorthand) = match self.peek_kind().clone() {
                        TokenKind::Str(k) => {
                            self.pos += 1;
                            (k, false)
                        }
                        TokenKind::Ident(k) => {
                            self.pos += 1;
                            let is_shorthand = !matches!(self.peek_kind(), TokenKind::Colon);
                            (k, is_shorthand)
                        }
                        _ => return Err(self.error_here("expected object key")),
                    };
                    let val = if shorthand {
                        Expr::Ident(key.clone())
                    } else {
                        self.expect(TokenKind::Colon, "expected ':' after object key")?;
                        self.parse_expr(depth + 1)?
                    };
                    entries.push((key, val));
                    if matches!(self.peek_kind(), TokenKind::Comma) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect(TokenKind::RBrace, "expected '}'")?;
                Ok(Expr::Object(entries))
            }
            TokenKind::Ident(name) => {
                self.pos += 1;
                if matches!(self.peek_kind(), TokenKind::Arrow) {
                    let body = self.parse_lambda_body(depth)?;
                    Ok(Expr::Lambda(vec![name], body))
                } else if matches!(self.peek_kind(), TokenKind::LParen) {
                    let args = self.parse_arg_list(depth + 1)?;
                    Ok(Expr::Call(name, args))
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            _ => Err(self.error_here("unexpected token in expression")),
        }
    }

    fn parse_arg_list(&mut self, depth: usize) -> Result<Vec<Expr>, String> {
        self.expect(TokenKind::LParen, "expected '('")?;
        let mut args = Vec::new();
        while !matches!(self.peek_kind(), TokenKind::RParen | TokenKind::Eof) {
            args.push(self.parse_expr(depth)?);
            if matches!(self.peek_kind(), TokenKind::Comma) {
                self.pos += 1;
            } else {
                break;
            }
        }
        self.expect(TokenKind::RParen, "expected ')'")?;
        Ok(args)
    }
}

// ----------------------------------------------------------------- Evaluator

type ScopeFrame = BTreeMap<String, RuntimeVal>;

#[derive(Clone)]
enum RuntimeVal {
    Json(Value),
    Closure {
        params: Vec<String>,
        body: LambdaBody,
        captured: Vec<ScopeFrame>,
    },
}

impl RuntimeVal {
    fn as_json(&self) -> Result<&Value, Abort> {
        match self {
            RuntimeVal::Json(v) => Ok(v),
            RuntimeVal::Closure { .. } => {
                Err(Abort::Runtime(String::from("unexpected function value")))
            }
        }
    }

    fn into_json(self) -> Result<Value, String> {
        match self {
            RuntimeVal::Json(v) => Ok(v),
            RuntimeVal::Closure { .. } => Err(String::from("cannot return a function value")),
        }
    }

    fn is_truthy(&self) -> bool {
        match self {
            RuntimeVal::Json(Value::Null) => false,
            RuntimeVal::Json(Value::Bool(b)) => *b,
            RuntimeVal::Json(Value::Int(n)) => *n != 0,
            RuntimeVal::Json(Value::String(s)) => !s.is_empty(),
            RuntimeVal::Json(Value::Array(a)) => !a.is_empty(),
            RuntimeVal::Json(Value::Object(o)) => !o.is_empty(),
            RuntimeVal::Closure { .. } => true,
        }
    }
}

enum Flow {
    None(RuntimeVal),
    Return(RuntimeVal),
    Break,
    Continue,
}

enum Abort {
    Cancelled,
    Timeout,
    Runtime(String),
}

struct Env<'a, H: CodeHost> {
    host: &'a mut H,
    request: &'a CodeRequest,
    deadline: Option<Instant>,
    audit: &'a mut CodeAudit,
    seen_captures: &'a mut HashSet<String>,
    scopes: Vec<ScopeFrame>,
    depth: usize,
    steps_since_service: usize,
}

impl<'a, H: CodeHost> Env<'a, H> {
    fn tick(&mut self) -> Result<(), Abort> {
        self.audit.steps += 1;
        if self.audit.steps > self.request.max_steps {
            return Err(Abort::Runtime(format!(
                "code exceeded max_steps limit ({})",
                self.request.max_steps
            )));
        }
        if crate::signal::fatal_signal().is_some() {
            return Err(Abort::Cancelled);
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(Abort::Timeout);
        }
        self.steps_since_service += 1;
        if self.steps_since_service >= SERVICE_STRIDE {
            self.poll_supervision()?;
        }
        Ok(())
    }

    fn poll_supervision(&mut self) -> Result<(), Abort> {
        self.steps_since_service = 0;
        supervisor::service();
        if crate::signal::fatal_signal().is_some() {
            return Err(Abort::Cancelled);
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(Abort::Timeout);
        }
        match supervisor::pending_outcome() {
            Some(supervisor::Outcome::Cancelled) => Err(Abort::Cancelled),
            Some(supervisor::Outcome::Timeout) => Err(Abort::Timeout),
            None => Ok(()),
        }
    }

    fn remaining_timeout_ms(&self) -> Result<Option<i64>, Abort> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let now = Instant::now();
        if now >= deadline {
            return Err(Abort::Timeout);
        }
        let rem = (deadline - now).as_millis().min(i64::MAX as u128) as i64;
        Ok(Some(rem.max(1)))
    }

    fn restore_outer_supervision(&self) -> Result<(), Abort> {
        let rem = self.remaining_timeout_ms()?;
        supervisor::begin_request(&self.request.id, rem);
        Ok(())
    }

    fn lookup(&self, name: &str) -> Option<RuntimeVal> {
        for frame in self.scopes.iter().rev() {
            if let Some(val) = frame.get(name) {
                return Some(val.clone());
            }
        }
        None
    }

    fn assign_existing(&mut self, name: &str, value: RuntimeVal) -> Result<(), Abort> {
        for frame in self.scopes.iter_mut().rev() {
            if let Some(slot) = frame.get_mut(name) {
                *slot = value;
                return Ok(());
            }
        }
        Err(Abort::Runtime(format!("undefined variable '{name}'")))
    }

    fn define(&mut self, name: String, value: RuntimeVal) {
        if let Some(frame) = self.scopes.last_mut() {
            frame.insert(name, value);
        }
    }

    fn eval_block(&mut self, stmts: &[Stmt]) -> Result<Flow, Abort> {
        self.depth += 1;
        if self.depth > MAX_EVAL_DEPTH {
            self.depth -= 1;
            return Err(Abort::Runtime(String::from("maximum call depth exceeded")));
        }
        self.scopes.push(BTreeMap::new());
        let res = (|| -> Result<Flow, Abort> {
            let mut last = RuntimeVal::Json(Value::Null);
            for stmt in stmts {
                match self.eval_stmt(stmt)? {
                    Flow::None(v) => last = v,
                    flow @ (Flow::Return(_) | Flow::Break | Flow::Continue) => return Ok(flow),
                }
            }
            Ok(Flow::None(last))
        })();
        self.scopes.pop();
        self.depth -= 1;
        res
    }

    fn eval_stmt(&mut self, stmt: &Stmt) -> Result<Flow, Abort> {
        self.tick()?;
        match stmt {
            Stmt::Let(name, expr) => {
                let val = self.eval_expr(expr)?;
                self.define(name.clone(), val);
                Ok(Flow::None(RuntimeVal::Json(Value::Null)))
            }
            Stmt::LetArray(names, expr) => {
                let val = self.eval_expr(expr)?;
                let arr = val
                    .as_json()?
                    .as_array()
                    .ok_or_else(|| Abort::Runtime(String::from("array destructuring expects an array")))?;
                for (idx, name) in names.iter().enumerate() {
                    let elem = arr.get(idx).cloned().unwrap_or(Value::Null);
                    self.define(name.clone(), RuntimeVal::Json(elem));
                }
                Ok(Flow::None(RuntimeVal::Json(Value::Null)))
            }
            Stmt::Assign(name, expr) => {
                let val = self.eval_expr(expr)?;
                self.assign_existing(name, val.clone())?;
                Ok(Flow::None(val))
            }
            Stmt::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.eval_expr(cond)?;
                if c.is_truthy() {
                    self.eval_block(then_branch)
                } else if let Some(branch) = else_branch {
                    self.eval_block(branch)
                } else {
                    Ok(Flow::None(RuntimeVal::Json(Value::Null)))
                }
            }
            Stmt::For { var, iter, body } => {
                let seq_val = self.eval_expr(iter)?;
                let items = seq_val
                    .as_json()?
                    .as_array()
                    .ok_or_else(|| Abort::Runtime(String::from("for loop expects an array")))?
                    .to_vec();
                let mut last = RuntimeVal::Json(Value::Null);
                for item in items {
                    self.tick()?;
                    self.scopes.push(BTreeMap::from([(var.clone(), RuntimeVal::Json(item))]));
                    let flow = (|| -> Result<Flow, Abort> {
                        for inner in body {
                            match self.eval_stmt(inner)? {
                                Flow::None(v) => last = v,
                                flow => return Ok(flow),
                            }
                        }
                        Ok(Flow::None(last.clone()))
                    })();
                    self.scopes.pop();
                    match flow? {
                        Flow::None(_) | Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::None(last))
            }
            Stmt::While { cond, body } => {
                let mut last = RuntimeVal::Json(Value::Null);
                loop {
                    self.tick()?;
                    if !self.eval_expr(cond)?.is_truthy() {
                        break;
                    }
                    match self.eval_block(body)? {
                        Flow::None(v) => last = v,
                        Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Ok(ret),
                    }
                }
                Ok(Flow::None(last))
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            Stmt::Return(expr) => {
                let val = match expr {
                    Some(e) => self.eval_expr(e)?,
                    None => RuntimeVal::Json(Value::Null),
                };
                Ok(Flow::Return(val))
            }
            Stmt::Expr(expr) => {
                let val = self.eval_expr(expr)?;
                Ok(Flow::None(val))
            }
        }
    }

    fn eval_expr(&mut self, expr: &Expr) -> Result<RuntimeVal, Abort> {
        self.tick()?;
        match expr {
            Expr::Literal(v) => Ok(RuntimeVal::Json(v.clone())),
            Expr::Ident(name) => self
                .lookup(name)
                .ok_or_else(|| Abort::Runtime(format!("undefined variable '{name}'"))),
            Expr::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval_expr(item)?.into_json().map_err(Abort::Runtime)?);
                }
                Ok(RuntimeVal::Json(Value::Array(out)))
            }
            Expr::Object(entries) => {
                let mut out = Vec::with_capacity(entries.len());
                for (k, v) in entries {
                    let jv = self.eval_expr(v)?.into_json().map_err(Abort::Runtime)?;
                    if let Some(slot) = out.iter_mut().find(|(ek, _)| ek == k) {
                        slot.1 = jv;
                    } else {
                        out.push((k.clone(), jv));
                    }
                }
                Ok(RuntimeVal::Json(Value::Object(out)))
            }
            Expr::Unary(op, inner) => {
                let v = self.eval_expr(inner)?;
                match op {
                    UnaryOp::Not => Ok(RuntimeVal::Json(Value::Bool(!v.is_truthy()))),
                    UnaryOp::Neg => {
                        let n = v
                            .as_json()?
                            .as_i64()
                            .ok_or_else(|| Abort::Runtime(String::from("unary '-' expects an integer")))?;
                        n.checked_neg()
                            .map(|res| RuntimeVal::Json(Value::Int(res)))
                            .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))
                    }
                }
            }
            Expr::Binary(op, lhs, rhs) => self.eval_binary(*op, lhs, rhs),
            Expr::Ternary(cond, then_expr, else_expr) => {
                if self.eval_expr(cond)?.is_truthy() {
                    self.eval_expr(then_expr)
                } else {
                    self.eval_expr(else_expr)
                }
            }
            Expr::Field(obj_expr, key) => {
                let val = self.eval_expr(obj_expr)?;
                let json = val.as_json()?;
                match json {
                    Value::Object(_) => Ok(RuntimeVal::Json(
                        json.get(key).cloned().unwrap_or(Value::Null),
                    )),
                    other => Err(Abort::Runtime(format!(
                        "cannot access property .{key} on {}",
                        type_name(other)
                    ))),
                }
            }
            Expr::Index(target_expr, idx_expr) => {
                let target = self.eval_expr(target_expr)?;
                let idx = self.eval_expr(idx_expr)?;
                match (target.as_json()?, idx.as_json()?) {
                    (Value::Array(arr), Value::Int(i)) => {
                        let Ok(pos) = usize::try_from(*i) else {
                            return Err(Abort::Runtime(format!("array index {i} out of bounds")));
                        };
                        arr.get(pos)
                            .cloned()
                            .map(RuntimeVal::Json)
                            .ok_or_else(|| Abort::Runtime(format!("array index {pos} out of bounds")))
                    }
                    (Value::Object(_), Value::String(k)) => Ok(RuntimeVal::Json(
                        target.as_json()?.get(k).cloned().unwrap_or(Value::Null),
                    )),
                    _ => Err(Abort::Runtime(String::from("invalid index operation"))),
                }
            }
            Expr::Lambda(params, body) => Ok(RuntimeVal::Closure {
                params: params.clone(),
                body: body.clone(),
                captured: self.scopes.clone(),
            }),
            Expr::Call(name, arg_exprs) => {
                let mut args = Vec::with_capacity(arg_exprs.len());
                for expr in arg_exprs {
                    args.push(self.eval_expr(expr)?);
                }
                if let Some(RuntimeVal::Closure {
                    params,
                    body,
                    captured,
                }) = self.lookup(name)
                {
                    return self.call_closure(&params, &body, &captured, &args);
                }
                self.call_builtin(name, args)
            }
            Expr::MethodCall(recv_expr, method, arg_exprs) => {
                let recv = self.eval_expr(recv_expr)?;
                let mut args = Vec::with_capacity(arg_exprs.len());
                for expr in arg_exprs {
                    args.push(self.eval_expr(expr)?);
                }
                let result = self.call_method(recv, method, &args)?;
                if method == "push"
                    && let Expr::Ident(name) = recv_expr.as_ref()
                {
                    let _ = self.assign_existing(name, result.clone());
                }
                Ok(result)
            }
        }
    }

    fn eval_binary(&mut self, op: BinOp, lhs: &Expr, rhs: &Expr) -> Result<RuntimeVal, Abort> {
        if matches!(op, BinOp::And) {
            let l = self.eval_expr(lhs)?;
            return if !l.is_truthy() {
                Ok(RuntimeVal::Json(Value::Bool(false)))
            } else {
                Ok(RuntimeVal::Json(Value::Bool(self.eval_expr(rhs)?.is_truthy())))
            };
        }
        if matches!(op, BinOp::Or) {
            let l = self.eval_expr(lhs)?;
            return if l.is_truthy() {
                Ok(RuntimeVal::Json(Value::Bool(true)))
            } else {
                Ok(RuntimeVal::Json(Value::Bool(self.eval_expr(rhs)?.is_truthy())))
            };
        }
        let l = self.eval_expr(lhs)?.into_json().map_err(Abort::Runtime)?;
        let r = self.eval_expr(rhs)?.into_json().map_err(Abort::Runtime)?;
        let out = match op {
            BinOp::Eq => Value::Bool(l == r),
            BinOp::Ne => Value::Bool(l != r),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => match (&l, &r) {
                (Value::Int(a), Value::Int(b)) => Value::Bool(match op {
                    BinOp::Lt => a < b,
                    BinOp::Le => a <= b,
                    BinOp::Gt => a > b,
                    BinOp::Ge => a >= b,
                    _ => unreachable!(),
                }),
                (Value::String(a), Value::String(b)) => Value::Bool(match op {
                    BinOp::Lt => a < b,
                    BinOp::Le => a <= b,
                    BinOp::Gt => a > b,
                    BinOp::Ge => a >= b,
                    _ => unreachable!(),
                }),
                _ => {
                    return Err(Abort::Runtime(String::from(
                        "comparison requires two ints or two strings",
                    )));
                }
            },
            BinOp::Add => match (l, r) {
                (Value::Int(a), Value::Int(b)) => a
                    .checked_add(b)
                    .map(Value::Int)
                    .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))?,
                (Value::String(mut a), Value::String(b)) => {
                    a.push_str(&b);
                    Value::String(a)
                }
                (Value::Array(mut a), Value::Array(b)) => {
                    a.extend(b);
                    Value::Array(a)
                }
                _ => {
                    return Err(Abort::Runtime(String::from(
                        "'+' expects two ints, two strings, or two arrays",
                    )));
                }
            },
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                let (Value::Int(a), Value::Int(b)) = (l, r) else {
                    return Err(Abort::Runtime(String::from("arithmetic expects integers")));
                };
                match op {
                    BinOp::Sub => a
                        .checked_sub(b)
                        .map(Value::Int)
                        .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))?,
                    BinOp::Mul => a
                        .checked_mul(b)
                        .map(Value::Int)
                        .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))?,
                    BinOp::Div => {
                        if b == 0 {
                            return Err(Abort::Runtime(String::from("division by zero")));
                        }
                        a.checked_div(b)
                            .map(Value::Int)
                            .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))?
                    }
                    BinOp::Mod => {
                        if b == 0 {
                            return Err(Abort::Runtime(String::from("modulo by zero")));
                        }
                        a.checked_rem(b)
                            .map(Value::Int)
                            .ok_or_else(|| Abort::Runtime(String::from("integer overflow")))?
                    }
                    _ => unreachable!(),
                }
            }
            BinOp::And | BinOp::Or => unreachable!(),
        };
        Ok(RuntimeVal::Json(out))
    }

    fn call_closure(
        &mut self,
        params: &[String],
        body: &LambdaBody,
        captured: &[ScopeFrame],
        args: &[RuntimeVal],
    ) -> Result<RuntimeVal, Abort> {
        if self.depth >= MAX_EVAL_DEPTH {
            return Err(Abort::Runtime(String::from("maximum call depth exceeded")));
        }
        let mut frame = BTreeMap::new();
        for (idx, param) in params.iter().enumerate() {
            frame.insert(
                param.clone(),
                args.get(idx)
                    .cloned()
                    .unwrap_or(RuntimeVal::Json(Value::Null)),
            );
        }
        let saved = std::mem::replace(&mut self.scopes, captured.to_vec());
        self.scopes.push(frame);
        self.depth += 1;

        let res = match body {
            LambdaBody::Expr(expr) => self.eval_expr(expr),
            LambdaBody::Block(stmts) => match self.eval_block(stmts)? {
                Flow::None(v) | Flow::Return(v) => Ok(v),
                Flow::Break => Err(Abort::Runtime(String::from("break used outside a loop"))),
                Flow::Continue => Err(Abort::Runtime(String::from("continue used outside a loop"))),
            },
        };

        self.depth -= 1;
        self.scopes = saved;
        res
    }

    fn call_method(
        &mut self,
        recv: RuntimeVal,
        method: &str,
        args: &[RuntimeVal],
    ) -> Result<RuntimeVal, Abort> {
        let json = recv.as_json()?;
        match json {
            Value::String(s) => self.call_string_method(s, method, args),
            Value::Array(arr) => self.call_array_method(arr, method, args),
            Value::Object(obj) => call_object_method(obj, method, args).map(RuntimeVal::Json),
            other => Err(Abort::Runtime(format!(
                "method .{method}() is not supported on {}",
                type_name(other)
            ))),
        }
    }

    fn call_string_method(
        &mut self,
        s: &str,
        method: &str,
        args: &[RuntimeVal],
    ) -> Result<RuntimeVal, Abort> {
        let out = match method {
            "trim" => Value::String(s.trim().to_string()),
            "trim_start" => Value::String(s.trim_start().to_string()),
            "trim_end" => Value::String(s.trim_end().to_string()),
            "to_lower" => Value::String(s.to_lowercase()),
            "to_upper" => Value::String(s.to_uppercase()),
            "strip_ansi" => Value::String(projection::strip_ansi(s)),
            "len" => Value::Int(s.len() as i64),
            "lines" => Value::Array(
                s.split_terminator('\n')
                    .map(|l| Value::String(l.to_string()))
                    .collect(),
            ),
            "contains" => Value::Bool(s.contains(expect_str_arg(args, 0, "contains")?.as_str())),
            "starts_with" => {
                Value::Bool(s.starts_with(expect_str_arg(args, 0, "starts_with")?.as_str()))
            }
            "ends_with" => {
                Value::Bool(s.ends_with(expect_str_arg(args, 0, "ends_with")?.as_str()))
            }
            "split" => {
                let sep = expect_str_arg(args, 0, "split")?;
                Value::Array(
                    s.split(&sep)
                        .map(|part| Value::String(part.to_string()))
                        .collect(),
                )
            }
            "replace" => {
                let from = expect_str_arg(args, 0, "replace")?;
                let to = expect_str_arg(args, 1, "replace")?;
                Value::String(s.replace(&from, &to))
            }
            "grep" => {
                let pat = expect_str_arg(args, 0, "grep")?;
                let ctx = args
                    .get(1)
                    .map(|v| expect_nonneg_usize(v, "grep context"))
                    .transpose()?
                    .unwrap_or(0);
                let (lines, ended_with_nl) = projection::split_lines_lossless(s);
                let (matched, _) =
                    projection::filter_lines_with_grep(&lines, &pat, ctx).map_err(Abort::Runtime)?;
                if matched.is_empty() {
                    Value::String(String::new())
                } else {
                    let mut out = matched.join("\n");
                    if ended_with_nl {
                        out.push('\n');
                    }
                    Value::String(out)
                }
            }
            "head" => {
                let n = expect_nonneg_usize(expect_arg(args, 0, "head")?, "head")?;
                let (lines, ended_with_nl) = projection::split_lines_lossless(s);
                let take = &lines[..n.min(lines.len())];
                let mut out = take.join("\n");
                if !take.is_empty() && (take.len() < lines.len() || ended_with_nl) {
                    out.push('\n');
                }
                Value::String(out)
            }
            "tail" => {
                let n = expect_nonneg_usize(expect_arg(args, 0, "tail")?, "tail")?;
                let (lines, ended_with_nl) = projection::split_lines_lossless(s);
                let start = lines.len().saturating_sub(n);
                let take = &lines[start..];
                let mut out = take.join("\n");
                if !take.is_empty() && ended_with_nl {
                    out.push('\n');
                }
                Value::String(out)
            }
            "slice" => {
                let chars: Vec<char> = s.chars().collect();
                let (start, end) = resolve_slice(chars.len(), args)?;
                Value::String(chars[start..end].iter().collect())
            }
            "parse_json" => parse_line(s.as_bytes())
                .map_err(|e| Abort::Runtime(format!("parse_json failed: {e}")))?,
            other => {
                return Err(Abort::Runtime(format!("unknown string method '.{other}'")));
            }
        };
        Ok(RuntimeVal::Json(out))
    }

    fn call_array_method(
        &mut self,
        arr: &[Value],
        method: &str,
        args: &[RuntimeVal],
    ) -> Result<RuntimeVal, Abort> {
        let out = match method {
            "len" => Value::Int(arr.len() as i64),
            "first" => arr.first().cloned().unwrap_or(Value::Null),
            "last" => arr.last().cloned().unwrap_or(Value::Null),
            "slice" => {
                let (start, end) = resolve_slice(arr.len(), args)?;
                Value::Array(arr[start..end].to_vec())
            }
            "join" => {
                let sep = expect_str_arg(args, 0, "join")?;
                let mut parts = Vec::with_capacity(arr.len());
                for item in arr {
                    parts.push(value_to_string(item));
                }
                Value::String(parts.join(&sep))
            }
            "push" => {
                let val = expect_arg(args, 0, "push")?.clone().into_json().map_err(Abort::Runtime)?;
                let mut next = arr.to_vec();
                next.push(val);
                Value::Array(next)
            }
            "flat" => {
                let mut out = Vec::new();
                for item in arr {
                    if let Value::Array(inner) = item {
                        out.extend(inner.iter().cloned());
                    } else {
                        out.push(item.clone());
                    }
                }
                Value::Array(out)
            }
            "sort" => {
                let mut sorted = arr.to_vec();
                sorted.sort_by(compare_json_values);
                Value::Array(sorted)
            }
            "sort_by" => {
                if args.len() != 1 {
                    return Err(Abort::Runtime(String::from(
                        "sort_by() expects exactly 1 argument",
                    )));
                }
                let closure = expect_closure_at(args, 0, "sort_by")?;
                let mut keyed = Vec::with_capacity(arr.len());
                for (idx, item) in arr.iter().enumerate() {
                    self.tick()?;
                    let key = self.call_closure(
                        closure.params,
                        closure.body,
                        closure.captured,
                        &[
                            RuntimeVal::Json(item.clone()),
                            RuntimeVal::Json(Value::Int(idx as i64)),
                        ],
                    )?;
                    let key = key.into_json().map_err(Abort::Runtime)?;
                    keyed.push((key, item.clone()));
                }
                let key_kind = |key: &Value| match key {
                    Value::Int(_) => Some(true),
                    Value::String(_) => Some(false),
                    _ => None,
                };
                let expected_kind = match keyed.first() {
                    None => None,
                    Some((key, _)) => match key_kind(key) {
                        Some(kind) => Some(kind),
                        None => {
                            return Err(Abort::Runtime(String::from(
                                "sort_by() closure must return an int or string",
                            )))
                        }
                    },
                };
                if let Some(kind) = expected_kind {
                    if !keyed
                        .iter()
                        .all(|(key, _)| key_kind(key) == Some(kind))
                    {
                        return Err(Abort::Runtime(String::from(
                            "sort_by keys must all be numbers or all strings",
                        )));
                    }
                }
                keyed.sort_by(|a, b| compare_json_values(&a.0, &b.0));
                Value::Array(keyed.into_iter().map(|(_, item)| item).collect())
            }
            "group_by" => {
                if args.len() != 1 {
                    return Err(Abort::Runtime(String::from(
                        "group_by() expects exactly 1 argument",
                    )));
                }
                let closure = expect_closure_at(args, 0, "group_by")?;
                let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
                for (idx, item) in arr.iter().enumerate() {
                    self.tick()?;
                    let key = self.call_closure(
                        closure.params,
                        closure.body,
                        closure.captured,
                        &[
                            RuntimeVal::Json(item.clone()),
                            RuntimeVal::Json(Value::Int(idx as i64)),
                        ],
                    )?;
                    let key = value_to_string(key.as_json()?);
                    if let Some((_, bucket)) = groups.iter_mut().find(|(k, _)| k == &key) {
                        bucket.push(item.clone());
                    } else {
                        groups.push((key, vec![item.clone()]));
                    }
                }
                Value::Object(
                    groups
                        .into_iter()
                        .map(|(key, bucket)| (key, Value::Array(bucket)))
                        .collect(),
                )
            }
            "reduce" => {
                if args.len() != 2 {
                    return Err(Abort::Runtime(String::from(
                        "reduce() expects exactly 2 arguments",
                    )));
                }
                let mut acc = args[0].clone().into_json().map_err(Abort::Runtime)?;
                if !matches!(acc, Value::Int(_) | Value::String(_)) {
                    return Err(Abort::Runtime(String::from(
                        "reduce() expects an int or string accumulator",
                    )));
                }
                let closure = expect_closure_at(args, 1, "reduce")?;
                for (idx, item) in arr.iter().enumerate() {
                    self.tick()?;
                    let next = self.call_closure(
                        closure.params,
                        closure.body,
                        closure.captured,
                        &[
                            RuntimeVal::Json(acc),
                            RuntimeVal::Json(item.clone()),
                            RuntimeVal::Json(Value::Int(idx as i64)),
                        ],
                    )?;
                    acc = next.into_json().map_err(Abort::Runtime)?;
                    if !matches!(acc, Value::Int(_) | Value::String(_)) {
                        return Err(Abort::Runtime(String::from(
                            "reduce() expects an int or string accumulator",
                        )));
                    }
                }
                acc
            }
            "unique" => {
                let mut out = Vec::new();
                for item in arr {
                    if !out.contains(item) {
                        out.push(item.clone());
                    }
                }
                Value::Array(out)
            }
            "map" | "filter" | "any" | "all" | "find" => {
                let RuntimeVal::Closure {
                    params,
                    body,
                    captured,
                } = expect_arg(args, 0, method)?
                else {
                    return Err(Abort::Runtime(format!(
                        ".{method}() expects a function argument"
                    )));
                };
                match method {
                    "map" => {
                        let mut out = Vec::with_capacity(arr.len());
                        for (idx, item) in arr.iter().enumerate() {
                            self.tick()?;
                            let val = self.call_closure(
                                params,
                                body,
                                captured,
                                &[
                                    RuntimeVal::Json(item.clone()),
                                    RuntimeVal::Json(Value::Int(idx as i64)),
                                ],
                            )?;
                            out.push(val.into_json().map_err(Abort::Runtime)?);
                        }
                        Value::Array(out)
                    }
                    "filter" => {
                        let mut out = Vec::new();
                        for (idx, item) in arr.iter().enumerate() {
                            self.tick()?;
                            let keep = self.call_closure(
                                params,
                                body,
                                captured,
                                &[
                                    RuntimeVal::Json(item.clone()),
                                    RuntimeVal::Json(Value::Int(idx as i64)),
                                ],
                            )?;
                            if keep.is_truthy() {
                                out.push(item.clone());
                            }
                        }
                        Value::Array(out)
                    }
                    "any" => {
                        let mut found = false;
                        for (idx, item) in arr.iter().enumerate() {
                            self.tick()?;
                            if self
                                .call_closure(
                                    params,
                                    body,
                                    captured,
                                    &[
                                        RuntimeVal::Json(item.clone()),
                                        RuntimeVal::Json(Value::Int(idx as i64)),
                                    ],
                                )?
                                .is_truthy()
                            {
                                found = true;
                                break;
                            }
                        }
                        Value::Bool(found)
                    }
                    "all" => {
                        let mut ok = true;
                        for (idx, item) in arr.iter().enumerate() {
                            self.tick()?;
                            if !self
                                .call_closure(
                                    params,
                                    body,
                                    captured,
                                    &[
                                        RuntimeVal::Json(item.clone()),
                                        RuntimeVal::Json(Value::Int(idx as i64)),
                                    ],
                                )?
                                .is_truthy()
                            {
                                ok = false;
                                break;
                            }
                        }
                        Value::Bool(ok)
                    }
                    "find" => {
                        let mut hit = Value::Null;
                        for (idx, item) in arr.iter().enumerate() {
                            self.tick()?;
                            if self
                                .call_closure(
                                    params,
                                    body,
                                    captured,
                                    &[
                                        RuntimeVal::Json(item.clone()),
                                        RuntimeVal::Json(Value::Int(idx as i64)),
                                    ],
                                )?
                                .is_truthy()
                            {
                                hit = item.clone();
                                break;
                            }
                        }
                        hit
                    }
                    _ => unreachable!(),
                }
            }
            other => {
                return Err(Abort::Runtime(format!("unknown array method '.{other}'")));
            }
        };
        Ok(RuntimeVal::Json(out))
    }

    // ---- Builtins ---------------------------------------------------------

    fn call_builtin(&mut self, name: &str, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        match name {
            "len" => {
                let v = expect_arg(&args, 0, "len")?.as_json()?;
                let n = match v {
                    Value::String(s) => s.len(),
                    Value::Array(a) => a.len(),
                    Value::Object(o) => o.len(),
                    other => {
                        return Err(Abort::Runtime(format!(
                            "len() does not accept {}",
                            type_name(other)
                        )));
                    }
                };
                Ok(RuntimeVal::Json(Value::Int(n as i64)))
            }
            "str" => Ok(RuntimeVal::Json(Value::String(value_to_string(
                expect_arg(&args, 0, "str")?.as_json()?,
            )))),
            "int" => {
                let v = expect_arg(&args, 0, "int")?.as_json()?;
                let n = match v {
                    Value::Int(n) => *n,
                    Value::Bool(b) => i64::from(*b),
                    Value::String(s) => s
                        .trim()
                        .parse::<i64>()
                        .map_err(|_| Abort::Runtime(format!("cannot parse int({s:?})")))?,
                    _ => return Err(Abort::Runtime(String::from("int() expects int or string"))),
                };
                Ok(RuntimeVal::Json(Value::Int(n)))
            }
            "json_parse" => {
                let s = expect_str_arg(&args, 0, "json_parse")?;
                let v = parse_line(s.as_bytes())
                    .map_err(|e| Abort::Runtime(format!("json_parse failed: {e}")))?;
                Ok(RuntimeVal::Json(v))
            }
            "json_encode" => {
                let v = expect_arg(&args, 0, "json_encode")?
                    .clone()
                    .into_json()
                    .map_err(Abort::Runtime)?;
                Ok(RuntimeVal::Json(Value::String(v.encode())))
            }
            "range" => {
                let (start, end) = if args.len() == 1 {
                    (0, expect_arg(&args, 0, "range")?.as_json()?.as_i64().ok_or_else(|| {
                        Abort::Runtime(String::from("range() expects integer arguments"))
                    })?)
                } else {
                    (
                        expect_arg(&args, 0, "range")?.as_json()?.as_i64().ok_or_else(|| {
                            Abort::Runtime(String::from("range() expects integer arguments"))
                        })?,
                        expect_arg(&args, 1, "range")?.as_json()?.as_i64().ok_or_else(|| {
                            Abort::Runtime(String::from("range() expects integer arguments"))
                        })?,
                    )
                };
                let count = (end - start).max(0);
                if count > 100_000 {
                    return Err(Abort::Runtime(String::from("range() exceeds 100000 items")));
                }
                Ok(RuntimeVal::Json(Value::Array(
                    (start..end).map(Value::Int).collect(),
                )))
            }
            "sh" => self.builtin_exec_command(args, false),
            "run" => self.builtin_exec_command(args, true),
            "parallel" => self.builtin_parallel(args),
            "map_parallel" => self.builtin_map_parallel(args),
            "workflow" => self.builtin_workflow(args),
            "state" => self.builtin_state(args),
            "read_capture" => self.builtin_read_capture(args),
            other => Err(Abort::Runtime(format!("unknown function '{other}'"))),
        }
    }

    fn charge_call(&mut self, count: usize) -> Result<(), Abort> {
        if self.audit.calls + count > self.request.max_calls {
            return Err(Abort::Runtime(format!(
                "code exceeded max_calls limit ({})",
                self.request.max_calls
            )));
        }
        self.audit.calls += count;
        Ok(())
    }

    fn record_outcome(&mut self, status: i64, termination: Termination, capture_id: Option<&str>) {
        if matches!(termination, Termination::Cancelled) {
            self.audit.cancelled_calls += 1;
        }
        if status != 0 || matches!(termination, Termination::SpawnError) {
            self.audit.failed_calls += 1;
            if self.audit.first_failure_status.is_none() {
                self.audit.first_failure_status = Some(status);
            }
        }
        if let Some(cid) = capture_id
            && self.seen_captures.insert(cid.to_string())
        {
            self.audit.captures.push(cid.to_string());
        }
    }

    fn merge_timeout(&self, requested: Option<i64>) -> Result<Option<i64>, Abort> {
        let outer = self.remaining_timeout_ms()?;
        Ok(match (requested, outer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        })
    }

    fn builtin_exec_command(
        &mut self,
        args: Vec<RuntimeVal>,
        is_argv: bool,
    ) -> Result<RuntimeVal, Abort> {
        self.poll_supervision()?;
        self.charge_call(1)?;

        let target_val = expect_arg(&args, 0, if is_argv { "run" } else { "sh" })?
            .clone()
            .into_json()
            .map_err(Abort::Runtime)?;
        let mut entries = if let Some(opts) = args.get(1) {
            opts.as_json()?
                .as_object()
                .ok_or_else(|| Abort::Runtime(String::from("options argument must be an object")))?
                .to_vec()
        } else {
            Vec::new()
        };

        entries.retain(|(k, _)| k != "argv" && k != "command");
        if is_argv {
            entries.push((String::from("argv"), target_val));
        } else {
            entries.push((String::from("command"), target_val));
        }
        let has_isolation_overrides = entries.iter().any(|(k, _)| k == "cwd" || k == "env");
        if has_isolation_overrides && !entries.iter().any(|(k, _)| k == "isolated") {
            entries.push((String::from("isolated"), Value::Bool(true)));
        }

        let limits = self.host.limits();
        let mut decoded = protocol::decode_exec(
            &Value::Object(entries),
            &limits,
            self.request.id.clone(),
        )
        .map_err(|err| Abort::Runtime(err.message))?;
        decoded.timeout_ms = self.merge_timeout(decoded.timeout_ms)?;

        let result = self
            .host
            .call_exec(&decoded)
            .map_err(Abort::Runtime)?;
        self.restore_outer_supervision()?;

        self.record_outcome(
            result.status,
            result.termination,
            result.capture_id.as_deref(),
        );
        if matches!(result.termination, Termination::Cancelled) {
            return Err(Abort::Cancelled);
        }
        if matches!(result.termination, Termination::Timeout)
            && self.deadline.is_some_and(|d| Instant::now() >= d)
        {
            return Err(Abort::Timeout);
        }

        response_to_runtime(Response::Result(result))
    }

    fn normalize_task_value(item: Value, idx: usize) -> Result<Value, Abort> {
        let mut entries = match item {
            Value::String(cmd) => vec![(String::from("command"), Value::String(cmd))],
            Value::Array(argv) => vec![(String::from("argv"), Value::Array(argv))],
            Value::Object(obj) => obj,
            _ => {
                return Err(Abort::Runtime(String::from(
                    "task spec must be an object, command string, or argv array",
                )));
            }
        };
        if !entries.iter().any(|(k, _)| k == "id") {
            entries.insert(0, (String::from("id"), Value::String(format!("t{idx}"))));
        }
        Ok(Value::Object(entries))
    }

    fn builtin_parallel(&mut self, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        self.poll_supervision()?;
        let raw_tasks = expect_arg(&args, 0, "parallel")?
            .as_json()?
            .as_array()
            .ok_or_else(|| Abort::Runtime(String::from("parallel() expects a tasks array")))?
            .to_vec();

        let normalized: Vec<Value> = raw_tasks
            .into_iter()
            .enumerate()
            .map(|(i, t)| Self::normalize_task_value(t, i))
            .collect::<Result<Vec<_>, _>>()?;

        let mut obj = if let Some(opts) = args.get(1) {
            opts.as_json()?
                .as_object()
                .ok_or_else(|| Abort::Runtime(String::from("parallel() options must be an object")))?
                .to_vec()
        } else {
            Vec::new()
        };
        obj.retain(|(k, _)| k != "tasks");
        obj.push((String::from("tasks"), Value::Array(normalized)));

        let limits = self.host.limits();
        let mut decoded = protocol::decode_parallel(
            &Value::Object(obj),
            &limits,
            self.request.id.clone(),
        )
        .map_err(|err| Abort::Runtime(err.message))?;
        decoded.timeout_ms = self.merge_timeout(decoded.timeout_ms)?;
        self.charge_call(decoded.tasks.len())?;

        let result = self
            .host
            .call_parallel(&decoded)
            .map_err(Abort::Runtime)?;
        self.restore_outer_supervision()?;

        for task in &result.tasks {
            self.record_outcome(task.status, task.termination, task.capture_id.as_deref());
        }
        if matches!(result.state, GroupState::Cancelled) {
            return Err(Abort::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Abort::Timeout);
        }

        response_to_runtime(Response::Parallel(result))
    }

    fn builtin_map_parallel(&mut self, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        self.poll_supervision()?;
        let items = expect_arg(&args, 0, "map_parallel")?
            .as_json()?
            .as_array()
            .ok_or_else(|| Abort::Runtime(String::from("map_parallel() expects an items array")))?
            .to_vec();
        if items.is_empty() {
            return Ok(RuntimeVal::Json(Value::Array(Vec::new())));
        }
        let concurrency = expect_nonneg_usize(
            expect_arg(&args, 1, "map_parallel")?,
            "map_parallel max_concurrency",
        )?;
        let RuntimeVal::Closure {
            params,
            body,
            captured,
        } = expect_arg(&args, 2, "map_parallel")?
        else {
            return Err(Abort::Runtime(String::from(
                "map_parallel() expects a mapper closure as 3rd argument",
            )));
        };

        let mut tasks = Vec::with_capacity(items.len());
        for (idx, item) in items.into_iter().enumerate() {
            self.tick()?;
            let produced = self
                .call_closure(
                    params,
                    body,
                    captured,
                    &[
                        RuntimeVal::Json(item),
                        RuntimeVal::Json(Value::Int(idx as i64)),
                    ],
                )?
                .into_json()
                .map_err(Abort::Runtime)?;
            tasks.push(Self::normalize_task_value(produced, idx)?);
        }

        let spec = Value::Object(vec![
            (
                String::from("max_concurrency"),
                Value::Int(concurrency as i64),
            ),
            (String::from("tasks"), Value::Array(tasks)),
        ]);
        let limits = self.host.limits();
        let mut decoded =
            protocol::decode_parallel(&spec, &limits, self.request.id.clone())
                .map_err(|err| Abort::Runtime(err.message))?;
        decoded.timeout_ms = self.merge_timeout(decoded.timeout_ms)?;
        self.charge_call(decoded.tasks.len())?;

        let result = self
            .host
            .call_parallel(&decoded)
            .map_err(Abort::Runtime)?;
        self.restore_outer_supervision()?;

        for task in &result.tasks {
            self.record_outcome(task.status, task.termination, task.capture_id.as_deref());
        }
        if matches!(result.state, GroupState::Cancelled) {
            return Err(Abort::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Abort::Timeout);
        }

        let val = response_to_runtime(Response::Parallel(result))?;
        Ok(RuntimeVal::Json(
            val.as_json()?
                .get("tasks")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
        ))
    }

    fn builtin_workflow(&mut self, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        self.poll_supervision()?;
        let spec = expect_arg(&args, 0, "workflow")?
            .clone()
            .into_json()
            .map_err(Abort::Runtime)?;
        let limits = self.host.limits();
        let mut decoded = protocol::decode_workflow(&spec, &limits, self.request.id.clone())
            .map_err(|err| Abort::Runtime(err.message))?;
        decoded.timeout_ms = self.merge_timeout(decoded.timeout_ms)?;
        self.charge_call(decoded.tasks.len())?;

        let result = self
            .host
            .call_workflow(&decoded)
            .map_err(Abort::Runtime)?;
        self.restore_outer_supervision()?;

        for task in &result.tasks {
            if matches!(
                task.base.state,
                super::json::TaskState::Completed
                    | super::json::TaskState::Timeout
                    | super::json::TaskState::Cancelled
                    | super::json::TaskState::SpawnError
            ) {
                self.record_outcome(
                    task.base.status,
                    task.base.termination,
                    task.base.capture_id.as_deref(),
                );
            }
        }
        if matches!(result.state, GroupState::Cancelled) {
            return Err(Abort::Cancelled);
        }
        if self.deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(Abort::Timeout);
        }

        response_to_runtime(Response::Workflow(result))
    }

    fn builtin_state(&mut self, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        self.tick()?;
        let spec = match args.first() {
            None => Value::Object(Vec::new()),
            Some(v) => match v.as_json()? {
                Value::Array(arr) => {
                    Value::Object(vec![(String::from("include"), Value::Array(arr.clone()))])
                }
                Value::Object(obj) => Value::Object(obj.clone()),
                _ => {
                    return Err(Abort::Runtime(String::from(
                        "state() expects an include array or options object",
                    )));
                }
            },
        };
        let decoded = protocol::decode_state(&spec, self.request.id.clone())
            .map_err(|err| Abort::Runtime(err.message))?;
        let state = self.host.call_state(&decoded);
        response_to_runtime(Response::State(state))
    }

    fn builtin_read_capture(&mut self, args: Vec<RuntimeVal>) -> Result<RuntimeVal, Abort> {
        self.tick()?;
        let capture_id = expect_str_arg(&args, 0, "read_capture")?;
        let mut entries = if let Some(opts) = args.get(1) {
            opts.as_json()?
                .as_object()
                .ok_or_else(|| Abort::Runtime(String::from("read_capture() options must be an object")))?
                .to_vec()
        } else {
            Vec::new()
        };
        entries.retain(|(k, _)| k != "capture_id");
        entries.push((String::from("capture_id"), Value::String(capture_id)));

        let limits = self.host.limits();
        let decoded = protocol::decode_read_capture(
            &Value::Object(entries),
            &limits,
            self.request.id.clone(),
        )
        .map_err(|err| Abort::Runtime(err.message))?;
        let res = self
            .host
            .call_read_capture(&decoded)
            .map_err(|(_, msg)| Abort::Runtime(msg))?;
        response_to_runtime(Response::Capture(res))
    }
}

fn response_to_runtime(response: Response) -> Result<RuntimeVal, Abort> {
    parse_line(response.encode().as_bytes())
        .map(RuntimeVal::Json)
        .map_err(|err| Abort::Runtime(format!("internal response decode error: {err}")))
}

fn call_object_method(
    obj: &[(String, Value)],
    method: &str,
    args: &[RuntimeVal],
) -> Result<Value, Abort> {
    match method {
        "len" => Ok(Value::Int(obj.len() as i64)),
        "keys" => Ok(Value::Array(
            obj.iter().map(|(k, _)| Value::String(k.clone())).collect(),
        )),
        "values" => Ok(Value::Array(obj.iter().map(|(_, v)| v.clone()).collect())),
        "entries" => Ok(Value::Array(
            obj.iter()
                .map(|(k, v)| Value::Array(vec![Value::String(k.clone()), v.clone()]))
                .collect(),
        )),
        "has" => {
            let key = expect_str_arg(args, 0, "has")?;
            Ok(Value::Bool(obj.iter().any(|(k, _)| k == &key)))
        }
        "get" => {
            let key = expect_str_arg(args, 0, "get")?;
            let default = match args.get(1) {
                Some(v) => v.clone().into_json().map_err(Abort::Runtime)?,
                None => Value::Null,
            };
            Ok(obj
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, v)| v.clone())
                .unwrap_or(default))
        }
        other => Err(Abort::Runtime(format!("unknown object method '.{other}'"))),
    }
}

struct ClosureRef<'a> {
    params: &'a [String],
    body: &'a LambdaBody,
    captured: &'a [ScopeFrame],
}

fn expect_closure_at<'a>(
    args: &'a [RuntimeVal],
    index: usize,
    method: &str,
) -> Result<ClosureRef<'a>, Abort> {
    match args.get(index) {
        Some(RuntimeVal::Closure {
            params,
            body,
            captured,
        }) => Ok(ClosureRef {
            params,
            body,
            captured,
        }),
        _ => Err(Abort::Runtime(format!(
            "{method}() expects a function argument"
        ))),
    }
}

fn expect_arg<'a>(args: &'a [RuntimeVal], idx: usize, name: &str) -> Result<&'a RuntimeVal, Abort> {
    args.get(idx)
        .ok_or_else(|| Abort::Runtime(format!("{name}() missing argument #{}", idx + 1)))
}

fn expect_str_arg(args: &[RuntimeVal], idx: usize, name: &str) -> Result<String, Abort> {
    let val = expect_arg(args, idx, name)?.as_json()?;
    val.as_str()
        .map(str::to_string)
        .ok_or_else(|| Abort::Runtime(format!("{name}() argument #{} must be a string", idx + 1)))
}

fn expect_nonneg_usize(val: &RuntimeVal, label: &str) -> Result<usize, Abort> {
    let n = val
        .as_json()?
        .as_i64()
        .ok_or_else(|| Abort::Runtime(format!("{label} must be an integer")))?;
    usize::try_from(n).map_err(|_| Abort::Runtime(format!("{label} must not be negative")))
}

fn resolve_slice(len: usize, args: &[RuntimeVal]) -> Result<(usize, usize), Abort> {
    let start = expect_nonneg_usize(expect_arg(args, 0, "slice")?, "slice start")?.min(len);
    let end = match args.get(1) {
        Some(v) => expect_nonneg_usize(v, "slice end")?.min(len),
        None => len,
    };
    Ok((start.min(end), end))
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::from("null"),
        Value::Bool(b) => b.to_string(),
        Value::Int(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(_) | Value::Object(_) => v.encode(),
    }
}

fn compare_json_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        _ => value_to_string(a).cmp(&value_to_string(b)),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::json::Captured;

    struct MockHost {
        fail_status: i64,
    }

    impl CodeHost for MockHost {
        fn limits(&self) -> Limits {
            Limits::default()
        }
        fn call_exec(&mut self, req: &protocol::ExecRequest) -> Result<ExecResult, String> {
            let mut r = ExecResult::new(req.id.clone(), self.fail_status, 0, "/tmp");
            if let Some(cmd) = req.command() {
                r.stdout = Captured {
                    data: super::super::json::StreamData::Utf8(cmd.to_string()),
                    bytes: cmd.len(),
                    truncated: false,
                };
            }
            if self.fail_status != 0 {
                r.capture_id = Some(String::from("cap-mock"));
            }
            Ok(r)
        }
        fn call_parallel(
            &mut self,
            req: &protocol::ParallelRequest,
        ) -> Result<ParallelResult, String> {
            Ok(ParallelResult::new(req.id.clone(), req.policy))
        }
        fn call_workflow(
            &mut self,
            req: &protocol::WorkflowRequest,
        ) -> Result<WorkflowResult, String> {
            Ok(WorkflowResult::new(req.id.clone(), req.policy, req.failure_policy))
        }
        fn call_state(&mut self, req: &protocol::StateRequest) -> StateResult {
            StateResult {
                id: req.id.clone(),
                cwd: String::from("/tmp"),
                last_status: 0,
                pid: 1,
                jobs: Vec::new(),
                options: Default::default(),
                sections: Vec::new(),
            }
        }
        fn call_read_capture(
            &self,
            req: &protocol::ReadCaptureRequest,
        ) -> Result<CaptureResult, (ErrorCode, String)> {
            Err((ErrorCode::CaptureNotFound, format!("missing {}", req.capture_id)))
        }
    }

    #[test]
    fn pure_expressions_closures_and_methods_evaluate_cleanly() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t1"),
            source: String::from(
                r#"
                let items = range(1, 5);
                let doubled = items.map((x, i) => x * 2 + i);
                let sum = 0;
                for n in doubled { sum = sum + n; }
                let [a, b] = doubled;
                { sum, a, b, has_a: args.flag }
            "#),
            args: Value::Object(vec![(String::from("flag"), Value::Bool(true))]),
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0, "{res:?}");
        assert_eq!(res.value.get("sum"), Some(&Value::Int(26)));
        assert_eq!(res.value.get("a"), Some(&Value::Int(2)));
        assert_eq!(res.value.get("b"), Some(&Value::Int(5)));
        assert_eq!(res.value.get("has_a"), Some(&Value::Bool(true)));
    }

    #[test]
    fn audit_records_failed_calls_and_step_limit_stops_loops() {
        let mut host = MockHost { fail_status: 7 };
        let req = CodeRequest {
            id: String::from("t2"),
            source: String::from(r#"let r = sh("echo hi"); return { ok: true, got: r.status };"#),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0);
        assert_eq!(res.audit.calls, 1);
        assert_eq!(res.audit.failed_calls, 1);
        assert_eq!(res.audit.first_failure_status, Some(7));
        assert_eq!(res.audit.captures, vec!["cap-mock"]);

        let mut host = MockHost { fail_status: 0 };
        let inf = CodeRequest {
            id: String::from("t3"),
            source: String::from("let i = 0; while true { i = i + 1; }"),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 50,
            max_calls: 10,
        };
        let inf_res = execute(&mut host, &inf);
        assert_eq!(inf_res.status, 1);
        assert!(inf_res.error.as_deref().unwrap_or("").contains("max_steps"));
    }

    #[test]
    fn array_reduce_sort_by_and_group_by_transform_closure_results() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t-aggregate"),
            source: String::from(
                r#"
                let rows = [{ name: "c", score: 3 }, { name: "a", score: 1 }, { name: "b", score: 3 }];
                let labels = rows.map(r => r.name + str(r.score));
                let total = labels.reduce(0, (acc, label) => acc + label.len());
                let by_score = rows.group_by(r => r.score);
                let sorted = rows.sort_by(r => r.score);
                let mixed = ["bb", "a", "cc"].sort_by(s => s);
                { total: total, scores: by_score.keys(), one: by_score.get("3").len(), names: sorted.map(r => r.name), mixed: mixed }
            "#),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0, "{res:?}");
        assert_eq!(res.value.get("total"), Some(&Value::Int(6)));
        assert_eq!(
            res.value.get("scores"),
            Some(&Value::Array(vec![
                Value::String(String::from("3")),
                Value::String(String::from("1")),
            ]))
        );
        assert_eq!(res.value.get("one"), Some(&Value::Int(2)));
        assert_eq!(
            res.value.get("names"),
            Some(&Value::Array(vec![
                Value::String(String::from("a")),
                Value::String(String::from("c")),
                Value::String(String::from("b")),
            ]))
        );
        assert_eq!(
            res.value.get("mixed"),
            Some(&Value::Array(vec![
                Value::String(String::from("a")),
                Value::String(String::from("bb")),
                Value::String(String::from("cc")),
            ]))
        );
    }

    #[test]
    fn array_sort_keeps_mixed_value_ordering_behavior() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t-sort-mixed"),
            source: String::from("[2, 10, '2'].sort()"),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0, "{res:?}");
        assert_eq!(
            res.value,
            Value::Array(vec![
                Value::Int(2),
                Value::Int(10),
                Value::String(String::from("2")),
            ])
        );
    }

    #[test]
    fn array_sort_by_sorts_homogeneous_int_keys() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t-sort-by-int"),
            source: String::from("[10, 2, 1, 20].sort_by(x => x)"),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0, "{res:?}");
        assert_eq!(
            res.value,
            Value::Array(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(10),
                Value::Int(20),
            ])
        );
    }

    #[test]
    fn array_sort_by_sorts_homogeneous_string_keys() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t-sort-by-string"),
            source: String::from("['b', 'aa', 'a'].sort_by(s => s)"),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 0, "{res:?}");
        assert_eq!(
            res.value,
            Value::Array(vec![
                Value::String(String::from("a")),
                Value::String(String::from("aa")),
                Value::String(String::from("b")),
            ])
        );
    }

    #[test]
    fn array_sort_by_handles_empty_and_single_element_arrays() {
        let mut host = MockHost { fail_status: 0 };
        for (id, source, expected) in [
            ("t-sort-by-empty", "[].sort_by(x => x)", Value::Array(vec![])),
            (
                "t-sort-by-single",
                "[5].sort_by(x => x)",
                Value::Array(vec![Value::Int(5)]),
            ),
        ] {
            let req = CodeRequest {
                id: String::from(id),
                source: String::from(source),
                args: Value::Null,
                timeout_ms: None,
                max_steps: 1000,
                max_calls: 10,
            };
            let res = execute(&mut host, &req);
            assert_eq!(res.status, 0, "{id}: {res:?}");
            assert_eq!(res.value, expected, "{id}: {res:?}");
        }
    }

    #[test]
    fn array_sort_by_rejects_mixed_key_types() {
        let mut host = MockHost { fail_status: 0 };
        for (id, source) in [
            ("t-sort-by-mixed-1", "[2, 10, '2'].sort_by(x => x)"),
            ("t-sort-by-mixed-2", "[2, '10', 3].sort_by(x => x)"),
            ("t-sort-by-mixed-3", "['b', 1, 'a', 0].sort_by(x => x)"),
        ] {
            let req = CodeRequest {
                id: String::from(id),
                source: String::from(source),
                args: Value::Null,
                timeout_ms: None,
                max_steps: 1000,
                max_calls: 10,
            };
            let res = execute(&mut host, &req);
            assert_eq!(res.status, 1, "{id}: {res:?}");
            assert!(
                res.error.as_deref().is_some_and(|e| e.contains("sort_by")),
                "{id}: expected sort_by error, got {res:?}"
            );
        }
    }

    #[test]
    fn array_sort_by_rejects_null_and_bool_keys() {
        let mut host = MockHost { fail_status: 0 };
        for (id, source) in [
            ("t-sort-by-null", "[1].sort_by(x => null)"),
            ("t-sort-by-bool", "[1].sort_by(x => x == 1)"),
        ] {
            let req = CodeRequest {
                id: String::from(id),
                source: String::from(source),
                args: Value::Null,
                timeout_ms: None,
                max_steps: 1000,
                max_calls: 10,
            };
            let res = execute(&mut host, &req);
            assert_eq!(res.status, 1, "{id}: {res:?}");
            assert!(
                res.error.as_deref().is_some_and(|e| e.contains("sort_by")),
                "{id}: expected sort_by error, got {res:?}"
            );
        }
    }

    #[test]
    fn array_aggregate_methods_reject_invalid_arguments() {
        for (id, source, expected) in [
            ("e-reduce-arity", "[].reduce(0)", "reduce() expects exactly 2 arguments"),
            ("e-reduce-extra", "[].reduce(0, (a, b) => a, 1)", "reduce() expects exactly 2 arguments"),
            ("e-reduce-fn", "[].reduce(0, 5)", "reduce() expects a function"),
            ("e-reduce-closure", "[1, 2].reduce(0, (a, b) => b > 0)", "reduce() expects an int or string accumulator"),
            ("e-sort-arity", "[].sort_by()", "sort_by() expects exactly 1 argument"),
            ("e-sort-extra", "[].sort_by(x => x, 1)", "sort_by() expects exactly 1 argument"),
            ("e-sort-key", "[1].sort_by(x => null)", "sort_by() closure must return an int or string"),
            ("e-group-arity", "[].group_by()", "group_by() expects exactly 1 argument"),
            ("e-group-extra", "[].group_by(x => x, 1)", "group_by() expects exactly 1 argument"),
        ] {
            let mut host = MockHost { fail_status: 0 };
            let req = CodeRequest {
                id: String::from(id),
                source: String::from(source),
                args: Value::Null,
                timeout_ms: None,
                max_steps: 1000,
                max_calls: 10,
            };
            let res = execute(&mut host, &req);
            assert_eq!(res.status, 1, "{id}: {res:?}");
            assert!(
                res.error.as_deref().is_some_and(|e| e.contains(expected)),
                "{id}: expected {expected:?}, got {res:?}"
            );
        }
    }

    #[test]
    fn array_aggregate_methods_respect_step_and_call_budgets() {
        let mut host = MockHost { fail_status: 0 };
        let req = CodeRequest {
            id: String::from("t-budget-reduce"),
            source: String::from("range(20).reduce(0, (acc, x) => acc + x)"),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 10,
            max_calls: 10,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 1, "{res:?}");
        assert!(res.error.as_deref().is_some_and(|e| e.contains("max_steps")), "{res:?}");

        let req = CodeRequest {
            id: String::from("t-budget-calls"),
            source: String::from(
                "let r = range(4).reduce(0, (acc, x) => { run([\"true\"]); acc + x }); r",
            ),
            args: Value::Null,
            timeout_ms: None,
            max_steps: 1000,
            max_calls: 2,
        };
        let res = execute(&mut host, &req);
        assert_eq!(res.status, 1, "{res:?}");
        assert!(res.error.as_deref().is_some_and(|e| e.contains("max_calls")), "{res:?}");
        assert_eq!(res.audit.calls, 2);
    }
}
