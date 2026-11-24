use std::collections::HashMap;
use std::fmt;
use std::path::Path;

pub mod guide;
pub mod presets;
pub mod style;

pub const INSTRUCTIONS: &[&str] = &[
    "tide", "pearl", "current", "reef", "shoal", "anchor", "coral", "wave", "wake",
];

/// `tide` keys that take `on` or `off`.
pub const TIDE_FLAGS: &[&str] = &[
    "greeting",
    "autosuggest",
    "highlight",
    "starship",
    "profile_paths",
    "autocd",
    "rprompt",
];

/// `tide` key that takes a number.
pub const TIDE_NUMBER: &str = "history";

/// `tide` key that selects the prompt engine.
pub const TIDE_PROMPT: &str = "prompt";

/// Valid values for `tide prompt`.
pub const TIDE_PROMPT_ENGINES: &[&str] = &["native", "starship", "omp"];

/// `tide` key for an explicit Oh My Posh configuration path.
pub const TIDE_OMP_CONFIG: &str = "omp_config";

/// Named keys accepted by `anchor` without a modifier prefix.
pub const NAMED_KEYS: &[&str] = &[
    "tab", "shift-tab", "enter", "escape", "up", "down", "left", "right", "home", "end",
    "delete", "backspace",
];

/// Modifier prefixes accepted by `anchor`, each followed by one character.
pub const KEY_PREFIXES: &[&str] = &["ctrl-", "ctrl+", "c-", "c+", "alt-", "alt+", "m-", "m+"];

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
    "accept_suggestion",
    "fuzzy_files",
];

pub fn valid_key(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if NAMED_KEYS.contains(&lower.as_str()) {
        return true;
    }

    for prefix in KEY_PREFIXES {
        if let Some(rest) = lower.strip_prefix(*prefix) {
            return rest.chars().count() == 1 && !rest.chars().any(char::is_whitespace);
        }
    }

    name.chars().count() == 1
}

#[derive(Debug, Default, Clone)]
pub struct Config {
    pub settings: HashMap<String, String>,
    pub env: Vec<(String, String)>,
    pub paths: Vec<String>,
    pub aliases: HashMap<String, String>,
    pub abbreviations: HashMap<String, String>,
    pub wake: Vec<String>,
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

/// Resolve the configured prompt engine (`"native"`, `"starship"`, or `"omp"`).
pub fn prompt_engine(settings: &HashMap<String, String>) -> &'static str {
    match settings.get(TIDE_PROMPT).map(String::as_str) {
        Some("omp") => "omp",
        Some("starship") => "starship",
        Some("native") => "native",
        _ if settings.get("starship").map(String::as_str) == Some("on") => "starship",
        _ => "native",
    }
}

pub fn load(dir: &Path) -> Result<Config, ConfigError> {
    let mut config = Config::default();
    let mut last_file = dir.join("config.sharky").display().to_string();

    for name in ["config.sharky", "theme.sharky"] {
        let path = dir.join(name);
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let file = path.display().to_string();
        let parsed = parse_raw(&source, &file)?;
        last_file = file;
        merge(&mut config, parsed);
    }

    materialize_preset(&mut config);
    validate(&config, &last_file)?;
    Ok(config)
}

pub fn parse(source: &str, file: &str) -> Result<Config, ConfigError> {
    let mut config = parse_raw(source, file)?;
    materialize_preset(&mut config);
    validate(&config, file)?;
    Ok(config)
}

fn parse_raw(source: &str, file: &str) -> Result<Config, ConfigError> {
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

    Ok(config)
}

fn materialize_preset(config: &mut Config) {
    let Some(name) = config.styles.get("preset").map(String::as_str) else {
        return;
    };
    let Some(preset) = presets::find(name) else {
        return;
    };

    let mut colors = HashMap::with_capacity(preset.colors.len() + config.colors.len());
    for (key, value) in preset.colors {
        colors.insert((*key).to_string(), (*value).to_string());
    }
    colors.extend(std::mem::take(&mut config.colors));
    config.colors = colors;

    let mut styles = HashMap::with_capacity(preset.styles.len() + config.styles.len());
    for (key, value) in preset.styles {
        styles.insert((*key).to_string(), (*value).to_string());
    }
    styles.extend(std::mem::take(&mut config.styles));
    config.styles = styles;
}

pub fn parse_theme(source: &str, file: &str) -> Result<Config, ConfigError> {
    parse(source, file)
}

