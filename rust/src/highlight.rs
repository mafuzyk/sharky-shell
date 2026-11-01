use crate::prompt;
use crate::shell::Shell;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Argument,
    Command,
    Builtin,
    String,
    Variable,
    Operator,
    Comment,
    Path,
    Error,
}

pub fn render(shell: &Shell, text: &[char]) -> String {
    if shell.setting("highlight") == Some("off") {
        return text.iter().collect();
    }

    let commands = shell.command_words();
    let roles = classify_with(text, |word| {
        if commands.iter().any(|name| name == word) {
            Role::Builtin
        } else {
            Role::Command
        }
    });

    let mut out = String::new();
    let mut index = 0;
    while index < text.len() {
        let role = roles[index];
        let start = index;
        while index < text.len() && roles[index] == role {
            index += 1;
        }
        let segment: String = text[start..index].iter().collect();
        if role == Role::Argument {
            out.push_str(&segment);
        } else {
            out.push_str(&prompt::painted(shell, role_key(role), &segment));
        }
    }
    out
}

fn role_key(role: Role) -> &'static str {
    match role {
        Role::Argument => "argument",
        Role::Command => "command",
        Role::Builtin => "builtin",
        Role::String => "string",
        Role::Variable => "variable",
        Role::Operator => "operator",
        Role::Comment => "comment",
        Role::Path => "path",
        Role::Error => "error",
    }
}

pub fn classify_with<F>(text: &[char], resolve: F) -> Vec<Role>
where
    F: Fn(&str) -> Role,
{
    let mut roles = vec![Role::Argument; text.len()];
    let mut index = 0;
    let mut command_position = true;
    let mut redirect_target = false;

    while index < text.len() {
        let char = text[index];

        if char == '\n' {
            command_position = true;
            redirect_target = false;
            index += 1;
            continue;
        }
        if char.is_whitespace() {
            index += 1;
            continue;
        }
        if char == '#' && (index == 0 || text[index - 1].is_whitespace()) {
            while index < text.len() && text[index] != '\n' {
                roles[index] = Role::Comment;
                index += 1;
            }
            continue;
        }
        if is_operator(char) {
            roles[index] = Role::Operator;
            while index + 1 < text.len() && is_operator(text[index + 1]) {
                index += 1;
                roles[index] = Role::Operator;
            }
            match text[index] {
                '<' | '>' => redirect_target = true,
                '|' | ';' | '&' | '(' | '{' => command_position = true,
                ')' | '}' => command_position = false,
                _ => {}
            }
            index += 1;
            continue;
        }

        let start = index;
        let mut plain = String::new();
        let mut special = false;

        while index < text.len() && !text[index].is_whitespace() && !is_operator(text[index]) {
            match text[index] {
                '\'' => {
                    special = true;
                    let quote = index;
                    roles[index] = Role::String;
                    index += 1;
                    let mut closed = false;
                    while index < text.len() {
                        roles[index] = Role::String;
                        if text[index] == '\'' {
                            index += 1;
                            closed = true;
                            break;
                        }
                        index += 1;
                    }
                    if !closed {
                        for role in &mut roles[quote..] {
                            *role = Role::Error;
                        }
                    }
                }
                '"' => {
                    special = true;
                    let quote = index;
                    roles[index] = Role::String;
                    index += 1;
                    let mut closed = false;
                    while index < text.len() {
                        if text[index] == '"' {
                            roles[index] = Role::String;
                            index += 1;
                            closed = true;
                            break;
                        }
                        if text[index] == '$' {
                            index = mark_variable(text, &mut roles, index);
                        } else if text[index] == '\\' {
                            roles[index] = Role::String;
                            index += 1;
                            if index < text.len() {
                                roles[index] = Role::String;
                                index += 1;
                            }
                        } else {
                            roles[index] = Role::String;
                            index += 1;
                        }
                    }
                    if !closed {
                        for role in &mut roles[quote..] {
                            *role = Role::Error;
                        }
                    }
                }
                '$' if text.get(index + 1) == Some(&'\'') => {
                    special = true;
                    let start = index;
                    roles[index] = Role::String;
                    index += 2;
                    let mut closed = false;
                    while index < text.len() {
                        roles[index] = Role::String;
                        if text[index] == '\'' {
                            index += 1;
                            closed = true;
                            break;
                        }
                        index += 1;
                    }
                    if !closed {
                        for role in &mut roles[start..] {
                            *role = Role::Error;
                        }
                    }
                }
                '$' => {
                    special = true;
                    index = mark_variable(text, &mut roles, index);
                }
                '\\' => {
                    special = true;
                    roles[index] = Role::String;
                    index += 1;
                    if index < text.len() {
                        roles[index] = Role::String;
                        index += 1;
                    }
                }
                char => {
                    plain.push(char);
                    index += 1;
                }
            }
        }

        if !special {
            let role = if redirect_target {
                Role::Path
            } else if command_position {
                resolve(&plain)
            } else if looks_like_path(&plain) {
                Role::Path
            } else {
                Role::Argument
            };
            for slot in &mut roles[start..index] {
                *slot = role;
            }
        }

        if command_position {
            let word: String = text[start..index].iter().collect();
            command_position = command_starter(&word);
        }
        redirect_target = false;
    }

    roles
}

