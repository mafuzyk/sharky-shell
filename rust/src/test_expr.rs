use crate::ast::literal;
use crate::lexer::{Part, Word};
use crate::shell::glob_match_opts;

pub trait Context {
    fn expand(&mut self, word: &Word) -> String;
    fn var_is_set(&self, name: &str) -> bool;
    fn option_is_set(&self, name: &str) -> bool;
    fn set_rematch(&mut self, groups: Vec<String>);
    fn nocasematch(&self) -> bool;
    fn extglob(&self) -> bool;
}

pub fn evaluate(words: &[Word], ctx: &mut dyn Context) -> Result<bool, String> {
    let mut parser = Parser { words, pos: 0, ctx };
    let value = parser.or()?;
    if parser.pos < words.len() {
        return Err(format!("unexpected '{}'", parser.describe()));
    }
    Ok(value)
}

struct Parser<'a> {
    words: &'a [Word],
    pos: usize,
    ctx: &'a mut dyn Context,
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
            && (crate::test_posix::is_unary(operator.as_str())
                || matches!(operator.as_str(), "-v" | "-o"))
            && self.pos < self.words.len()
        {
            let operand = self.words[self.pos].clone();
            self.pos += 1;
            let value = self.ctx.expand(&operand);
            return Ok(match operator.as_str() {
                "-v" => self.ctx.var_is_set(&value),
                "-o" => self.ctx.option_is_set(&value),
                _ => crate::test_posix::unary(&operator, &value),
            });
        }

        let left = self.ctx.expand(&word);
        let Some(operator) = self.operator() else {
            return Ok(!left.is_empty());
        };

        if !matches!(
            operator.as_str(),
            "==" | "="
                | "!="
                | "=~"
                | "<"
                | ">"
                | "-eq"
                | "-ne"
                | "-lt"
                | "-le"
                | "-gt"
                | "-ge"
                | "-nt"
                | "-ot"
                | "-ef"
        ) {
            return Ok(!left.is_empty());
        }

        self.pos += 1;
        let Some(right_word) = self.words.get(self.pos).cloned() else {
            return Err(format!("missing operand after '{operator}'"));
        };
        self.pos += 1;
        let right = self.ctx.expand(&right_word);
        binary(&operator, &left, &right, self.ctx)
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

fn binary(
    operator: &str,
    left: &str,
    right: &str,
    ctx: &mut dyn Context,
) -> Result<bool, String> {
    let nocasematch = ctx.nocasematch();
    let extglob = ctx.extglob();
    match operator {
        "==" | "=" => Ok(glob_match_opts(right, left, extglob, nocasematch)),
        "!=" => Ok(!glob_match_opts(right, left, extglob, nocasematch)),
        "=~" => {
            let pattern = if nocasematch {
                std::borrow::Cow::Owned(format!("(?i){right}"))
            } else {
                std::borrow::Cow::Borrowed(right)
            };
            let matched = regex_lite::Regex::new(&pattern)
                .ok()
                .and_then(|regex| {
                    regex.captures(left).map(|caps| {
                        (0..caps.len())
                            .map(|idx| {
                                caps.get(idx)
                                    .map(|m| m.as_str().to_string())
                                    .unwrap_or_default()
                            })
                            .collect::<Vec<_>>()
                    })
                });
            match matched {
                Some(groups) => {
                    ctx.set_rematch(groups);
                    Ok(true)
                }
                None => {
                    ctx.set_rematch(Vec::new());
                    Ok(false)
                }
            }
        }
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        "-nt" => Ok(crate::test_posix::newer(left, right)),
        "-ot" => Ok(crate::test_posix::newer(right, left)),
        "-ef" => Ok(crate::test_posix::same_file(left, right)),
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
