use crate::shell::Shell;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(i64),
    Name(String),
    Op(&'static str),
    Open,
    Close,
    End,
}

pub fn evaluate(source: &str, shell: &Shell) -> Result<i64, String> {
    let tokens = tokenize(source)?;
    let mut parser = Parser {
        tokens,
        pos: 0,
        shell,
    };
    let value = parser.expression(0)?;
    match parser.peek() {
        Token::End => Ok(value),
        other => Err(format!("unexpected {other:?} in arithmetic")),
    }
}

fn tokenize(source: &str) -> Result<Vec<Token>, String> {
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
            while pos < chars.len() && chars[pos].is_ascii_digit() {
                pos += 1;
            }
            let text: String = chars[start..pos].iter().collect();
            let value = text
                .parse()
                .map_err(|_| format!("invalid number '{text}'"))?;
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
                return Err("'$' without a name in arithmetic".to_string());
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
        let two: String = rest.iter().take(2).collect();

        if let Some(op) = match two.as_str() {
            "**" => Some("**"),
            "==" => Some("=="),
            "!=" => Some("!="),
            "<=" => Some("<="),
            ">=" => Some(">="),
            "&&" => Some("&&"),
            "||" => Some("||"),
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
            other => return Err(format!("unexpected '{other}' in arithmetic")),
        }
    }

    tokens.push(Token::End);
    Ok(tokens)
}

struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    shell: &'a Shell,
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

    fn expression(&mut self, min_power: u8) -> Result<i64, String> {
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
                self.expression(right_power)?;
                left = 0;
                continue;
            }
            if op == "||" && left != 0 {
                self.expression(right_power)?;
                left = 1;
                continue;
            }

            let right = self.expression(right_power)?;
            left = apply(op, left, right)?;
        }

        Ok(left)
    }

    fn unary(&mut self) -> Result<i64, String> {
        match self.peek().clone() {
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
            Token::Open => {
                self.advance();
                let value = self.expression(0)?;
                match self.advance() {
                    Token::Close => Ok(value),
                    _ => Err("missing ')' in arithmetic".to_string()),
                }
            }
            Token::Number(value) => {
                self.advance();
                Ok(value)
            }
            Token::Name(name) => {
                self.advance();
                let raw = self.shell.var(&name);
                Ok(raw.trim().parse().unwrap_or(0))
            }
            other => Err(format!("unexpected {other:?} in arithmetic")),
        }
    }
}

fn binding(op: &str) -> Option<(u8, u8)> {
    match op {
        "||" => Some((1, 2)),
        "&&" => Some((2, 3)),
        "==" | "!=" | "<" | "<=" | ">" | ">=" => Some((3, 4)),
        "+" | "-" => Some((4, 5)),
        "*" | "/" | "%" => Some((5, 6)),
        "**" => Some((7, 7)),
        _ => None,
    }
}

fn apply(op: &str, left: i64, right: i64) -> Result<i64, String> {
    Ok(match op {
        "+" => left.wrapping_add(right),
        "-" => left.wrapping_sub(right),
        "*" => left.wrapping_mul(right),
        "/" => {
            if right == 0 {
                return Err("division by zero".to_string());
            }
            left.wrapping_div(right)
        }
        "%" => {
            if right == 0 {
                return Err("division by zero".to_string());
            }
            left.wrapping_rem(right)
        }
        "**" => {
            if right < 0 {
                return Err("negative exponent".to_string());
            }
            left.wrapping_pow(right as u32)
        }
        "==" => (left == right) as i64,
        "!=" => (left != right) as i64,
        "<" => (left < right) as i64,
        "<=" => (left <= right) as i64,
        ">" => (left > right) as i64,
        ">=" => (left >= right) as i64,
        "&&" => (left != 0 && right != 0) as i64,
        "||" => (left != 0 || right != 0) as i64,
        _ => return Err(format!("unknown operator '{op}'")),
    })
}