fn validate(config: &Config, file: &str) -> Result<(), ConfigError> {
    for (key, value) in &config.styles {
        match key.as_str() {
            "preset" | "prompt.symbol.glyph" | "prompt.error.glyph" | "prompt.os.glyph" => continue,
            "prompt" | "rprompt" => validate_template_styles(config, file, key, value)?,
            _ if style::looks_like_style(value) => {
                validate_style_spec(config, file, key, value)?;
            }
            _ => continue,
        }
    }

    for (key, value) in &config.settings {
        if TIDE_FLAGS.contains(&key.as_str()) {
            if !matches!(value.as_str(), "on" | "off") {
                return Err(ConfigError {
                    file: file.to_string(),
                    line: 0,
                    message: format!("tide {key} expects on or off, found '{value}'"),
                });
            }
        } else if key.as_str() == TIDE_NUMBER && value.parse::<usize>().is_err() {
            return Err(ConfigError {
                file: file.to_string(),
                line: 0,
                message: format!("tide {key} expects a number, found '{value}'"),
            });
        } else if key.as_str() == TIDE_PROMPT && !TIDE_PROMPT_ENGINES.contains(&value.as_str()) {
            return Err(ConfigError {
                file: file.to_string(),
                line: 0,
                message: format!(
                    "tide {key} expects {}, found '{value}'",
                    TIDE_PROMPT_ENGINES.join(", ")
                ),
            });
        } else if key.as_str() == TIDE_OMP_CONFIG && value.trim().is_empty() {
            return Err(ConfigError {
                file: file.to_string(),
                line: 0,
                message: format!("tide {key} expects a path"),
            });
        }
    }

    Ok(())
}

fn validate_style_spec(
    config: &Config,
    file: &str,
    key: &str,
    value: &str,
) -> Result<(), ConfigError> {
    let spec = style::StyleSpec::parse(value).map_err(|message| ConfigError {
        file: file.to_string(),
        line: 0,
        message: format!("{key}: {message}"),
    })?;
    for reference in spec.named_colors() {
        if !config.colors.contains_key(reference) {
            return Err(ConfigError {
                file: file.to_string(),
                line: 0,
                message: format!("{key} refers to undefined colour '@{reference}'"),
            });
        }
    }
    Ok(())
}

fn validate_template_styles(
    config: &Config,
    file: &str,
    key: &str,
    template: &str,
) -> Result<(), ConfigError> {
    let mut chars = template.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '\\' {
            let _ = chars.next();
            continue;
        }
        if ch != '{' {
            continue;
        }
        let start = idx + 1;
        let mut depth = 1usize;
        let mut end = None;
        while let Some((inner_idx, inner_ch)) = chars.next() {
            if inner_ch == '\\' {
                let _ = chars.next();
                continue;
            }
            if inner_ch == '{' {
                depth += 1;
            } else if inner_ch == '}' {
                depth -= 1;
                if depth == 0 {
                    end = Some(inner_idx);
                    break;
                }
            }
        }
        let Some(end_idx) = end else {
            break;
        };
        let expr = &template[start..end_idx];
        if let Some(colon) = find_top_level_colon(expr) {
            let prefix = &expr[..colon];
            if style::looks_like_style(prefix) {
                validate_style_spec(config, file, key, prefix)?;
            }
            let content = &expr[colon + 1..];
            validate_template_styles(config, file, key, content)?;
        }
    }
    Ok(())
}

