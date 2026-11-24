use crate::shell::Shell;

/// Why an arithmetic evaluation stopped. A refused write to a readonly
/// variable already printed its message, so the callers report it differently
/// from a syntax or division error.
#[derive(Debug, Clone, PartialEq)]
pub enum ArithError {
    Message(String),
    Readonly,
}

impl From<String> for ArithError {
    fn from(message: String) -> Self {
        ArithError::Message(message)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(i64),
    Name(String),
    Op(&'static str),
    Open,
    Close,
    End,
}

pub fn evaluate(source: &str, shell: &mut Shell) -> Result<i64, ArithError> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        shell,
        active: true,
    };
    let value = parser.expression(0)?;
    match parser.peek() {
        Token::End => Ok(value),
        other => Err(format!("unexpected {other:?} in arithmetic").into()),
    }
}

/// Digit value used by `base#number` literals. Bash accepts the full
/// `0-9a-zA-Z@_` alphabet, so bases up to 64 are supported.
fn digit_value(char: char) -> Option<u32> {
    char.to_digit(36).or(match char {
        '@' => Some(62),
        '_' => Some(63),
        _ => None,
    })
}

fn is_literal_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || matches!(char, '@' | '_')
}

/// Accumulate digits with wrapping arithmetic, matching bash's behavior for
/// constants wider than 64 bits.
fn parse_radix(digits: &[char], base: u32) -> Option<i64> {
    let mut value = 0_i64;
    for &char in digits {
        let digit = digit_value(char)?;
        if digit >= base {
            return None;
        }
        value = value
            .wrapping_mul(i64::from(base))
            .wrapping_add(i64::from(digit));
    }
    Some(value)
}

fn too_great(token: &str) -> String {
    format!("{token}: value too great for base (error token is \"{token}\")")
}

