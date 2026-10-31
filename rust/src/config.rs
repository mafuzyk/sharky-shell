use std::collections::HashMap;
use std::fmt;
use std::path::Path;

pub const INSTRUCTIONS: &[&str] = &["tide", "pearl", "current", "reef", "anchor", "coral", "wave"];

pub const ACTIONS: &[&str] = &[
    "history_search",
    "complete",
    "clear_screen",
    "accept_line",
    "abort",
    "backward_char",
    "forward_char",
    "backward_word",
    "forward_word",
    "beginning_of_line",
    "end_of_line",
    "delete_char",
    "backward_delete_char",
    "kill_line",
    "yank",
    "transpose_chars",
    "upcase_word",
    "downcase_word",
    "capitalize_word",
    "undo",
];

#[derive(Debug, Default, Clone)]
pub struct Config {
    pub settings: HashMap<String, String>,
    pub env: Vec<(String, String)>,
    pub paths: Vec<String>,
    pub aliases: HashMap<String, String>,
    pub keybinds: HashMap<String, String>,
    pub colors: HashMap<String, String>,
    pub styles: HashMap<String, String>,
}

#[derive(Debug)]
pub struct ConfigError {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: {}", self.file, self.line, self.message)
    }
}

pub fn load(dir: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::default();

    for name in ["config.sharky", "theme.sharky"] {
        let path = dir.join(name);
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = parse(&source, &path.display().to_string())?;
        merge(&mut config, parsed);
    }

    Ok(config)
}

pub fn parse(source: &str, file: &str) -> Result<Config, ConfigError> {
    let mut config = Config::default();

    for (index, line) in source.lines().enumerate() {
        let number = index + 1;
        let tokens = tokenize(line).map_err(|message| ConfigError {
            file: file.to_string(),
            line: number,
            message,
        })?;
        if tokens.is_empty() {
            continue;
        }

        apply(&mut config, &tokens).map_err(|message| ConfigError {
            file: file.to_string(),
            line: number,
            message,
        })?;
    }

    validate(&config, file)?;
    Ok(config)
}

pub fn parse_theme(source: &str, file: &str) -> Result<Config, ConfigError> {
    parse(source, file)
}

fn validate(config: &Config, file: &str) -> Result<(), ConfigError> {
    for (key, value) in &config.styles {
        let Some(reference) = value.strip_prefix('@') else {
            continue;
        };
        if !config.colors.contains_key(reference) {
            return Err(ConfigError {
                file: file.to_string(),
                line: 0,
                message: format!("{key} refers to undefined colour '@{reference}'"),
            });
        }
    }

    for (key, value) in &config.settings {
        match key.as_str() {
            "greeting" | "autosuggest" | "highlight" => {
                if !matches!(value.as_str(), "on" | "off") {
                    return Err(ConfigError {
                        file: file.to_string(),
                        line: 0,
                        message: format!("tide {key} expects on or off, found '{value}'"),
                    });
                }
            }
            "history" => {
                if value.parse::<usize>().is_err() {
                    return Err(ConfigError {
                        file: file.to_string(),
                        line: 0,
                        message: format!("tide history expects a number, found '{value}'"),
                    });
                }
            }
            _ => {}
        }
    }

    Ok(())
}

fn merge(into: &mut Config, from: Config) {
    into.settings.extend(from.settings);
    into.env.extend(from.env);
    into.paths.extend(from.paths);
    into.aliases.extend(from.aliases);
    into.keybinds.extend(from.keybinds);
    into.colors.extend(from.colors);
    into.styles.extend(from.styles);
}

fn apply(config: &mut Config, tokens: &[String]) -> Result<(), String> {
    let instruction = tokens[0].as_str();
    let rest = &tokens[1..];

    match instruction {
        "tide" => {
            let (key, value) = pair(rest, "tide")?;
            config.settings.insert(key, value);
        }
        "pearl" => {
            let (key, value) = pair(rest, "pearl")?;
            if key.is_empty() {
                return Err("pearl needs a variable name".to_string());
            }
            config.env.push((key, value));
        }
        "current" => {
            let [value] = rest else {
                return Err("current takes one directory".to_string());
            };
            config.paths.push(value.clone());
        }
        "reef" => {
            let (key, value) = pair(rest, "reef")?;
            config.aliases.insert(key, value);
        }
        "anchor" => {
            let (key, value) = pair(rest, "anchor")?;
            if !ACTIONS.contains(&value.as_str()) {
                return Err(format!("unknown action '{value}'"));
            }
            config.keybinds.insert(key, value);
        }
        "coral" => {
            let (key, value) = pair(rest, "coral")?;
            if !is_colour(&value) {
                return Err(format!("'{value}' is not a colour like #071923"));
            }
            config.colors.insert(key, value);
        }
        "wave" => {
            let (key, value) = pair(rest, "wave")?;
            config.styles.insert(key, value);
        }
        other => {
            return Err(format!(
                "unknown instruction '{other}', expected one of {}",
                INSTRUCTIONS.join(", ")
            ));
        }
    }

    Ok(())
}

