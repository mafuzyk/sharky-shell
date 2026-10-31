use crate::shell::Shell;

pub fn render(shell: &Shell) -> String {
    let template = shell
        .style("prompt")
        .map(str::to_string)
        .unwrap_or_else(|| "{cwd} {symbol} ".to_string());

    let mut out = String::new();
    let mut rest = template.as_str();

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };

        let name = &after[..close];
        match name {
            "cwd" => out.push_str(&painted(shell, "prompt.cwd", &display_cwd(shell))),
            "symbol" => out.push_str(&painted(shell, "prompt.symbol", "❯")),
            "status" => out.push_str(&shell.last_status.to_string()),
            "user" => out.push_str(&painted(shell, "prompt.user", &shell.var("USER"))),
            "host" => out.push_str(&painted(shell, "prompt.host", &shell.var("HOSTNAME"))),
            other => {
                out.push('{');
                out.push_str(other);
                out.push('}');
            }
        }

        rest = &after[close + 1..];
    }

    out.push_str(rest);
    out
}

pub fn display_cwd(shell: &Shell) -> String {
    let cwd = shell.cwd.display().to_string();
    let home = shell.var("HOME");
    if !home.is_empty()
        && let Some(rest) = cwd.strip_prefix(&home)
    {
        return if rest.is_empty() {
            "~".to_string()
        } else {
            format!("~{rest}")
        };
    }
    cwd
}

fn painted(shell: &Shell, key: &str, text: &str) -> String {
    if !shell.colour_enabled() || text.is_empty() {
        return text.to_string();
    }

    let Some(style) = shell.style(key) else {
        return text.to_string();
    };
    let Some(name) = style.strip_prefix('@') else {
        return text.to_string();
    };
    let Some(hex) = shell.colour(name) else {
        return text.to_string();
    };
    let Some((red, green, blue)) = rgb(hex) else {
        return text.to_string();
    };

    format!("\x1b[38;2;{red};{green};{blue}m{text}\x1b[0m")
}

fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    Some((
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ))
}