fn tokenize(source: &str) -> Result<Vec<Token>, ArithError> {
    let chars: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut pos = 0;

    while pos < chars.len() {
        let char = chars[pos];
        if char.is_whitespace() {
            pos += 1;
            continue;
        }

        if char.is_ascii_digit() {
            let start = pos;

            // `base#number` literals, e.g. 16#ff or 2#101.
            let mut base_end = pos;
            while base_end < chars.len() && chars[base_end].is_ascii_digit() {
                base_end += 1;
            }
            if base_end < chars.len() && chars[base_end] == '#' {
                let base_text = &chars[start..base_end];
                let token: String = chars[start..base_end + 1].iter().collect();
                // A leading zero makes the base field octal, just like any
                // other integer literal (so `08#1` is an error in bash).
                let base = if base_text.len() > 1 && base_text[0] == '0' {
                    parse_radix(&base_text[1..], 8).and_then(|value| u32::try_from(value).ok())
                } else {
                    base_text.iter().collect::<String>().parse::<u32>().ok()
                };
                let Some(base) = base.filter(|base| (2..=64).contains(base)) else {
                    return Err(format!(
                        "{token}: invalid arithmetic base (error token is \"{token}\")"
                    )
                    .into());
                };
                pos = base_end + 1;
                let digits_start = pos;
                while pos < chars.len() && digit_value(chars[pos]).is_some() {
                    pos += 1;
                }
                if pos == digits_start {
                    let token: String = chars[start..pos].iter().collect();
                    return Err(format!(
                        "{token}: invalid integer constant (error token is \"{token}\")"
                    )
                    .into());
                }
                let token: String = chars[start..pos].iter().collect();
                let Some(value) = parse_radix(&chars[digits_start..pos], base) else {
                    return Err(too_great(&token).into());
                };
                tokens.push(Token::Number(value));
                continue;
            }

            // Hexadecimal literals, e.g. 0x10 or 0Xff.
            if char == '0' && matches!(chars.get(pos + 1), Some('x' | 'X')) {
                pos += 2;
                let digits_start = pos;
                while pos < chars.len() && chars[pos].is_ascii_hexdigit() {
                    pos += 1;
                }
                if pos < chars.len() && is_literal_char(chars[pos]) {
                    while pos < chars.len() && is_literal_char(chars[pos]) {
                        pos += 1;
                    }
                    let token: String = chars[start..pos].iter().collect();
                    return Err(too_great(&token).into());
                }
                if pos == digits_start {
                    // Bash accepts a bare `0x` as zero.
                    tokens.push(Token::Number(0));
                    continue;
                }
                let token: String = chars[start..pos].iter().collect();
                let Some(value) = parse_radix(&chars[digits_start..pos], 16) else {
                    return Err(too_great(&token).into());
                };
                tokens.push(Token::Number(value));
                continue;
            }

            // Binary is not a bash integer literal prefix; report the whole
            // token instead of silently parsing a decimal zero followed by a name.
            if char == '0' && matches!(chars.get(pos + 1), Some('b' | 'B')) {
                pos += 2;
                while pos < chars.len() && is_literal_char(chars[pos]) {
                    pos += 1;
                }
                let token: String = chars[start..pos].iter().collect();
                return Err(too_great(&token).into());
            }

            // A leading zero selects octal; 8 and 9 are reported as errors.
            let base = if char == '0' { 8 } else { 10 };
            while pos < chars.len() && chars[pos].is_ascii_digit() {
                pos += 1;
            }
            let token: String = chars[start..pos].iter().collect();
            let Some(value) = parse_radix(&chars[start..pos], base) else {
                return Err(too_great(&token).into());
            };
            tokens.push(Token::Number(value));
            continue;
        }

        if char == '$' {
            pos += 1;
            let start = pos;
            while pos < chars.len()
                && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_')
            {
                pos += 1;
            }
            if start == pos {
                return Err("'$' without a name in arithmetic".to_string().into());
            }
            tokens.push(Token::Name(chars[start..pos].iter().collect()));
            continue;
        }

        if char.is_ascii_alphabetic() || char == '_' {
            let start = pos;
            while pos < chars.len()
                && (chars[pos].is_ascii_alphanumeric() || chars[pos] == '_')
            {
                pos += 1;
            }
            tokens.push(Token::Name(chars[start..pos].iter().collect()));
            continue;
        }

        let rest = &chars[pos..];
        let three: String = rest.iter().take(3).collect();
        let two: String = rest.iter().take(2).collect();

        if let Some(op) = match three.as_str() {
            "<<=" => Some("<<="),
            ">>=" => Some(">>="),
            _ => None,
        } {
            tokens.push(Token::Op(op));
            pos += 3;
            continue;
        }

        if let Some(op) = match two.as_str() {
            "**" => Some("**"),
            "==" => Some("=="),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "&&" => Some("&&"),
            "||" => Some("||"),
            "++" => Some("++"),
            "--" => Some("--"),
            "+=" => Some("+="),
            "-=" => Some("-="),
            "*=" => Some("*="),
            "/=" => Some("/="),
            "%=" => Some("%="),
            "&=" => Some("&="),
            "|=" => Some("|="),
            "^=" => Some("^="),
            "<<" => Some("<<"),
            ">>" => Some(">>"),
            _ => None,
        } {
            tokens.push(Token::Op(op));
            pos += 2;
            continue;
        }

        match char {
            '(' => {
                tokens.push(Token::Open);
                pos += 1;
            }
            ')' => {
                tokens.push(Token::Close);
                pos += 1;
            }
            '+' => {
                tokens.push(Token::Op("+"));
                pos += 1;
            }
            '-' => {
                tokens.push(Token::Op("-"));
                pos += 1;
            }
            '*' => {
                tokens.push(Token::Op("*"));
                pos += 1;
            }
            '/' => {
                tokens.push(Token::Op("/"));
                pos += 1;
            }
            '%' => {
                tokens.push(Token::Op("%"));
                pos += 1;
            }
            '=' => {
                tokens.push(Token::Op("="));
                pos += 1;
            }
            '<' => {
                tokens.push(Token::Op("<"));
                pos += 1;
            }
            '>' => {
                tokens.push(Token::Op(">"));
                pos += 1;
            }
            '!' => {
                tokens.push(Token::Op("!"));
                pos += 1;
            }
            '&' => {
                tokens.push(Token::Op("&"));
                pos += 1;
            }
            '|' => {
                tokens.push(Token::Op("|"));
                pos += 1;
            }
            '^' => {
                tokens.push(Token::Op("^"));
                pos += 1;
            }
            '~' => {
                tokens.push(Token::Op("~"));
                pos += 1;
            }
            '?' => {
                tokens.push(Token::Op("?"));
                pos += 1;
            }
            ':' => {
                tokens.push(Token::Op(":"));
                pos += 1;
            }
            other => return Err(format!("unexpected '{other}' in arithmetic").into()),
        }
    }

    tokens.push(Token::End);
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    shell: &'a mut Shell,
    active: bool,
}

