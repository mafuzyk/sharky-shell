use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use crate::exec;
use crate::lexer::{self, Part, Token, TokenKind, Word};

pub struct Shell {
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub last_status: i32,
}

#[derive(Clone, Copy)]
enum Connector {
    Always,
    And,
    Or,
}

struct Item {
    words: Vec<Word>,
    connector: Connector,
}

enum Flow {
    Status(i32),
    Exit(i32),
}

impl Shell {
    pub fn new() -> Self {
        let env = std::env::vars_os()
            .filter_map(|(key, value)| {
                Some((
                    key.into_string().ok()?,
                    String::from_utf8(value.into_vec()).ok()?,
                ))
            })
            .collect();

        Shell {
            env,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            last_status: 0,
        }
    }

    pub fn run(&mut self, source: &str) -> i32 {
        let tokens = match lexer::tokenize(source) {
            Ok(tokens) => tokens,
            Err(err) => {
                eprintln!("sharky: {}", err.message);
                return 2;
            }
        };

        let items = match parse_items(tokens) {
            Ok(items) => items,
            Err(message) => {
                eprintln!("sharky: {message}");
                return 2;
            }
        };

        let mut status = self.last_status;
        let mut connector = Connector::Always;

        for item in &items {
            let should_run = match connector {
                Connector::Always => true,
                Connector::And => status == 0,
                Connector::Or => status != 0,
            };

            if should_run {
                match self.run_item(&item.words) {
                    Flow::Status(code) => {
                        status = code;
                        self.last_status = code;
                    }
                    Flow::Exit(code) => return code,
                }
            }

            connector = item.connector;
        }

        status
    }

    pub fn var(&self, name: &str) -> String {
        match name {
            "?" => return self.last_status.to_string(),
            _ => {}
        }

        self.env
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .unwrap_or_default()
    }

    fn run_item(&mut self, words: &[Word]) -> Flow {
        let argv: Vec<String> = words.iter().flat_map(|word| expand_word(word, self)).collect();
        if argv.is_empty() {
            return Flow::Status(0);
        }

        match self.builtin(&argv) {
            Some(flow) => flow,
            None => Flow::Status(exec::run(&argv, self)),
        }
    }

    fn builtin(&mut self, words: &[String]) -> Option<Flow> {
        match words[0].as_str() {
            ":" | "true" => Some(Flow::Status(0)),
            "false" => Some(Flow::Status(1)),
            "exit" => Some(Flow::Exit(
                words
                    .get(1)
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(self.last_status),
            )),
            "cd" => Some(Flow::Status(self.cd(words.get(1).map(String::as_str)))),
            _ => None,
        }
    }

    fn cd(&mut self, target: Option<&str>) -> i32 {
        let Some(target) = target else {
            eprintln!("sharky: cd: missing operand");
            return 1;
        };

        match std::env::set_current_dir(target) {
            Ok(()) => {
                self.cwd = std::env::current_dir().unwrap_or_else(|_| self.cwd.clone());
                0
            }
            Err(err) => {
                eprintln!("sharky: cd: {target}: {err}");
                1
            }
        }
    }
}

fn parse_items(tokens: Vec<Token>) -> Result<Vec<Item>, String> {
    let mut items = Vec::new();
    let mut words = Vec::new();

    for token in tokens {
        match token.kind {
            TokenKind::Word(word) => words.push(word),
            TokenKind::Semi | TokenKind::Newline => {
                if !words.is_empty() {
                    items.push(Item {
                        words: std::mem::take(&mut words),
                        connector: Connector::Always,
                    });
                }
            }
            TokenKind::AndIf | TokenKind::OrIf => {
                if words.is_empty() {
                    return Err("syntax error near logical operator".to_string());
                }
                items.push(Item {
                    words: std::mem::take(&mut words),
                    connector: if matches!(token.kind, TokenKind::AndIf) {
                        Connector::And
                    } else {
                        Connector::Or
                    },
                });
            }
            TokenKind::Eof => break,
            other => return Err(format!("unsupported syntax: {other:?}")),
        }
    }

    if !words.is_empty() {
        items.push(Item {
            words,
            connector: Connector::Always,
        });
    }

    Ok(items)
}

fn expand_word(word: &Word, shell: &Shell) -> Vec<String> {
    let mut fields = Fields::new();

    for part in &word.parts {
        match part {
            Part::Literal(text) => fields.push(text, false),
            Part::Single(text) | Part::Escaped(text) => fields.push(text, true),
            Part::Double(inner) => {
                let mut text = String::new();
                expand_text(inner, shell, &mut text);
                fields.push(&text, true);
            }
            Part::Variable(name) => fields.push(&shell.var(name), false),
        }
    }

    fields.finish()
}

fn expand_text(parts: &[Part], shell: &Shell, out: &mut String) {
    for part in parts {
        match part {
            Part::Literal(text) | Part::Single(text) | Part::Escaped(text) => out.push_str(text),
            Part::Double(inner) => expand_text(inner, shell, out),
            Part::Variable(name) => out.push_str(&shell.var(name)),
        }
    }
}

struct Fields {
    done: Vec<String>,
    current: String,
    started: bool,
}

impl Fields {
    fn new() -> Self {
        Fields {
            done: Vec::new(),
            current: String::new(),
            started: false,
        }
    }

    fn push(&mut self, text: &str, quoted: bool) {
        if quoted {
            self.current.push_str(text);
            self.started = true;
            return;
        }

        for char in text.chars() {
            if matches!(char, ' ' | '\t' | '\n') {
                if self.started {
                    self.flush();
                }
            } else {
                self.current.push(char);
                self.started = true;
            }
        }
    }

    fn flush(&mut self) {
        self.done.push(std::mem::take(&mut self.current));
        self.started = false;
    }

    fn finish(mut self) -> Vec<String> {
        if self.started {
            self.flush();
        }
        self.done
    }
}
