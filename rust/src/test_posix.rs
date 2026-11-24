//! POSIX `test` and `[` conditions.
//!
//! `[` is the same builtin as `test` with a required closing `]`. The meaning of
//! an expression depends on how many arguments it received, exactly as in bash:
//! the fixed zero-to-four-argument forms are tried first and anything longer is
//! parsed as a condition expression where `-o` binds looser than `-a`, `!`
//! negates a primary, and parentheses group.
//!
//! Both builtins exist so a login session never depends on `test`/`[` being
//! reachable through `PATH`: the standard profile files only work if their
//! guards can be evaluated before `PATH` itself is set up.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;

/// Evaluates the arguments of `test` (or `[`) after expansion.
///
/// Returns the truth of the expression, or the diagnostic for a malformed one;
/// the caller turns those into statuses 0/1 and 2.
pub fn run(args: &[String], bracket: bool) -> Result<bool, String> {
    let args = if bracket {
        match args.split_last() {
            Some((last, rest)) if last == "]" => rest,
            _ => return Err("missing `]'".to_string()),
        }
    } else {
        args
    };

    match args.len() {
        0 => Ok(false),
        1 => Ok(!args[0].is_empty()),
        2 => two(args),
        3 => three(args),
        4 => match four(args) {
            Some(value) => value,
            None => expression(args),
        },
        _ => expression(args),
    }
}

fn two(args: &[String]) -> Result<bool, String> {
    if args[0] == "!" {
        return Ok(!(!args[1].is_empty()));
    }
    if is_unary(&args[0]) {
        return Ok(unary(&args[0], &args[1]));
    }
    Err(format!("{}: unary operator expected", args[0]))
}

fn three(args: &[String]) -> Result<bool, String> {
    if args[0] == "!" {
        return Ok(!two(&args[1..])?);
    }
    // Only the fixed three-argument form reads `-a`/`-o` as binary operators;
    // inside a longer expression they are the AND/OR connectors. A missing
    // right operand only disqualifies the string and file operators.
    let operator = args[1].as_str();
    if (is_binary(operator) && !args[2].is_empty()) || matches!(operator, "-a" | "-o") {
        return binary(operator, &args[0], &args[2]);
    }
    if args[0] == "(" && args[2] == ")" {
        return Ok(!args[1].is_empty());
    }
    Err(format!("{}: binary operator expected", args[1]))
}

/// The two four-argument forms bash recognises; `None` means the generic
/// expression parser decides.
fn four(args: &[String]) -> Option<Result<bool, String>> {
    if args[0] == "!" {
        return Some(three(&args[1..]).map(|value| !value));
    }
    if args[0] == "(" && args[3] == ")" {
        return Some(two(&args[1..3]));
    }
    None
}

fn expression(args: &[String]) -> Result<bool, String> {
    let mut parser = Parser { args, pos: 0 };
    let value = parser.or()?;
    if parser.pos < args.len() {
        return Err("too many arguments".to_string());
    }
    Ok(value)
}

struct Parser<'a> {
    args: &'a [String],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        let args: &'a [String] = self.args;
        args.get(self.pos).map(String::as_str)
    }

    fn next(&mut self) -> Option<&'a str> {
        let word = self.peek()?;
        self.pos += 1;
        Some(word)
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut value = self.and()?;
        while self.peek() == Some("-o") {
            self.pos += 1;
            let right = self.and()?;
            value = value || right;
        }
        Ok(value)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut value = self.not()?;
        while self.peek() == Some("-a") {
            self.pos += 1;
            let right = self.not()?;
            value = value && right;
        }
        Ok(value)
    }

    fn not(&mut self) -> Result<bool, String> {
        if self.peek() == Some("!") {
            self.pos += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        if self.peek() == Some("(") {
            self.pos += 1;
            let value = self.or()?;
            if self.next() != Some(")") {
                return Err("too many arguments".to_string());
            }
            return Ok(value);
        }

        let Some(word) = self.next() else {
            return Err("too many arguments".to_string());
        };
        let word = word.to_string();

        if is_unary(&word) {
            let Some(operand) = self.next().map(str::to_string) else {
                return Err(format!("{word}: unary operator expected"));
            };
            return Ok(unary(&word, &operand));
        }

        if let Some(operator) = self.peek().map(str::to_string)
            && is_binary(&operator)
        {
            self.pos += 1;
            let Some(right) = self.next().map(str::to_string) else {
                return Err(format!("{operator}: argument expected"));
            };
            return binary(&operator, &word, &right);
        }

        Ok(!word.is_empty())
    }
}