impl Parser<'_> {
    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::End)
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        self.pos += 1;
        token
    }

    fn expression(&mut self, min_power: u8) -> Result<i64, ArithError> {
        if min_power == 0
            && let Some(value) = self.assignment()?
        {
            return Ok(value);
        }

        let mut left = self.unary()?;

        while let Token::Op(op) = *self.peek() {
            let Some((power, right_power)) = binding(op) else {
                break;
            };
            if power < min_power {
                break;
            }
            self.advance();

            if op == "&&" && left == 0 {
                self.with_active(false, |parser| parser.expression(right_power))?;
                left = 0;
                continue;
            }
            if op == "||" && left != 0 {
                self.with_active(false, |parser| parser.expression(right_power))?;
                left = 1;
                continue;
            }
            if op == "?" {
                left = self.conditional(left)?;
                continue;
            }

            let right = self.expression(right_power)?;
            left = if self.active {
                apply(op, left, right)?
            } else {
                0
            };
        }

        Ok(left)
    }

    /// Parse the two branches of `condition ? when_true : when_false`.
    /// The untaken branch is parsed with `active` disabled so assignments and
    /// runtime errors in it have no effect, matching bash's lazy evaluation.
    fn conditional(&mut self, condition: i64) -> Result<i64, ArithError> {
        let when_true = if condition != 0 {
            self.expression(0)?
        } else {
            self.with_active(false, |parser| parser.expression(0))?
        };
        if self.advance() != Token::Op(":") {
            return Err("expected ':' in arithmetic conditional".to_string().into());
        }
        // The false branch is right-associative and, like bash, cannot start
        // with a bare assignment (parentheses are required for that).
        let when_false = if condition != 0 {
            self.with_active(false, |parser| parser.expression(1))?
        } else {
            self.expression(1)?
        };
        if !self.active {
            return Ok(0);
        }
        Ok(if condition != 0 { when_true } else { when_false })
    }

    fn with_active<T>(
        &mut self,
        active: bool,
        action: impl FnOnce(&mut Self) -> Result<T, ArithError>,
    ) -> Result<T, ArithError> {
        let previous = self.active;
        self.active = active;
        let result = action(self);
        self.active = previous;
        result
    }

    fn assignment(&mut self) -> Result<Option<i64>, ArithError> {
        let Token::Name(name) = self.peek().clone() else {
            return Ok(None);
        };
        let Some(Token::Op(operator)) = self.tokens.get(self.pos + 1).cloned() else {
            return Ok(None);
        };
        if !matches!(
            operator,
            "=" | "+="
                | "-="
                | "*="
                | "/="
                | "%="
                | "&="
                | "|="
                | "^="
                | "<<="
                | ">>="
        ) {
            return Ok(None);
        }

        self.advance();
        self.advance();
        let right = self.expression(0)?;
        if !self.active {
            return Ok(Some(0));
        }
        let current = self.shell.var(&name).trim().parse::<i64>().unwrap_or(0);
        let value = match operator {
            "=" => right,
            "+=" => current.wrapping_add(right),
            "-=" => current.wrapping_sub(right),
            "*=" => current.wrapping_mul(right),
            "/=" => {
                if right == 0 {
                    return Err("division by zero".to_string().into());
                }
                current.wrapping_div(right)
            }
            "%=" => {
                if right == 0 {
                    return Err("division by zero".to_string().into());
                }
                current.wrapping_rem(right)
            }
            "&=" => current & right,
            "|=" => current | right,
            "^=" => current ^ right,
            "<<=" => current.wrapping_shl(right as u32),
            ">>=" => current.wrapping_shr(right as u32),
            _ => unreachable!(),
        };
        if !self.shell.set_var(&name, value.to_string()) {
            return Err(ArithError::Readonly);
        }
        Ok(Some(value))
    }

    fn unary(&mut self) -> Result<i64, ArithError> {
        match self.peek().clone() {
            Token::Op("++") | Token::Op("--") => {
                let increment = self.peek() == &Token::Op("++");
                self.advance();
                let Token::Name(name) = self.peek().clone() else {
                    return Err("increment needs a variable".to_string().into());
                };
                self.advance();
                let current = self.shell.var(&name).trim().parse::<i64>().unwrap_or(0);
                let value = if increment {
                    current.wrapping_add(1)
                } else {
                    current.wrapping_sub(1)
                };
                if self.active && !self.shell.set_var(&name, value.to_string()) {
                    return Err(ArithError::Readonly);
                }
                Ok(value)
            }
            Token::Op("-") => {
                self.advance();
                Ok(-self.unary()?)
            }
            Token::Op("+") => {
                self.advance();
                self.unary()
            }
            Token::Op("!") => {
                self.advance();
                Ok((self.unary()? == 0) as i64)
            }
            Token::Op("~") => {
                self.advance();
                Ok(!self.unary()?)
            }
            Token::Open => {
                self.advance();
                let value = self.expression(0)?;
                match self.advance() {
                    Token::Close => Ok(value),
                    _ => Err("missing ')' in arithmetic".to_string().into()),
                }
            }
            Token::Number(value) => {
                self.advance();
                Ok(value)
            }
            Token::Name(name) => {
                self.advance();
                let raw = self.shell.var(&name);
                let value: i64 = raw.trim().parse().unwrap_or(0);
                if self.peek() == &Token::Op("++") || self.peek() == &Token::Op("--") {
                    let increment = self.peek() == &Token::Op("++");
                    self.advance();
                    let next = if increment {
                        value.wrapping_add(1)
                    } else {
                        value.wrapping_sub(1)
                    };
                    if self.active && !self.shell.set_var(&name, next.to_string()) {
                        return Err(ArithError::Readonly);
                    }
                    return Ok(value);
                }
                Ok(value)
            }
            other => Err(format!("unexpected {other:?} in arithmetic").into()),
        }
    }
}

