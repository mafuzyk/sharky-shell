//! Static human-readable guide data for the declarative config language.
//!
//! This module only produces text. It parses no files and wires no CLI.

use crate::{
    ACTIONS, INSTRUCTIONS, KEY_PREFIXES, NAMED_KEYS, TIDE_CURSOR, TIDE_CURSOR_STYLES, TIDE_FLAGS,
    TIDE_NUMBER, TIDE_OMP_CONFIG, TIDE_PROMPT, TIDE_PROMPT_ENGINES, presets,
};

/// Stable topic id with a short human-readable title.
pub const INDEX: &[(&str, &str)] = &[
    ("index", "Configuration guide"),
    ("directives", "Parser directives"),
    ("set", "set — shell preferences and defaults"),
    ("env", "env — environment variable"),
    ("path", "path — PATH entry"),
    ("alias", "alias — command alias"),
    ("abbr", "abbr — abbreviation"),
    ("bind", "bind — key binding"),
    ("color", "color — colour definition"),
    ("style", "style — prompt or style"),
    ("startup", "startup — interactive startup command"),
    ("include", "include — modular config inclusion"),
    ("set history", "set history"),
    ("files", "config.sharky, conf.d/*.sharky and theme.sharky"),
    ("theme", "Palette, prompt and styles"),
    ("presets", "Built-in prompt presets"),
    ("prompt", "Prompt engines, templates, transient and rprompt"),
    ("overwrite", "Overwrite target styling"),
];

/// Plain-text index: one `id  title` line per topic, for `sharky config explain`.
pub fn index_text() -> String {
    let width = INDEX.iter().map(|(id, _)| id.len()).max().unwrap_or(0);
    let mut out = String::from("Configuration guide topics (sharky config explain TOPIC):\n");
    for (id, title) in INDEX {
        out.push_str(&format!("{id:<width$}  {title}\n"));
    }
    out
}