pub(crate) fn is_unary(operator: &str) -> bool {
    matches!(
        operator,
        "-a" | "-b"
            | "-c"
            | "-d"
            | "-e"
            | "-f"
            | "-g"
            | "-G"
            | "-h"
            | "-k"
            | "-L"
            | "-n"
            | "-N"
            | "-O"
            | "-p"
            | "-r"
            | "-s"
            | "-S"
            | "-t"
            | "-u"
            | "-w"
            | "-x"
            | "-z"
    )
}

fn is_binary(operator: &str) -> bool {
    matches!(
        operator,
        "=" | "=="
            | "!="
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
    )
}

pub(crate) fn unary(operator: &str, value: &str) -> bool {
    let path = Path::new(value);
    match operator {
        "-n" => !value.is_empty(),
        "-z" => value.is_empty(),
        // `-e` and the historical `-a` follow symlinks, so a dangling link is
        // not an existing file; `-L`/`-h` look at the link itself.
        "-e" | "-a" => path.exists(),
        "-L" | "-h" => std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_symlink()),
        "-f" => std::fs::metadata(path).is_ok_and(|meta| meta.is_file()),
        "-d" => std::fs::metadata(path).is_ok_and(|meta| meta.is_dir()),
        "-b" => std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_block_device()),
        "-c" => std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_char_device()),
        "-p" => std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_fifo()),
        "-S" => std::fs::metadata(path).is_ok_and(|meta| meta.file_type().is_socket()),
        "-s" => std::fs::metadata(path).is_ok_and(|meta| meta.len() > 0),
        "-g" => mode(path, 0o2000),
        "-u" => mode(path, 0o4000),
        "-k" => mode(path, 0o1000),
        "-r" => accessible(path, nix::unistd::AccessFlags::R_OK),
        "-w" => accessible(path, nix::unistd::AccessFlags::W_OK),
        "-x" => accessible(path, nix::unistd::AccessFlags::X_OK),
        "-O" => {
            std::fs::metadata(path).is_ok_and(|meta| meta.uid() == unsafe { nix::libc::geteuid() })
        }
        "-G" => {
            std::fs::metadata(path).is_ok_and(|meta| meta.gid() == unsafe { nix::libc::getegid() })
        }
        "-N" => std::fs::metadata(path).is_ok_and(|meta| meta.mtime() > meta.atime()),
        "-t" => value
            .trim()
            .parse::<i32>()
            .is_ok_and(|fd| unsafe { nix::libc::isatty(fd) } == 1),
        _ => false,
    }
}

fn mode(path: &Path, flag: u32) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.permissions().mode() & flag != 0)
}

fn accessible(path: &Path, flag: nix::unistd::AccessFlags) -> bool {
    nix::unistd::access(path, flag).is_ok()
}

fn binary(operator: &str, left: &str, right: &str) -> Result<bool, String> {
    match operator {
        // `[` compares strings literally; glob matching belongs to `[[ ]]`.
        "=" | "==" => Ok(left == right),
        "!=" => Ok(left != right),
        "<" => Ok(left < right),
        ">" => Ok(left > right),
        "-a" => Ok(!left.is_empty() && !right.is_empty()),
        "-o" => Ok(!left.is_empty() || !right.is_empty()),
        "-nt" => Ok(newer(left, right)),
        "-ot" => Ok(newer(right, left)),
        "-ef" => Ok(same_file(left, right)),
        _ => {
            let left = integer(left)?;
            let right = integer(right)?;
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

/// `test -nt`: the left file exists and is newer than the right one, or the
/// right one is missing.
pub(crate) fn newer(left: &str, right: &str) -> bool {
    let left = std::fs::metadata(left);
    let right = std::fs::metadata(right);
    match (left, right) {
        (Ok(left), Ok(right)) => left.modified().ok() > right.modified().ok(),
        (Ok(_), Err(_)) => true,
        _ => false,
    }
}

pub(crate) fn same_file(left: &str, right: &str) -> bool {
    let (Ok(left), Ok(right)) = (std::fs::metadata(left), std::fs::metadata(right)) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Mirrors bash: surrounding blanks and a sign are accepted, anything that is
/// not a whole number is an error naming the offending argument.
fn integer(value: &str) -> Result<i128, String> {
    let text = value.trim();
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("{value}: integer expected"));
    }
    text.parse()
        .map_err(|_| format!("{value}: integer expected"))
}
