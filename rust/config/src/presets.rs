//! Built-in prompt and palette presets activated with `wave preset <name>`.

/// Definition of a built-in prompt preset (`coral` palette + `wave` styles).
#[derive(Debug, Clone, Copy)]
pub struct Preset {
    pub name: &'static str,
    pub description: &'static str,
    pub colors: &'static [(&'static str, &'static str)],
    pub styles: &'static [(&'static str, &'static str)],
}

/// Ordered preset names for help text and error diagnostics.
pub const NAMES: &[&str] = &[
    "sharky",
    "neolambda",
    "pure",
    "robbyrussell",
    "powerline",
    "catppuccin",
    "tokyonight",
];

/// All built-in presets shipped inside `sharky-config`.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "sharky",
        description: "Ocean palette with compact single-line prompt",
        colors: &[
            ("abyss", "#071923"),
            ("foam", "#E4F5F4"),
            ("sea", "#35B8C5"),
            ("kelp", "#8CCFA8"),
            ("danger", "#FF8F80"),
            ("muted", "#6C8A96"),
        ],
        styles: &[
            ("prompt", "{cwd}[ {git}{git_dirty}][ {duration}] {symbol} "),
            ("prompt.symbol.glyph", "❯"),
            ("prompt.cwd", "@kelp"),
            ("prompt.git", "@sea"),
            ("prompt.git_dirty", "@danger"),
            ("prompt.duration", "@muted"),
            ("prompt.symbol", "@sea"),
            ("prompt.error", "@danger bold"),
            ("command", "@foam"),
            ("builtin", "@sea"),
            ("string", "@kelp"),
            ("variable", "@sea"),
            ("error", "@danger"),
            ("suggestion", "@muted"),
            ("selection", "@abyss on @sea"),
        ],
    },
    Preset {
        name: "neolambda",
        description: "Two-line rounded prompt inspired by Oh My Fish neolambda",
        colors: &[
            ("frame", "#FFFFFF"),
            ("text", "#FFFFFF"),
            ("user", "#DF005F"),
            ("host", "#DF5F00"),
            ("cwd", "#87FF00"),
            ("git", "#D787FF"),
            ("venv", "#5FD7FF"),
            ("nix", "#5FAFFF"),
            ("error", "#FF5F5F"),
            ("muted", "#8A8A8A"),
            ("cyan", "#56B6C2"),
            ("yellow", "#E5C07B"),
            ("abyss", "#071923"),
        ],
        styles: &[
            (
                "prompt",
                "{@frame:╭─}[({venv}) ]{user} {@text:at} {host} {@text:in} {cwd_full}[ {@text:on} {git}{git_dirty}][ {nix}][ {duration}]\n{@frame:╰─}{symbol} ",
            ),
            ("prompt.symbol.glyph", "λ"),
            ("prompt.user", "@user"),
            ("prompt.host", "@host"),
            ("prompt.cwd", "@cwd"),
            ("prompt.git", "@git"),
            ("prompt.git_dirty", "@error"),
            ("prompt.venv", "@venv"),
            ("prompt.nix", "@nix"),
            ("prompt.duration", "@muted"),
            ("prompt.symbol", "@frame"),
            ("prompt.error", "@error bold"),
            ("command", "@text"),
            ("builtin", "@cyan"),
            ("string", "@yellow"),
            ("variable", "@cyan"),
            ("error", "@error"),
            ("overwrite", "@error"),
            ("suggestion", "@muted"),
            ("selection", "@abyss on @cyan"),
        ],
    },
    Preset {
        name: "pure",
        description: "Minimal two-line prompt inspired by Pure",
        colors: &[
            ("cwd", "#5FAFFF"),
            ("git", "#8A8A8A"),
            ("dirty", "#FF87D7"),
            ("duration", "#D7AF5F"),
            ("symbol", "#D787FF"),
            ("error", "#FF5F5F"),
            ("text", "#E6EDF3"),
            ("cyan", "#56B6C2"),
            ("abyss", "#071923"),
        ],
        styles: &[
            ("prompt", "{cwd}[ {git}{git_dirty}][ {duration}]\n{symbol} "),
            ("prompt.symbol.glyph", "❯"),
            ("prompt.cwd", "@cwd"),
            ("prompt.git", "@git"),
            ("prompt.git_dirty", "@dirty"),
            ("prompt.duration", "@duration"),
            ("prompt.symbol", "@symbol"),
            ("prompt.error", "@error"),
            ("command", "@text"),
            ("builtin", "@cyan"),
            ("string", "@duration"),
            ("variable", "@cwd"),
            ("error", "@error"),
            ("suggestion", "@git"),
            ("selection", "@abyss on @cwd"),
        ],
    },
    Preset {
        name: "robbyrussell",
        description: "Compact single-line prompt inspired by robbyrussell",
        colors: &[
            ("ok", "#5FD787"),
            ("error", "#FF5F5F"),
            ("cwd", "#5FD7FF"),
            ("git", "#5F87FF"),
            ("branch", "#FF5F5F"),
            ("dirty", "#D7AF5F"),
            ("text", "#E6EDF3"),
            ("muted", "#7F848E"),
            ("abyss", "#071923"),
        ],
        styles: &[
            (
                "prompt",
                "{symbol} {cwd_short}[ {@git bold:git:(}{git}{@git bold:)}[ {git_dirty}]] ",
            ),
            ("prompt.symbol.glyph", "➜"),
            ("prompt.symbol", "@ok bold"),
            ("prompt.error", "@error bold"),
            ("prompt.cwd", "@cwd bold"),
            ("prompt.git", "@branch bold"),
            ("prompt.git_dirty", "@dirty bold"),
            ("command", "@text"),
            ("builtin", "@cwd"),
            ("string", "@dirty"),
            ("variable", "@git"),
            ("error", "@error"),
            ("suggestion", "@muted"),
            ("selection", "@abyss on @cwd"),
        ],
    },
    Preset {
        name: "powerline",
        description: "Background-styled segment bar with right-side duration and clock",
        colors: &[
            ("ink", "#071923"),
            ("light", "#E4F5F4"),
            ("sea", "#35B8C5"),
            ("kelp", "#8CCFA8"),
            ("danger", "#FF8F80"),
            ("slate", "#1E3440"),
            ("muted", "#9FB7C0"),
        ],
        styles: &[
            (
                "prompt",
                "{@ink on @sea bold: {cwd_short} }[{@ink on @kelp: {git}{git_dirty} }] {symbol} ",
            ),
            ("rprompt", "[{duration} ]{@muted:{time}}"),
            ("prompt.symbol.glyph", "❯"),
            ("prompt.symbol", "@sea bold"),
            ("prompt.error", "@danger bold"),
            ("prompt.duration", "@muted"),
            ("prompt.time", "@muted"),
            ("command", "@light"),
            ("builtin", "@sea"),
            ("string", "@kelp"),
            ("variable", "@sea"),
            ("error", "@danger"),
            ("suggestion", "@muted"),
            ("selection", "@ink on @sea"),
        ],
    },
    Preset {
        name: "catppuccin",
        description: "Catppuccin Mocha palette with crisp two-line prompt and right clock",
        colors: &[
            ("base", "#1E1E2E"),
            ("text", "#CDD6F4"),
            ("mauve", "#CBA6F7"),
            ("blue", "#89B4FA"),
            ("green", "#A6E3A1"),
            ("peach", "#FAB387"),
            ("red", "#F38BA8"),
            ("overlay", "#6C7086"),
            ("surface", "#313244"),
        ],
        styles: &[
            (
                "prompt",
                "{cwd}[ {git}{git_dirty}][ {duration}]\n{symbol} ",
            ),
            ("rprompt", "[{venv} ][{time}]"),
            ("prompt.symbol.glyph", "❯"),
            ("prompt.cwd", "@blue bold"),
            ("prompt.git", "@mauve"),
            ("prompt.git_dirty", "@peach"),
            ("prompt.duration", "@overlay"),
            ("prompt.venv", "@green"),
            ("prompt.time", "@overlay"),
            ("prompt.symbol", "@mauve bold"),
            ("prompt.error", "@red bold"),
            ("command", "@text"),
            ("builtin", "@blue"),
            ("string", "@green"),
            ("variable", "@peach"),
            ("error", "@red"),
            ("suggestion", "@overlay"),
            ("selection", "@base on @blue"),
        ],
    },
    Preset {
        name: "tokyonight",
        description: "Tokyo Night palette with user/host context and git status",
        colors: &[
            ("bg", "#1A1B26"),
            ("fg", "#C0CAF5"),
            ("cyan", "#7DCFFF"),
            ("blue", "#7AA2F7"),
            ("magenta", "#BB9AF7"),
            ("green", "#9ECE6A"),
            ("yellow", "#E0AF68"),
            ("red", "#F7768E"),
            ("comment", "#565F89"),
        ],
        styles: &[
            (
                "prompt",
                "[{user}@{host} ]{cwd}[ {git}{git_dirty}][ {duration}]\n{symbol} ",
            ),
            ("prompt.symbol.glyph", "❯"),
            ("prompt.user", "@cyan"),
            ("prompt.host", "@blue"),
            ("prompt.cwd", "@blue bold"),
            ("prompt.git", "@magenta"),
            ("prompt.git_dirty", "@yellow"),
            ("prompt.duration", "@comment"),
            ("prompt.symbol", "@cyan bold"),
            ("prompt.error", "@red bold"),
            ("command", "@fg"),
            ("builtin", "@cyan"),
            ("string", "@green"),
            ("variable", "@magenta"),
            ("error", "@red"),
            ("suggestion", "@comment"),
            ("selection", "@bg on @cyan"),
        ],
    },
];

/// Look up a built-in preset by name.
pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|preset| preset.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_names_match_table() {
        let table_names: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        assert_eq!(NAMES, table_names.as_slice());
        for name in NAMES {
            assert!(find(name).is_some(), "missing preset {name}");
        }
        assert!(find("unknown").is_none());
    }
}