fn find_top_level_colon(expr: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (idx, ch) in expr.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ':' if depth == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

fn merge(into: &mut Config, from: Config) {
    into.settings.extend(from.settings);
    into.env.extend(from.env);
    into.paths.extend(from.paths);
    into.aliases.extend(from.aliases);
    into.abbreviations.extend(from.abbreviations);
    into.wake.extend(from.wake);
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
        "shoal" => {
            let (key, value) = pair(rest, "shoal")?;
            config.abbreviations.insert(key, value);
        }
        "wake" => {
            let [value] = rest else {
                return Err("wake takes one command".to_string());
            };
            config.wake.push(value.clone());
        }
        "anchor" => {
            let (key, value) = pair(rest, "anchor")?;
            if !ACTIONS.contains(&value.as_str()) {
                return Err(format!("unknown action '{value}'"));
            }
            if !valid_key(&key) {
                return Err(format!("unknown key '{key}'"));
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
            if key == "preset" && presets::find(&value).is_none() {
                return Err(format!(
                    "unknown preset '{value}', expected one of {}",
                    presets::NAMES.join(", ")
                ));
            }
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

pub fn tokenize(line: &str) -> Result<Vec<String>, String> {
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
                if next == 'u' {
                    let (ch, next_pos) = unicode_escape(chars, pos + 2)?;
                    value.push(ch);
                    pos = next_pos;
                } else {
                    value.push(match next {
                        'n' => '\n',
                        't' => '\t',
                        other => other,
                    });
                    pos += 2;
                }
            }
            char => {
                value.push(char);
                pos += 1;
            }
        }
    }

    Err("unclosed quote".to_string())
}

fn unicode_escape(chars: &[char], start: usize) -> Result<(char, usize), String> {
    let (hex, next_pos) = if chars.get(start) == Some(&'{') {
        let Some(rel_end) = chars[start + 1..].iter().position(|&c| c == '}') else {
            return Err("unclosed unicode escape".to_string());
        };
        if rel_end == 0 || rel_end > 6 {
            return Err("unicode escape expects 1..6 hex digits".to_string());
        }
        (&chars[start + 1..start + 1 + rel_end], start + 2 + rel_end)
    } else {
        let Some(slice) = chars.get(start..start + 4) else {
            return Err("unicode escape expects 4 hex digits".to_string());
        };
        (slice, start + 4)
    };

    let digits: String = hex.iter().collect();
    let code = u32::from_str_radix(&digits, 16)
        .map_err(|_| format!("invalid unicode escape '\\u{digits}'"))?;
    let ch = char::from_u32(code)
        .ok_or_else(|| format!("invalid unicode scalar U+{code:04X}"))?;
    Ok((ch, next_pos))
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
    fn parses_shoal_abbreviations() {
        let config = parse("shoal gco \"git checkout\"\n", "config.sharky").unwrap();
        assert_eq!(config.abbreviations["gco"], "git checkout");

        let error = parse("shoal\n", "config.sharky").unwrap_err();
        assert!(error.message.contains("property and a value"), "{error}");
    }

    #[test]
    fn parses_wake_commands() {
        let config = parse("wake \"fastfetch --logo none\"\n", "config.sharky").unwrap();
        assert_eq!(config.wake, vec!["fastfetch --logo none".to_string()]);

        let error = parse("wake\n", "config.sharky").unwrap_err();
        assert!(error.message.contains("one command"), "{error}");
    }

    #[test]
    fn keeps_koral_theme_keys() {
        let source = "coral abyss #071923\nwave terminal.background @abyss\nwave tab.active @abyss\n";
        let theme = parse_theme(source, "theme.sharky").unwrap();
        assert_eq!(theme.styles["terminal.background"], "@abyss");
        assert_eq!(theme.styles["tab.active"], "@abyss");
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
    fn accepts_bindable_key_names() {
        for key in [
            "ctrl-x",
            "c-x",
            "alt-x",
            "m-x",
            "tab",
            "shift-tab",
            "enter",
            "escape",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "delete",
            "backspace",
            "x",
        ] {
            let source = format!("anchor {key} kill_line\n");
            assert!(parse(&source, "config.sharky").is_ok(), "{key} rejected");
        }

        let source = "anchor ctrl+t accept_suggestion\n";
        assert!(parse(source, "config.sharky").is_ok());
    }

    #[test]
    fn rejects_an_unknown_key_name_with_its_line() {
        let error = parse("anchor ctrl+nope kill_line\n", "config.sharky").unwrap_err();
        assert!(error.message.contains("unknown key 'ctrl+nope'"), "{error}");
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
    fn shipped_examples_are_valid() {
        for name in ["examples/config.sharky", "examples/theme.sharky"] {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(name);
            let source = std::fs::read_to_string(&path).expect("example must exist");
            parse(&source, name).unwrap_or_else(|error| panic!("{error}"));
        }
    }

    #[test]
    fn rejects_a_non_numeric_history() {
        let error = parse("tide history many", "config.sharky").unwrap_err();
        assert!(error.message.contains("expects a number"));
    }

    #[test]
    fn all_presets_materialize_and_validate() {
        for name in presets::NAMES {
            let source = format!("wave preset {name}\n");
            let theme = parse_theme(&source, "theme.sharky")
                .unwrap_or_else(|err| panic!("preset {name} failed: {err}"));
            assert!(theme.styles.contains_key("prompt"), "missing prompt in {name}");
            assert!(
                theme.styles.contains_key("prompt.symbol.glyph"),
                "missing glyph in {name}"
            );
        }
    }

    #[test]
    fn rejects_unknown_preset_with_line_number() {
        let error = parse_theme("\nwave preset bogus\n", "theme.sharky").unwrap_err();
        assert_eq!(error.line, 2);
        assert!(error.message.contains("unknown preset 'bogus'"), "{error}");
    }

    #[test]
    fn user_declarations_override_preset_regardless_of_order() {
        let before = parse_theme(
            "coral user #123456\nwave prompt.symbol.glyph \"$\"\nwave preset neolambda\n",
            "theme.sharky",
        )
        .unwrap();
        assert_eq!(before.colors["user"], "#123456");
        assert_eq!(before.styles["prompt.symbol.glyph"], "$");
        assert_eq!(before.colors["cwd"], "#87FF00");

        let after = parse_theme(
            "wave preset neolambda\ncoral user #654321\nwave prompt.symbol.glyph \">\"\n",
            "theme.sharky",
        )
        .unwrap();
        assert_eq!(after.colors["user"], "#654321");
        assert_eq!(after.styles["prompt.symbol.glyph"], ">");
    }

    #[test]
    fn load_merges_config_and_theme_before_preset_validation() {
        let dir = std::env::temp_dir().join(format!("sharky-cfg-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.sharky"),
            "tide prompt native\nwave prompt.cwd \"@user bold\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("theme.sharky"), "wave preset neolambda\n").unwrap();

        let cfg = load(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(cfg.styles["prompt.cwd"], "@user bold");
        assert_eq!(cfg.colors["user"], "#DF005F");
    }

    #[test]
    fn validates_rich_styles_and_inline_prompt_spans() {
        let ok = parse_theme(
            "coral ink #071923\ncoral sea #35B8C5\nwave prompt \"{@ink on @sea bold:{cwd}} \"\nwave prompt.error \"#FF5F5F bold\"\nwave selection \"@ink on @sea\"\n",
            "theme.sharky",
        )
        .unwrap();
        assert_eq!(ok.styles["selection"], "@ink on @sea");

        let bad_mod = parse_theme("coral sea #35B8C5\nwave prompt.cwd \"@sea bld\"\n", "theme.sharky")
            .unwrap_err();
        assert!(bad_mod.message.contains("unknown style token 'bld'"), "{bad_mod}");

        let bad_inline =
            parse_theme("wave prompt \"{@missing:hi} \"\n", "theme.sharky").unwrap_err();
        assert!(
            bad_inline.message.contains("undefined colour '@missing'"),
            "{bad_inline}"
        );
    }

    #[test]
    fn validates_prompt_engine_and_omp_config() {
        let cfg = parse(
            "tide prompt omp\ntide omp_config \"~/theme.omp.json\"\ntide rprompt on\n",
            "config.sharky",
        )
        .unwrap();
        assert_eq!(prompt_engine(&cfg.settings), "omp");
        assert_eq!(cfg.settings["omp_config"], "~/theme.omp.json");
        assert_eq!(cfg.settings["rprompt"], "on");

        let mut legacy = HashMap::new();
        legacy.insert("starship".to_string(), "on".to_string());
        assert_eq!(prompt_engine(&legacy), "starship");

        let mut overridden = legacy.clone();
        overridden.insert("prompt".to_string(), "omp".to_string());
        assert_eq!(prompt_engine(&overridden), "omp");

        let err = parse("tide prompt fish\n", "config.sharky").unwrap_err();
        assert!(err.message.contains("native, starship, omp"), "{err}");

        let err = parse("tide omp_config \"   \"\n", "config.sharky").unwrap_err();
        assert!(err.message.contains("expects a path"), "{err}");
    }

    #[test]
    fn decodes_unicode_escapes_and_os_glyph_in_quotes() {
        let cfg = parse_theme(
            "wave prompt \"\\uE0B6{os_icon}\\u{E0B0} \"\nwave prompt.os.glyph \"\\u{F17C}\"\n",
            "theme.sharky",
        )
        .unwrap();
        assert_eq!(cfg.styles["prompt"], "\u{E0B6}{os_icon}\u{E0B0} ");
        assert_eq!(cfg.styles["prompt.os.glyph"], "\u{F17C}");

        assert!(parse("wave prompt \"\\uZZZZ\"", "theme.sharky").is_err());
        assert!(parse("wave prompt \"\\u{110000}\"", "theme.sharky").is_err());
    }
}