/// Binding powers follow bash's precedence table, from lowest to highest.
/// The second value is the right-hand minimum power (`+1` for left-associative
/// operators, equal for right-associative ones).
fn binding(op: &str) -> Option<(u8, u8)> {
    match op {
        "?" => Some((1, 1)),
        "||" => Some((2, 3)),
        "&&" => Some((3, 4)),
        "|" => Some((4, 5)),
        "^" => Some((5, 6)),
        "&" => Some((6, 7)),
        "==" | "!=" => Some((7, 8)),
        "<" | "<=" | ">" | ">=" => Some((8, 9)),
        "<<" | ">>" => Some((9, 10)),
        "+" | "-" => Some((10, 11)),
        "*" | "/" | "%" => Some((11, 12)),
        "**" => Some((13, 13)),
        _ => None,
    }
}

fn apply(op: &str, left: i64, right: i64) -> Result<i64, ArithError> {
    Ok(match op {
        "+" => left.wrapping_add(right),
        "-" => left.wrapping_sub(right),
        "*" => left.wrapping_mul(right),
        "/" => {
            if right == 0 {
                return Err("division by zero".to_string().into());
            }
            left.wrapping_div(right)
        }
        "%" => {
            if right == 0 {
                return Err("division by zero".to_string().into());
            }
            left.wrapping_rem(right)
        }
        "**" => {
            if right < 0 {
                return Err("negative exponent".to_string().into());
            }
            left.wrapping_pow(right as u32)
        }
        "<<" => left.wrapping_shl(right as u32),
        ">>" => left.wrapping_shr(right as u32),
        "&" => left & right,
        "|" => left | right,
        "^" => left ^ right,
        "==" => (left == right) as i64,
        "!=" => (left != right) as i64,
        "<" => (left < right) as i64,
        "<=" => (left <= right) as i64,
        ">" => (left > right) as i64,
        ">=" => (left >= right) as i64,
        "&&" => (left != 0 && right != 0) as i64,
        "||" => (left != 0 || right != 0) as i64,
        _ => return Err(format!("unknown operator '{op}'").into()),
    })
}