fn mark_variable(text: &[char], roles: &mut [Role], start: usize) -> usize {
    roles[start] = Role::Variable;
    let Some(&next) = text.get(start + 1) else {
        return start + 1;
    };

    if next == '{' {
        let mut index = start + 2;
        while index < text.len() && text[index] != '}' {
            roles[index] = Role::Variable;
            index += 1;
        }
        if index < text.len() {
            roles[index] = Role::Variable;
            index += 1;
        } else {
            for role in &mut roles[start..] {
                *role = Role::Error;
            }
        }
        return index;
    }

    if next == '(' {
        let mut depth = 0usize;
        let mut index = start + 1;
        while index < text.len() {
            roles[index] = Role::Variable;
            match text[index] {
                '(' => depth += 1,
                ')' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return index + 1;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        for role in &mut roles[start..] {
            *role = Role::Error;
        }
        return index;
    }

    if next.is_ascii_digit() {
        let mut index = start + 1;
        while index < text.len() && text[index].is_ascii_digit() {
            roles[index] = Role::Variable;
            index += 1;
        }
        return index;
    }

    if next.is_ascii_alphabetic() || next == '_' {
        let mut index = start + 1;
        while index < text.len() && (text[index].is_ascii_alphanumeric() || text[index] == '_') {
            roles[index] = Role::Variable;
            index += 1;
        }
        return index;
    }

    if matches!(next, '?' | '$' | '!' | '#' | '@' | '*' | '-') {
        roles[start + 1] = Role::Variable;
        return start + 2;
    }

    start + 1
}

fn command_starter(word: &str) -> bool {
    matches!(
        word,
        "if" | "then"
            | "else"
            | "elif"
            | "do"
            | "while"
            | "until"
            | "for"
            | "in"
            | "case"
            | "esac"
            | "fi"
            | "done"
            | "function"
            | "time"
            | "{"
            | "}"
            | "!"
    )
}

fn looks_like_path(word: &str) -> bool {
    word.contains('/') || word.starts_with('~') || word == "." || word == ".."
}

fn is_operator(char: char) -> bool {
    matches!(char, '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}')
}

#[cfg(test)]
mod tests {
    use super::{Role, classify_with};

    fn classify(text: &str) -> Vec<Role> {
        let chars: Vec<char> = text.chars().collect();
        classify_with(&chars, |word| match word {
            "cd" => Role::Builtin,
            "echo" | "cat" => Role::Command,
            _ => Role::Argument,
        })
    }

    fn at(roles: &[Role], text: &str, needle: usize) -> Role {
        let index = text.char_indices().nth(needle).map(|(i, _)| i).unwrap_or(0);
        roles[index]
    }

    #[test]
    fn commands_builtins_and_arguments() {
        let text = "cd /tmp";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 0), Role::Builtin);
        assert_eq!(at(&roles, text, 3), Role::Path);

        let text = "echo hello";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 0), Role::Command);
        assert_eq!(at(&roles, text, 5), Role::Argument);
    }

    #[test]
    fn strings_variables_and_operators() {
        let text = "echo \"$HOME\" 'plain' | cat";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 5), Role::String);
        assert_eq!(at(&roles, text, 6), Role::Variable);
        assert_eq!(at(&roles, text, 11), Role::String);
        assert_eq!(at(&roles, text, 16), Role::String);
        assert_eq!(at(&roles, text, 21), Role::Operator);
        assert_eq!(at(&roles, text, 23), Role::Command);
    }

    #[test]
    fn comments_redirects_and_paths() {
        let text = "cat < in > out # note";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 4), Role::Operator);
        assert_eq!(at(&roles, text, 6), Role::Path);
        assert_eq!(at(&roles, text, 11), Role::Path);
        assert_eq!(at(&roles, text, 15), Role::Comment);
    }

    #[test]
    fn unterminated_constructs_are_errors() {
        let text = "echo \"oops";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 5), Role::Error);

        let text = "echo ${HOME";
        let roles = classify(text);
        assert_eq!(at(&roles, text, 5), Role::Error);
    }
}
