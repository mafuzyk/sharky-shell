pub fn expand(line: &str, history: &[String]) -> Result<String, String> {
    if line.starts_with('^') {
        return quick_substitution(line, history);
    }

    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    let mut single = false;

    while index < chars.len() {
        let char = chars[index];

        if char == '\\' && !single {
            out.push(char);
            if let Some(next) = chars.get(index + 1) {
                out.push(*next);
                index += 2;
            } else {
                index += 1;
            }
            continue;
        }

        if char == '\'' {
            single = !single;
            out.push(char);
            index += 1;
            continue;
        }

        if char != '!' || single {
            out.push(char);
            index += 1;
            continue;
        }

        let Some(next) = chars.get(index + 1).copied() else {
            out.push('!');
            index += 1;
            continue;
        };

        if next.is_whitespace() || next == '=' || next == '(' {
            out.push('!');
            index += 1;
            continue;
        }

        let (replacement, consumed) = event(&chars[index..], history)?;
        out.push_str(&replacement);
        index += consumed;
    }

    Ok(out)
}

fn event(chars: &[char], history: &[String]) -> Result<(String, usize), String> {
    match chars.get(1).copied() {
        Some('!') => Ok((previous(history)?, 2)),
        Some('$') => Ok((argument(history, Argument::Last)?, 2)),
        Some('*') => Ok((argument(history, Argument::All)?, 2)),
        Some('^') => Ok((argument(history, Argument::First)?, 2)),
        Some('?') => {
            let mut end = 2;
            while end < chars.len() && chars[end] != '?' {
                end += 1;
            }
            let needle: String = chars[2..end].iter().collect();
            if needle.is_empty() {
                return Err("!?: event not found".to_string());
            }
            let entry = history
                .iter()
                .rev()
                .find(|entry| entry.contains(&needle))
                .cloned()
                .ok_or_else(|| format!("!?{needle}?: event not found"))?;
            let consumed = if end < chars.len() { end + 1 } else { end };
            Ok((entry, consumed))
        }
        Some('-') if chars.get(2).is_some_and(|char| char.is_ascii_digit()) => {
            let mut end = 2;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            let count: usize = chars[2..end].iter().collect::<String>().parse().unwrap_or(0);
            let entry = nth_back(history, count)?;
            Ok((entry, end))
        }
        Some(char) if char.is_ascii_digit() => {
            let mut end = 1;
            while end < chars.len() && chars[end].is_ascii_digit() {
                end += 1;
            }
            let number: usize = chars[1..end].iter().collect::<String>().parse().unwrap_or(0);
            if number == 0 || number > history.len() {
                let text: String = chars[..end].iter().collect();
                return Err(format!("{text}: event not found"));
            }
            Ok((history[number - 1].clone(), end))
        }
        Some(_) => {
            let mut end = 1;
            while end < chars.len() && is_event_char(chars[end]) {
                end += 1;
            }
            let prefix: String = chars[1..end].iter().collect();
            let entry = history
                .iter()
                .rev()
                .find(|entry| entry.starts_with(&prefix))
                .cloned()
                .ok_or_else(|| format!("!{prefix}: event not found"))?;
            Ok((entry, end))
        }
        None => Ok(("!".to_string(), 1)),
    }
}

fn is_event_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || matches!(char, '_' | '-' | '.' | '/' | ':')
}

#[derive(Clone, Copy)]
enum Argument {
    First,
    Last,
    All,
}

fn previous(history: &[String]) -> Result<String, String> {
    history
        .last()
        .cloned()
        .ok_or_else(|| "!!: event not found".to_string())
}

fn nth_back(history: &[String], count: usize) -> Result<String, String> {
    if count == 0 || count > history.len() {
        return Err(format!("!-{count}: event not found"));
    }
    Ok(history[history.len() - count].clone())
}

fn argument(history: &[String], which: Argument) -> Result<String, String> {
    let previous = history
        .last()
        .ok_or_else(|| "!$: event not found".to_string())?;
    let mut words = previous.split_whitespace();
    let _command = words.next();
    let args: Vec<&str> = words.collect();
    if args.is_empty() {
        return Err("!$: event not found".to_string());
    }
    Ok(match which {
        Argument::First => args[0].to_string(),
        Argument::Last => args[args.len() - 1].to_string(),
        Argument::All => args.join(" "),
    })
}

fn quick_substitution(line: &str, history: &[String]) -> Result<String, String> {
    let rest = &line[1..];
    let Some(first) = rest.find('^') else {
        return Ok(line.to_string());
    };
    let old = &rest[..first];
    let after = &rest[first + 1..];
    let Some(second) = after.find('^') else {
        return Ok(line.to_string());
    };
    let new = &after[..second];

    let previous = history
        .last()
        .ok_or_else(|| format!("{line}: event not found"))?;
    if old.is_empty() || !previous.contains(old) {
        return Err(format!("{line}: substitution failed"));
    }
    Ok(previous.replacen(old, new, 1))
}

#[cfg(test)]
mod tests {
    use super::expand;

    fn history() -> Vec<String> {
        vec![
            "echo first second".to_string(),
            "printf '%s' lastarg".to_string(),
            "echo alpha".to_string(),
        ]
    }

    #[test]
    fn expands_events() {
        let history = history();
        assert_eq!(expand("!!", &history).unwrap(), "echo alpha");
        assert_eq!(expand("!1", &history).unwrap(), "echo first second");
        assert_eq!(expand("!-2", &history).unwrap(), "printf '%s' lastarg");
        assert_eq!(expand("!echo a", &history).unwrap(), "echo alpha a");
        assert_eq!(expand("!?alpha?", &history).unwrap(), "echo alpha");
        assert_eq!(expand("echo !$", &history).unwrap(), "echo alpha");
        assert_eq!(expand("echo !*", &history).unwrap(), "echo alpha");
        assert_eq!(expand("echo !^", &history).unwrap(), "echo alpha");
    }

    #[test]
    fn quoting_suppresses_expansion() {
        let history = history();
        assert_eq!(expand("echo '!!'", &history).unwrap(), "echo '!!'");
        assert_eq!(expand("echo \\!\\!", &history).unwrap(), "echo \\!\\!");
        assert_eq!(expand("echo \"!!\"", &history).unwrap(), "echo \"echo alpha\"");
        assert_eq!(expand("echo a ! b", &history).unwrap(), "echo a ! b");
    }

    #[test]
    fn missing_events_are_errors() {
        let history = history();
        assert!(expand("!nope", &history).unwrap_err().contains("event not found"));
        assert!(expand("!9", &history).unwrap_err().contains("event not found"));
        assert!(expand("!!", &[]).unwrap_err().contains("event not found"));
    }

    #[test]
    fn quick_substitution_replaces_in_the_previous_command() {
        let history = history();
        assert_eq!(expand("^alpha^beta^", &history).unwrap(), "echo beta");
        assert!(expand("^missing^x^", &history).is_err());
    }
}