/// Human text for a topic id from [`INDEX`], or legacy aliases.
pub fn topic(id: &str) -> Option<String> {
    match id {
        "index" => Some(
            "Sharky reads declarative configuration at startup: config.sharky, conf.d/*.sharky, \
             and theme.sharky. They are data, never shell code: one statement per line, // \
             comments, optional double quotes, and no =, braces or semicolons. A line reads \
             `instruction property value`. Look up a topic by its id: directives, set, \
             set history, bind, include, files, theme, overwrite. Legacy directives \
             (tide, pearl, current, reef, shoal, anchor, coral, wave, wake) remain accepted as \
             deprecated aliases. Invalid input is reported as `file:line: message` and the \
             shell continues with defaults."
                .to_string(),
        ),
        "directives" => {
            let meanings = [
                ("set", "shell preference (legacy alias: tide)"),
                ("env", "environment variable (legacy alias: pearl)"),
                ("path", "prepend a directory to PATH (legacy alias: current)"),
                ("alias", "alias (legacy alias: reef)"),
                ("abbr", "abbreviation expanded on space or Enter (legacy alias: shoal)"),
                ("startup", "command run at interactive startup (legacy alias: wake)"),
                ("bind", "key binding (legacy alias: anchor)"),
                ("color", "colour definition (legacy alias: coral)"),
                ("style", "prompt template, preset or style (legacy alias: wave)"),
                ("include", "load another .sharky config file inline"),
            ];
            let list = INSTRUCTIONS
                .iter()
                .map(|name| {
                    let meaning = meanings
                        .iter()
                        .find(|(key, _)| key == name)
                        .map_or("directive", |(_, text)| *text);
                    format!("`{name}` — {meaning}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!(
                "Each line starts with one parser directive. The accepted instructions are:\n\
                 {list}\n\
                 color and style normally live in theme.sharky; the rest live in config.sharky \
                 or conf.d/*.sharky. Legacy maritime names are deprecated and will be removed \
                 in a future release."
            ))
        }
        "set" | "tide" => {
            let defaults = [
                ("greeting", "off"),
                ("autosuggest", "on"),
                ("highlight", "on"),
                ("starship", "off"),
                ("profile_paths", "on"),
                ("autocd", "on"),
                ("rprompt", "off"),
                ("vi", "off"),
                ("transient_prompt", "off"),
            ];
            let flags = TIDE_FLAGS
                .iter()
                .map(|key| {
                    let default = defaults
                        .iter()
                        .find(|(name, _)| name == key)
                        .map_or("unset", |(_, value)| *value);
                    format!("`{key}` on or off (default {default})")
                })
                .collect::<Vec<_>>()
                .join("; ");
            Some(format!(
                "`set KEY VALUE` (legacy alias `tide`) sets a shell preference.\n\
                 Flags: {flags}.\n\
                 `{prompt}` selects the prompt engine ({engines}; default native).\n\
                 `{cursor}` sets cursor shape ({cursors}; default terminal default).\n\
                 `{omp_cfg}` sets an optional Oh My Posh theme file path (supports ~/ and defaults to POSH_THEME).\n\
                 `{key}` takes a number of history entries to keep (default 1000).\n\
                 Invalid values are rejected with `file:line: message`.",
                prompt = TIDE_PROMPT,
                engines = TIDE_PROMPT_ENGINES.join(", "),
                cursor = TIDE_CURSOR,
                cursors = TIDE_CURSOR_STYLES.join(", "),
                omp_cfg = TIDE_OMP_CONFIG,
                key = TIDE_NUMBER
            ))
        }
        "env" | "pearl" => Some(
            "`env NAME value` (legacy alias `pearl`) sets an environment variable for the shell \
             and its children. NAME is a bare word and the value may be double quoted. A line \
             with no name is rejected; several env lines set several variables."
                .to_string(),
        ),
        "path" | "current" => Some(
            "`path directory` (legacy alias `current`) prepends one directory to PATH. Quote a \
             directory that contains spaces. Multiple path lines are applied in order, so the \
             last directory listed ends up first in PATH."
                .to_string(),
        ),
        "alias" | "reef" => Some(
            "`alias name command` (legacy alias `reef`) defines an alias. The command may be \
             double quoted when it contains spaces. Aliases expand when the name is used as a \
             command."
                .to_string(),
        ),
        "abbr" | "shoal" => Some(
            "`abbr abbreviation expansion` (legacy alias `shoal`) defines an abbreviation for the \
             line editor: typing the abbreviation and then pressing space or Enter expands it. \
             Quote an expansion that contains spaces. Abbreviations affect typed input, not scripts."
                .to_string(),
        ),
        "startup" | "wake" => Some(
            "`startup command` (legacy alias `wake`) registers one command to run at interactive \
             startup, before the first prompt. Quote a command that contains spaces. Multiple \
             startup lines append and all run; a non-interactive shell does not run them."
                .to_string(),
        ),
        "include" => Some(
            "`include path` loads another declarative .sharky file inline. Relative paths resolve \
             from the containing file's directory and ~/ expands to HOME. Cycles and depth over 16 \
             are rejected."
                .to_string(),
        ),
        "color" | "coral" => Some(
            "`color name #RRGGBB` (legacy alias `coral`) defines a colour, normally in theme.sharky. \
             The value must be six hexadecimal digits after #; the name can then be used as @name \
             in a style. Defining a colour twice keeps the last value."
                .to_string(),
        ),
        "style" | "wave" => Some(
            "`style key value` (legacy alias `wave`) sets a prompt template, preset or style, \
             normally in theme.sharky.\n\
             `style preset NAME` applies a built-in preset as a base layer (see `presets`).\n\
             Styles accept @name or #RRGGBB, optional `on @bg`/`on #RRGGBB`, and attributes \
             `bold`, `dim`, `italic`, `underline`, `reverse`/`invert` (for example \
             `style prompt.git \"@ink on @kelp bold\"`).\n\
             `style prompt`, `style rprompt`, `style prompt.transient` and \
             `style prompt.continuation` support \\uXXXX/\\u{X} Unicode escapes, {cwd}, \
             {cwd_short}, {cwd_full}, {cwd_home}, {git}, {git_dirty}, {duration}, {duration_ms}, \
             {symbol}, {status}, {error_status}, {user}, {host}, {os_icon}, {shell}, {mem}, \
             {venv}, {nix}, {ssh}, {jobs}, {time}, {env:VAR} and {fill}, conditional [...] groups, \
             inline {style:text} spans, and `style prompt.symbol.glyph` / \
             `style prompt.error.glyph` / `style prompt.os.glyph`. `style overwrite` is the \
             optional style for redirect targets."
                .to_string(),
        ),
        "set history" | "tide history" => Some(format!(
            "`set {key} N` (legacy alias `tide {key}`) sets how many command lines a session keeps \
             in memory (default 1000). N must be a non-negative integer; an unset key or 0 falls \
             back to 1000.\n\
             A non-numeric value is rejected when config.sharky is parsed: `set {key} many` \
             reports `set {key} expects a number, found 'many'` and the shell keeps its \
             defaults.\n\
             This limit bounds only one session's view; it never trims the shared file. \
             Auto-clean bounds the file itself with a vacuum that caps it at 262144 records by \
             file position, independently of every session's memory limit.\n\
             Entries are stored in one shared file at $XDG_DATA_HOME/sharky/history, or \
             ~/.local/share/sharky/history when that is unset, unless SHARKY_HISTORY overrides it.\n\
             New records use a one-line, TAB-separated v2 grammar: \
             `h2 <when-us> <cwd> <npaths> <paths...> <cmd> <hits>`, with escapes inside each field. \
             Legacy v1 lines coexist; migration is lazy, so nothing is rewritten on load.",
            key = TIDE_NUMBER
        )),
        "bind" | "anchor" => Some(format!(
            "`bind KEY ACTION` (legacy alias `anchor`) binds a key to a reader action.\n\
             Actions: {}.\n\
             Keys: named keys {}; modifier prefixes {} followed by one character; or a single \
             character. Unknown actions and keys are rejected with `file:line: message`.",
            ACTIONS.join(", "),
            NAMED_KEYS.join(", "),
            KEY_PREFIXES.join(", "),
        )),
        "files" => Some(
            "Sharky reads $SHARKY_CONFIG, or ${XDG_CONFIG_HOME:-~/.config}/sharky, at startup. \
             It parses config.sharky first, then conf.d/*.sharky in alphabetical order, then \
             theme.sharky, and merges them: a later value overwrites an earlier one for the same \
             key, while the list-like instructions env, path and startup append. Inside one file \
             a later line for the same key wins as well. `include` pulls extra files inline. \
             Invalid input is reported as `file:line: message`; the shell continues with defaults."
                .to_string(),
        ),
        "theme" => Some(
            "theme.sharky defines the palette and the prompt. `style preset NAME` loads a built-in \
             preset (catppuccin, tokyonight, sharky, neolambda, pure, robbyrussell, powerline) \
             before your own overrides. `color NAME #RRGGBB` defines a colour; `style KEY VALUE` \
             sets a prompt template, right prompt (`rprompt`), transient prompt (`prompt.transient`), \
             continuation prompt (`prompt.continuation`), symbol glyph (`prompt.symbol.glyph`) or \
             style (`@name`, `#RRGGBB`, `on @bg`, `bold`/`dim`/`italic`/`underline`/`reverse`). \
             Prompt placeholders are {cwd}, {cwd_short}, {cwd_full}, {cwd_home}, {git}, \
             {git_dirty}, {duration}, {duration_ms}, {symbol}, {status}, {error_status}, {user}, \
             {host}, {os_icon}, {shell}, {mem}, {venv}, {nix}, {ssh}, {jobs}, {time}, {env:VAR} \
             and {fill}. Set NO_COLOR=1 to keep the prompt plain."
                .to_string(),
        ),
        "presets" => {
            let list = presets::PRESETS
                .iter()
                .map(|preset| format!("`{}` — {}", preset.name, preset.description))
                .collect::<Vec<_>>()
                .join("\n");
            Some(format!(
                "Built-in presets activated via `style preset NAME` in theme.sharky or config.sharky:\n\
                 {list}\n\
                 User `color` and `style` rules always override preset defaults regardless of order."
            ))
        }
        "prompt" => Some(
            "Sharky supports native declarative templates and external prompt binaries:\n\
             - Native (default, `set prompt native`): `style prompt`, `style rprompt`, \
             `style prompt.transient` (with `set transient_prompt on`), `style prompt.continuation`, \
             `style preset NAME`, conditional `[...]` groups (shown when an inner placeholder is \
             non-empty), inline `{style:text}` spans, `{env:VAR}`, and `style prompt.symbol.glyph \"λ\"`.\n\
             - Starship (`set prompt starship` or `set starship on`): runs `starship prompt`.\n\
             - Oh My Posh (`set prompt omp`, optional `set omp_config \"~/theme.omp.json\"`): runs \
             `oh-my-posh print primary --shell=universal --escape=false`.\n\
             Enable external right prompts with `set rprompt on`; native `style rprompt` is active \
             whenever defined unless `set rprompt off` is set."
                .to_string(),
        ),
        "overwrite" => Some(
            "`style overwrite @colour` (legacy `wave overwrite`) is the optional style for files \
             a redirect would truncate. It paints the literal target of >, 2> and &> when that \
             path already exists. Only unquoted static paths are matched: quotes, variables, \
             tilde and other expansions stay plain, and no filesystem check runs when highlighting \
             or colour is off. Leave the style out for no overwrite colour."
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_indexed_topic_has_text() {
        for (id, title) in INDEX {
            assert!(!title.is_empty(), "{id} has no title");
            let text = topic(id).unwrap_or_else(|| panic!("no text for {id}"));
            assert!(!text.trim().is_empty(), "{id} has no text");
        }
    }

    #[test]
    fn index_text_is_plain_text_and_lists_every_topic() {
        let index = index_text();
        assert!(!index.contains('{'), "index must not be JSON: {index}");
        for (id, title) in INDEX {
            assert!(index.contains(id), "index missing id {id}");
            assert!(index.contains(title), "index missing title {title}");
        }
    }

    #[test]
    fn every_recognized_directive_has_a_discoverable_topic() {
        for name in INSTRUCTIONS {
            assert!(
                INDEX.iter().any(|(id, _)| id == name),
                "{name} is missing from the index"
            );
            assert!(topic(name).is_some(), "{name} has no topic text");
        }
    }

    #[test]
    fn directives_cover_parser_instructions() {
        let text = topic("directives").unwrap();
        for name in INSTRUCTIONS {
            assert!(text.contains(&format!("`{name}`")), "missing {name}");
        }
    }

    #[test]
    fn bind_covers_parser_actions_and_keys() {
        let text = topic("bind").unwrap();
        for action in ACTIONS {
            assert!(text.contains(*action), "missing action {action}");
        }
        for key in NAMED_KEYS {
            assert!(text.contains(*key), "missing key {key}");
        }
        for prefix in KEY_PREFIXES {
            assert!(text.contains(*prefix), "missing prefix {prefix}");
        }
        assert_eq!(topic("anchor").unwrap(), text);
    }

    #[test]
    fn set_covers_parser_flags_and_defaults() {
        let text = topic("set").unwrap();
        for key in TIDE_FLAGS {
            assert!(text.contains(&format!("`{key}`")), "missing {key}");
        }
        assert!(text.contains(TIDE_NUMBER));
        assert!(text.contains(TIDE_CURSOR));
        assert!(text.contains("default 1000"));
        assert_eq!(topic("tide").unwrap(), text);
    }

    #[test]
    fn set_history_is_a_targeted_topic() {
        let text = topic("set history").unwrap();
        assert!(text.contains("set history"));
        assert!(text.contains("1000"));
        assert!(text.contains("expects a number"));
        assert_eq!(topic("tide history").unwrap(), text);
    }

    #[test]
    fn files_topic_documents_merge_and_overwrite() {
        let text = topic("files").unwrap();
        assert!(text.contains("config.sharky"));
        assert!(text.contains("conf.d/*.sharky"));
        assert!(text.contains("theme.sharky"));
        assert!(text.contains("overwrites"));
        assert!(text.contains("append"));
    }

    #[test]
    fn overwrite_topic_documents_the_current_style() {
        let text = topic("overwrite").unwrap();
        assert!(text.contains("style overwrite"));
        assert!(text.contains("@colour"));
        assert!(text.contains("2>"));
        assert!(text.contains("&>"));
        assert!(text.contains("unquoted static paths"));
    }

    #[test]
    fn unknown_topic_has_no_text() {
        assert!(topic("not-a-topic").is_none());
    }

    #[test]
    fn presets_and_prompt_topics_cover_new_capabilities() {
        let presets_text = topic("presets").unwrap();
        for name in presets::NAMES {
            assert!(presets_text.contains(name), "missing preset {name}");
        }
        let prompt_text = topic("prompt").unwrap();
        assert!(prompt_text.contains("oh-my-posh"));
        assert!(prompt_text.contains("starship"));
        assert!(prompt_text.contains("omp_config"));
        assert!(prompt_text.contains("rprompt"));
    }
}
