use crate::ast::literal;
use crate::lexer::{Part, Word};
use crate::shell::glob_match;

pub fn evaluate(words: &[Word], expand: &mut dyn FnMut(&Word) -> String) -> Result<bool, String> {
    let mut parser = Parser {
        words,
        pos: 0,
        expand,
    };
    let value = parser.or()?;
    if parser.pos < words.len() {
        return Err(format!("unexpected '{}'", parser.describe()));
    }
    Ok(value)
}

struct Parser<'a> {
    words: &'a [Word],
    pos: usize,
    expand: &'a mut dyn FnMut(&Word) -> String,
}

impl Parser<'_> {
    fn describe(&self) -> String {
        self.words
            .get(self.pos)
            .and_then(literal)
            .unwrap_or_default()
    }

    fn at(&self, operator: &str) -> bool {
        self.words
            .get(self.pos)
            .and_then(bare)
            .is_some_and(|text| text == operator)
    }

    fn operator(&self) -> Option<String> {
        self.words.get(self.pos).and_then(bare)
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut value = self.and()?;
        while self.at("||") {
            self.pos += 1;
            let right = self.and()?;
            value = value || right;
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut value = self.not()?;
        while self.at("&&") {
            self.pos += 1;
            let right = self.not()?;
            value = value && right;
        }
        Ok(value)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.at("!") {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        if self.at("(") {
            self.pos += 1;
            let value = self.or()?;
            if !self.at(")") {
                return Err("expected ')'".to_string());
            }
            self.pos += 1;
            return Ok(value);
        }

        let Some(word) = self.words.get(self.pos).cloned() else {
            return Err("missing operand".to_string());
        };
        self.pos += 1;

        if let Some(operator) = bare(&word)
            && matches!(
                operator.as_str(),
                "-n" | "-z" | "-e" | "-a" | "-f" | "-d" | "-r" | "-w" | "-x" | "-s" | "-L"
                    | "-h"
            )
            && self.pos < self.words.len()
        {
            let operand = self.words[self.pos].clone();
            self.pos += 1;
            let value = (self.expand)(&operand);
            return Ok(unary(&operator, &value));
        }

        let left = (self.expand)(&word);
        let Some(operator) = self.operator() else {
            return Ok(!left.is_empty());
        };

        if !matches!(
            operator.as_str(),
            "==" | "=" | "!=" | "=~" | "<" | ">" | "-eq" | "-ne" | "-lt" | "-le" | "-gt"
                    | "-ge"
        ) {
            return Ok(!left.is_empty());
        }

        self.pos += 1;
        let Some(right_word) = self.words.get(self.pos).cloned() else {
            return Err(format!("missing operand after '{operator}'"));
        };
        self.pos += 1;
        let right = (self.expand)(&right_word);
        binary(&operator, &left, &right)
    }
}

fn bare(word: &Word) -> Option<String> {
    match word.parts.as_slice() {
        [Part::Literal(text)] => Some(text.clone()),
        _ => None,
    }
}

fn to_i(value: &str) -> i64 {
    let value = value.trim();
    let mut end = usize::from(value.starts_with('-') || value.starts_with('+'));

    while end < value.len() && value.as_bytes()[end].is_ascii_digit() {
        end += 1;
    }

    value[..end].parse().unwrap_or(0)
}

fn unary(operator: &str, value: &str) -> bool {
    match operator {
        "-n" => !value.is_empty(),
        "-z" => value.is_empty(),
        "-e" | "-a" => std::fs::symlink_metadata(value).is_ok(),
        "-L" | "-h" => std::fs::symlink_metadata(value)
            .is_ok_and(|meta| meta.file_type().is_symlink()),
        "-f" => std::fs::metadata(value).is_ok_and(|meta| meta.is_file()),
        "-d" => std::fs::metadata(value).is_ok_and(|meta| meta.is_dir()),
        "-s" => std::fs::metadata(value).is_ok_and(|meta| meta.len() > 0),
        "-r" => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(value)
                .is_ok_and(|meta| meta.permissions().mode() & 0o444 != 0)
        }
        "-w" => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(value)
                .is_ok_and(|meta| meta.permissions().mode() & 0o222 != 0)
        }
        "-x" => {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(value)
                .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0)
        }
        _ => false,
    }
}

fn binary(operator: &str, left: &str, right: &str) -> Result<bool, String> {
    match operator {
        "==" | "=" => Ok(glob_match(right, left)),
        "!=" => Ok(!glob_match(right, left)),
        "=~" => Ok(regex_lite::Regex::new(right)
            .map(|regex| regex.is_match(left))
            .unwrap_or(false)),
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        _ => {
            let left = to_i(left);
            let right = to_i(right);
            Ok(match operator {
                "-eq" => left == right,
                "-ne" => left != right,
                "-lt" => left < right,
                "-le" => left <= right,
                "-gt" => left > right,
                "-ge" => left >= right,
                _ => return Err(format!("unknown operator '{operator}'")),
            })
        }
    }
}