fn pair(rest: &[String], instruction: &str) -> Result<(String, String), String> {
    match rest {
        [key, value] => Ok((key.clone(), value.clone())),
        [key, _value, extra @ ..] => Err(format!(
            "{instruction} {key} takes one value, found {}",
            extra.len() + 1
        )),
        [] => Err(format!("{instruction} needs a property and a value")),
        [key] => Err(format!("{instruction} {key} needs a value")),
    }
}

fn is_colour(value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else {
        return false;
    };
    hex.len() == 6 && hex.chars().all(|char| char.is_ascii_hexdigit())
}

fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut pos = 0;

    while pos < chars.len() {
        match chars[pos] {
            ' ' | '\t' | '\r' => pos += 1,
            '/' if chars.get(pos + 1) == Some(&'/') => break,
            '"' => {
                let (value, next) = quoted(&chars, pos)?;
                tokens.push(value);
                pos = next;
            }
            _ => {
                let start = pos;
                while pos < chars.len() {
                    if chars[pos].is_whitespace() {
                        break;
                    }
                    if chars[pos] == '/' && chars.get(pos + 1) == Some(&'/') {
                        break;
                    }
                    pos += 1;
                }
                tokens.push(chars[start..pos].iter().collect());
            }
        }
    }

    Ok(tokens)
}

fn quoted(chars: &[char], start: usize) -> Result<(String, usize), String> {
    let mut value = String::new();
    let mut pos = start + 1;

    while pos < chars.len() {
        match chars[pos] {
            '"' => return Ok((value, pos + 1)),
            '\\' => {
                let Some(&next) = chars.get(pos + 1) else {
                    return Err("trailing backslash".to_string());
                };
                value.push(match next {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                pos += 2;
            }
            char => {
                value.push(char);
                pos += 1;
            }
        }
    }

    Err("unclosed quote".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_example() {
        let source = r#"
// Sharky ~ settings
tide greeting off
tide autosuggest on
tide history 10000

pearl EDITOR "nvim"
current "~/.cargo/bin"
reef ll "ls -lah"
anchor ctrl+r history_search
"#;
        let config = parse(source, "config.sharky").unwrap();

        assert_eq!(config.settings["greeting"], "off");
        assert_eq!(config.settings["history"], "10000");
        assert_eq!(config.env, vec![("EDITOR".to_string(), "nvim".to_string())]);
        assert_eq!(config.paths, vec!["~/.cargo/bin".to_string()]);
        assert_eq!(config.aliases["ll"], "ls -lah");
        assert_eq!(config.keybinds["ctrl+r"], "history_search");
    }

    #[test]
    fn parses_the_theme_example() {
        let source = r#"
coral kelp #8CCFA8
coral sea #35B8C5
wall prompt.no "x"
wave prompt "{cwd} ❯ "
wave prompt.cwd @kelp
wave command @sea
"#;
        let source = source.replace("wall", "wave");
        let theme = parse_theme(&source, "theme.shaky").unwrap();

        assert_eq!(theme.colors["kelp"], "#8CCFA8");
        assert_eq!(theme.styles["prompt.cwd"], "@kelp");
    }

    #[test]
    fn rejects_an_unknown_instruction() {
        let error = parse("tidee greeting on", "config.sharky").unwrap_err();
        assert!(error.message.contains("unknown instruction 'tidee'"));
        assert_eq!(error.line, 1);
    }

    #[test]
    fn rejects_an_unknown_keybinding_action() {
        let error = parse("anchor ctrl+r teleport", "config.sharky").unwrap_err();
        assert!(error.message.contains("unknown action 'teleport'"));
    }

    #[test]
    fn rejects_an_invalid_colour() {
        let error = parse("coral kelp green", "theme.sharky").unwrap_err();
        assert!(error.message.contains("not a colour"));
    }

    #[test]
    fn rejects_a_reference_to_an_undefined_colour() {
        let source = "coral sea #35B8C5\nwave command @kelp";
        let error = parse_theme(source, "theme.sharky").unwrap_err();
        assert!(error.message.contains("undefined colour '@kelp'"));
    }

    #[test]
    fn rejects_an_unclosed_quote() {
        let error = parse(r#"pearl EDITOR "nvim"#, "config.sharky").unwrap_err();
        assert!(error.message.contains("unclosed quote"));
    }

    #[test]
    fn keeps_double_slashes_inside_quotes() {
        let config = parse(r#"reef web "firefox https://example.com""#, "config.sharky").unwrap();
        assert_eq!(config.aliases["web"], "firefox https://example.com");
    }

    #[test]
    fn rejects_a_boolean_that_is_not_on_or_off() {
        let error = parse("tide greeting yes", "config.sharky").unwrap_err();
        assert!(error.message.contains("expects on or off"));
    }

    #[test]
    fn rejects_a_non_numeric_history() {
        let error = parse("tide history many", "config.sharky").unwrap_err();
        assert!(error.message.contains("expects a number"));
    }
}
