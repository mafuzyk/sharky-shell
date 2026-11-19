//! Static human-readable guide data for the declarative config language.
//!
//! This module only produces text. It parses no files and wires no CLI.

use crate::{ACTIONS, INSTRUCTIONS, KEY_PREFIXES, NAMED_KEYS, TIDE_FLAGS, TIDE_NUMBER};

/// Stable topic id with a short human-readable title.
pub const INDEX: &[(&str, &str)] = &[
    ("index", "Configuration guide"),
    ("directives", "Parser directives"),
    ("tide", "tide — shell preferences and defaults"),
    ("pearl", "pearl — environment variable"),
    ("current", "current — PATH entry"),
    ("reef", "reef — alias"),
    ("shoal", "shoal — abbreviation"),
    ("anchor", "anchor — key binding"),
    ("coral", "coral — colour definition"),
    ("wave", "wave — prompt or style"),
    ("wake", "wake — startup command"),
    ("tide history", "tide history"),
    ("files", "config.sharky and theme.sharky"),
    ("theme", "Palette, prompt and styles"),
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

/// Human text for a topic id from [`INDEX`], or `None` for an unknown id.
pub fn topic(id: &str) -> Option<String> {
    match id {
        "index" => Some(
            "Sharky reads two declarative files at startup: config.sharky for settings and \
             theme.sharky for colours and styles. They are data, never shell code: one \
             statement per line, // comments, optional double quotes, and no =, braces or \
             semicolons. A line reads `instruction property value`. Look up a topic by its \
             id: directives, tide, tide history, anchor, files, theme, overwrite. Invalid \
             input is reported as `file:line: message` and the shell continues with defaults."
                .to_string(),
        ),
        "directives" => {
            let meanings = [
                ("tide", "shell preference"),
                ("pearl", "environment variable"),
                ("current", "prepend a directory to PATH"),
                ("reef", "alias"),
                ("shoal", "abbreviation expanded on space or Enter"),
                ("wake", "command run at interactive startup"),
                ("anchor", "key binding"),
                ("coral", "colour definition"),
                ("wave", "prompt template or style"),
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
                 coral and wave normally live in theme.sharky; the rest live in config.sharky. \
                 An unknown instruction, the wrong number of values or an unclosed quote is \
                 rejected instead of being executed."
            ))
        }
        "tide" => {
            let defaults = [
                ("greeting", "off"),
                ("autosuggest", "on"),
                ("highlight", "on"),
                ("starship", "off"),
                ("profile_paths", "on"),
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
                "`tide KEY VALUE` sets a shell preference.\n\
                 Flags: {flags}.\n\
                 `{key}` takes a number of history entries to keep (default 1000).\n\
                 A flag that is not on or off, or a non-numeric history value, is rejected \
                 with `file:line: message`.",
                key = TIDE_NUMBER
            ))
        }
        "pearl" => Some(
            "`pearl NAME value` sets an environment variable for the shell and its children. \
             NAME is a bare word and the value may be double quoted. A line with no name is \
             rejected; several pearl lines set several variables."
                .to_string(),
        ),
        "current" => Some(
            "`current directory` prepends one directory to PATH. Quote a directory that contains \
             spaces. Multiple current lines are applied in order, so the last directory listed \
             ends up first in PATH."
                .to_string(),
        ),
        "reef" => Some(
            "`reef name command` defines an alias. The command may be double quoted when it \
             contains spaces. Aliases expand when the name is used as a command."
                .to_string(),
        ),
        "shoal" => Some(
            "`shoal abbreviation expansion` defines an abbreviation for the line editor: typing \
             the abbreviation and then pressing space or Enter expands it. Quote an expansion \
             that contains spaces. Abbreviations affect typed input, not scripts."
                .to_string(),
        ),
        "wake" => Some(
            "`wake command` registers one command to run at interactive startup, before the first \
             prompt. Quote a command that contains spaces. Multiple wake lines append and all run; \
             a non-interactive shell does not run them."
                .to_string(),
        ),
        "coral" => Some(
            "`coral name #RRGGBB` defines a colour, normally in theme.sharky. The value must be six \
             hexadecimal digits after #; the name can then be used as @name in a wave style. \
             Defining a colour twice keeps the last value."
                .to_string(),
        ),
        "wave" => Some(
            "`wave key value` sets a prompt template or a style, normally in theme.sharky. Styles \
             reference a colour with @name and an undefined reference is an error. Prompt \
             placeholders are {cwd}, {symbol}, {status}, {user} and {host}. `wave overwrite` is the \
             optional style for redirect targets."
                .to_string(),
        ),
        "tide history" => Some(format!(
            "`tide {key} N` sets how many command lines Sharky keeps. N must be a non-negative \
             integer; an unset key or 0 falls back to 1000.\n\
             A non-numeric value is rejected when config.sharky is parsed: `tide {key} many` \
             reports `tide {key} expects a number, found 'many'` and the shell keeps its \
             defaults.\n\
             Entries are stored in one shared file at $XDG_DATA_HOME/sharky/history, or \
             ~/.local/share/sharky/history when that is unset, unless SHARKY_HISTORY overrides it.",
            key = TIDE_NUMBER
        )),
        "anchor" => Some(format!(
            "`anchor KEY ACTION` binds a key to a reader action.\n\
             Actions: {}.\n\
             Keys: named keys {}; modifier prefixes {} followed by one character; or a single \
             character. Unknown actions and keys are rejected with `file:line: message`.",
            ACTIONS.join(", "),
            NAMED_KEYS.join(", "),
            KEY_PREFIXES.join(", "),
        )),
        "files" => Some(
            "Sharky reads $SHARKY_CONFIG, or ${XDG_CONFIG_HOME:-~/.config}/sharky, at startup. \
             It parses config.sharky first, then theme.sharky, and merges them: a later value \
             overwrites an earlier one for the same key, while the list-like instructions \
             pearl, current and wake append. Inside one file a later line for the same key \
             wins as well. Both files are data only, so a typo cannot execute code. Invalid \
             input is reported as `file:line: message`; the shell continues with defaults."
                .to_string(),
        ),
        "theme" => Some(
            "theme.sharky defines the palette and the prompt. `coral NAME #RRGGBB` defines a \
             colour; `wave KEY VALUE` sets a prompt template or a style. Styles reference a \
             colour with @name, and a reference to a colour that was never defined is an error. \
             Prompt placeholders are {cwd}, {symbol}, {status}, {user} and {host}. Set NO_COLOR=1 \
             to keep the prompt plain."
                .to_string(),
        ),
        "overwrite" => Some(
            "`wave overwrite @colour` is the current optional style for files a redirect would \
             truncate. It paints the literal target of >, 2> and &> when that path already exists. \
             Only unquoted static paths are matched: quotes, variables, tilde and other expansions \
             stay plain, and no filesystem check runs when highlighting or colour is off. Leave \
             the style out for no overwrite colour."
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
    fn anchor_covers_parser_actions_and_keys() {
        let text = topic("anchor").unwrap();
        for action in ACTIONS {
            assert!(text.contains(*action), "missing action {action}");
        }
        for key in NAMED_KEYS {
            assert!(text.contains(*key), "missing key {key}");
        }
        for prefix in KEY_PREFIXES {
            assert!(text.contains(*prefix), "missing prefix {prefix}");
        }
    }

    #[test]
    fn tide_covers_parser_flags_and_defaults() {
        let text = topic("tide").unwrap();
        for key in TIDE_FLAGS {
            assert!(text.contains(&format!("`{key}`")), "missing {key}");
        }
        assert!(text.contains(TIDE_NUMBER));
        assert!(text.contains("default 1000"));
    }

    #[test]
    fn tide_history_is_a_targeted_topic() {
        let text = topic("tide history").unwrap();
        assert!(text.contains("tide history"));
        assert!(text.contains("1000"));
        assert!(text.contains("expects a number"));
    }

    #[test]
    fn files_topic_documents_merge_and_overwrite() {
        let text = topic("files").unwrap();
        assert!(text.contains("config.sharky"));
        assert!(text.contains("theme.sharky"));
        assert!(text.contains("overwrites"));
        assert!(text.contains("append"));
    }

    #[test]
    fn overwrite_topic_documents_the_current_style() {
        let text = topic("overwrite").unwrap();
        assert!(text.contains("wave overwrite"));
        assert!(text.contains("@colour"));
        assert!(text.contains("2>"));
        assert!(text.contains("&>"));
        assert!(text.contains("unquoted static paths"));
    }

    #[test]
    fn unknown_topic_has_no_text() {
        assert!(topic("not-a-topic").is_none());
    }
}
