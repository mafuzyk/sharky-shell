//! Declarative style parsing for `wave` values and inline `{style:...}` spans.

/// A colour referenced by `@name` from the palette or an inline `#RRGGBB` literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColorRef {
    Named(String),
    Rgb(u8, u8, u8),
}

/// Parsed terminal attributes and foreground/background colours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StyleSpec {
    pub fg: Option<ColorRef>,
    pub bg: Option<ColorRef>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// Parse a `#RRGGBB` hexadecimal colour string into RGB components.
pub fn parse_hex_rgb(value: &str) -> Option<(u8, u8, u8)> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}

/// Parse a single colour token (`@name` or `#RRGGBB`).
pub fn parse_color_ref(token: &str) -> Option<ColorRef> {
    if let Some(name) = token.strip_prefix('@') {
        if name.is_empty() || name.chars().any(|ch| ch.is_whitespace() || ch == ':') {
            return None;
        }
        return Some(ColorRef::Named(name.to_string()));
    }
    parse_hex_rgb(token).map(|(r, g, b)| ColorRef::Rgb(r, g, b))
}

/// Heuristic indicating whether a `wave` value or `{prefix:...}` should be
/// validated and rendered as a `StyleSpec` rather than an opaque literal.
pub fn looks_like_style(value: &str) -> bool {
    value.split_whitespace().any(|token| {
        token.starts_with('@')
            || parse_hex_rgb(token).is_some()
            || matches!(
                token,
                "on" | "bold" | "dim" | "italic" | "underline" | "reverse" | "invert"
            )
    })
}

impl StyleSpec {
    /// Parse a space-separated style specification such as
    /// `"@pink bold"` or `"@ink on @sea italic underline"`.
    pub fn parse(input: &str) -> Result<Self, String> {
        let mut spec = Self::default();
        let mut tokens = input.split_whitespace();
        let mut count = 0usize;

        while let Some(token) = tokens.next() {
            count += 1;
            match token {
                "bold" => spec.bold = true,
                "dim" => spec.dim = true,
                "italic" => spec.italic = true,
                "underline" => spec.underline = true,
                "reverse" | "invert" => spec.reverse = true,
                "on" => {
                    let Some(next) = tokens.next() else {
                        return Err("expected colour after 'on'".to_string());
                    };
                    if spec.bg.is_some() {
                        return Err("duplicate background colour".to_string());
                    }
                    let Some(color) = parse_color_ref(next) else {
                        return Err(format!("invalid background colour '{next}'"));
                    };
                    spec.bg = Some(color);
                }
                _ if token.starts_with('@') || token.starts_with('#') => {
                    if spec.fg.is_some() {
                        return Err("duplicate foreground colour".to_string());
                    }
                    let Some(color) = parse_color_ref(token) else {
                        return Err(format!("invalid colour '{token}'"));
                    };
                    spec.fg = Some(color);
                }
                other => {
                    return Err(format!("unknown style token '{other}'"));
                }
            }
        }

        if count == 0 {
            return Err("empty style".to_string());
        }

        Ok(spec)
    }

    /// Yield every `@name` palette reference in this style.
    pub fn named_colors(&self) -> impl Iterator<Item = &str> {
        let fg = match &self.fg {
            Some(ColorRef::Named(name)) => Some(name.as_str()),
            _ => None,
        };
        let bg = match &self.bg {
            Some(ColorRef::Named(name)) => Some(name.as_str()),
            _ => None,
        };
        fg.into_iter().chain(bg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_reference_and_hex() {
        assert_eq!(
            StyleSpec::parse("@kelp").unwrap(),
            StyleSpec {
                fg: Some(ColorRef::Named("kelp".to_string())),
                ..StyleSpec::default()
            }
        );
        assert_eq!(
            StyleSpec::parse("#FF8F80").unwrap(),
            StyleSpec {
                fg: Some(ColorRef::Rgb(0xFF, 0x8F, 0x80)),
                ..StyleSpec::default()
            }
        );
    }

    #[test]
    fn parses_background_and_modifiers_in_any_order() {
        let spec = StyleSpec::parse("bold @ink on @sea italic underline dim reverse").unwrap();
        assert_eq!(spec.fg, Some(ColorRef::Named("ink".to_string())));
        assert_eq!(spec.bg, Some(ColorRef::Named("sea".to_string())));
        assert!(spec.bold && spec.dim && spec.italic && spec.underline && spec.reverse);
        assert_eq!(spec.named_colors().collect::<Vec<_>>(), vec!["ink", "sea"]);

        let inverted = StyleSpec::parse("@pink invert").unwrap();
        assert!(inverted.reverse);
        assert!(looks_like_style("reverse"));
        assert!(looks_like_style("@pink invert"));
    }

    #[test]
    fn rejects_malformed_specs() {
        assert!(StyleSpec::parse("").is_err());
        assert!(StyleSpec::parse("@kelp on").is_err());
        assert!(StyleSpec::parse("@kelp bld").is_err());
        assert!(StyleSpec::parse("@").is_err());
        assert!(StyleSpec::parse("#12345").is_err());
        assert!(StyleSpec::parse("@a @b").is_err());
        assert!(StyleSpec::parse("on @a on @b").is_err());
    }

    #[test]
    fn looks_like_style_distinguishes_opaque_literals() {
        assert!(looks_like_style("@kelp"));
        assert!(looks_like_style("bold #FFFFFF"));
        assert!(looks_like_style("@ink on @sea"));
        assert!(!looks_like_style("x"));
        assert!(!looks_like_style("plain text"));
    }
}
