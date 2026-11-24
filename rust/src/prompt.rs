use std::ffi::OsString;
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use sharky_config as config;
use sharky_config::style::{ColorRef, StyleSpec, parse_hex_rgb};

use crate::shell::Shell;
use crate::term;

const FILL_PREFIX: &str = "\x00SHARKY_FILL:";
const FILL_SUFFIX: char = '\x00';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptEngine {
    Native,
    Starship,
    Omp,
}

#[derive(Default)]
struct PromptCtx {
    git_branch: Option<Option<String>>,
    git_dirty: Option<bool>,
    user: Option<String>,
    host: Option<String>,
}

struct EvalResult {
    text: String,
    has_dynamic: bool,
    any_non_empty: bool,
}

fn active_engine(shell: &Shell) -> PromptEngine {
    match shell.setting(config::TIDE_PROMPT) {
        Some("omp") => PromptEngine::Omp,
        Some("starship") => PromptEngine::Starship,
        Some("native") => PromptEngine::Native,
        _ if shell.setting("starship") == Some("on") => PromptEngine::Starship,
        _ => PromptEngine::Native,
    }
}

#[cfg(test)]
fn render(shell: &Shell) -> String {
    expand_for_width(&render_raw(shell), term::width(), shell)
}

/// Evaluate the prompt without expanding `{fill}` spacers so the editor can
/// re-fit flexible rows cleanly when the terminal is resized.
pub fn render_raw(shell: &Shell) -> String {
    match active_engine(shell) {
        PromptEngine::Starship => {
            if let Some(prompt) = starship_prompt(shell, false) {
                return prompt;
            }
        }
        PromptEngine::Omp => {
            if let Some(prompt) = omp_prompt(shell, false) {
                return prompt;
            }
        }
        PromptEngine::Native => {}
    }

    render_native_raw(shell)
}

pub fn expand_for_width(raw: &str, columns: usize, shell: &Shell) -> String {
    expand_fills(raw, columns, shell)
}

pub fn render_right(shell: &Shell) -> Option<String> {
    if shell.setting("rprompt") == Some("off") {
        return None;
    }

    match active_engine(shell) {
        PromptEngine::Native => {
            let template = shell.style("rprompt")?;
            let mut ctx = PromptCtx::default();
            let result = eval_template(shell, template, true, &mut ctx);
            (!result.text.is_empty()).then_some(result.text)
        }
        PromptEngine::Starship => {
            if shell.setting("rprompt") == Some("on") {
                starship_prompt(shell, true).map(|text| text.trim_end_matches(['\r', '\n']).to_string())
            } else {
                None
            }
        }
        PromptEngine::Omp => {
            if shell.setting("rprompt") == Some("on") {
                omp_prompt(shell, true).map(|text| text.trim_end_matches(['\r', '\n']).to_string())
            } else {
                None
            }
        }
    }
    .filter(|text| !text.is_empty())
}

fn render_native_raw(shell: &Shell) -> String {
    let template = shell
        .style("prompt")
        .unwrap_or("{cwd} {symbol} ");
    let mut ctx = PromptCtx::default();
    eval_template(shell, template, true, &mut ctx).text
}

pub fn render_transient(shell: &Shell) -> String {
    let template = shell.style("prompt.transient").unwrap_or("{symbol} ");
    let mut ctx = PromptCtx::default();
    eval_template(shell, template, true, &mut ctx).text
}

pub fn render_continuation(shell: &Shell) -> String {
    let template = shell.style("prompt.continuation").unwrap_or("... ");
    let mut ctx = PromptCtx::default();
    eval_template(shell, template, true, &mut ctx).text
}

fn expand_fills(rendered: &str, columns: usize, shell: &Shell) -> String {
    if !rendered.contains(FILL_PREFIX) {
        return rendered.to_string();
    }

    let columns = columns.max(1);
    let mut out = String::with_capacity(rendered.len() + 32);
    for (idx, line) in rendered.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let Some(start) = line.find(FILL_PREFIX) else {
            out.push_str(line);
            continue;
        };
        let rest = &line[start + FILL_PREFIX.len()..];
        let Some(end_rel) = rest.find(FILL_SUFFIX) else {
            out.push_str(line);
            continue;
        };
        let fill_ch = rest[..end_rel].chars().next().unwrap_or(' ');
        let left = &line[..start];
        let right = &rest[end_rel + FILL_SUFFIX.len_utf8()..];
        let left_width = crate::editor::visual_width(left);
        let right_width = crate::editor::visual_width(right);
        if left_width.saturating_add(right_width).saturating_add(1) > columns {
            out.push_str(left);
            continue;
        }
        let pad_count = columns - left_width - right_width;
        let pad_text = fill_ch.to_string().repeat(pad_count);
        out.push_str(left);
        if fill_ch == ' ' {
            out.push_str(&pad_text);
        } else {
            out.push_str(&painted(shell, "prompt.fill", &pad_text));
        }
        out.push_str(right);
    }
    out
}

fn eval_template(shell: &Shell, template: &str, styled: bool, ctx: &mut PromptCtx) -> EvalResult {
    let mut out = String::new();
    let mut has_dynamic = false;
    let mut any_non_empty = false;
    let mut pos = 0usize;

    while pos < template.len() {
        let rest = &template[pos..];
        let Some(ch) = rest.chars().next() else {
            break;
        };

        if ch == '\\' {
            let after_bs = pos + ch.len_utf8();
            if let Some(next) = template[after_bs..].chars().next() {
                if matches!(next, '\\' | '[' | ']' | '{' | '}') {
                    out.push(next);
                    pos = after_bs + next.len_utf8();
                    continue;
                }
            }
            out.push('\\');
            pos = after_bs;
            continue;
        }

        if ch == '[' {
            if let Some(close) = find_matching_bracket(template, pos) {
                let inner_slice = &template[pos + 1..close];
                let inner = eval_template(shell, inner_slice, styled, ctx);
                if inner.has_dynamic {
                    has_dynamic = true;
                }
                if !inner.has_dynamic || inner.any_non_empty {
                    out.push_str(&inner.text);
                    if inner.has_dynamic && inner.any_non_empty {
                        any_non_empty = true;
                    }
                }
                pos = close + 1;
                continue;
            }
            out.push('[');
            pos += 1;
            continue;
        }

        if ch == '{' {
            if let Some(close) = find_matching_brace(template, pos) {
                let expr = &template[pos + 1..close];
                if let Some(colon) = find_top_level_colon(expr) {
                    let prefix = &expr[..colon];
                    if sharky_config::style::looks_like_style(prefix)
                        && let Ok(spec) = StyleSpec::parse(prefix)
                    {
                        let inner_slice = &expr[colon + 1..];
                        let inner = eval_template(shell, inner_slice, false, ctx);
                        if inner.has_dynamic {
                            has_dynamic = true;
                        }
                        if !inner.has_dynamic || inner.any_non_empty {
                            if styled {
                                out.push_str(&paint_spec(shell, &spec, &inner.text));
                            } else {
                                out.push_str(&inner.text);
                            }
                            if inner.has_dynamic && inner.any_non_empty {
                                any_non_empty = true;
                            }
                        }
                        pos = close + 1;
                        continue;
                    }
                }

                if expr == "fill" || expr.starts_with("fill:") {
                    let ch = expr
                        .strip_prefix("fill:")
                        .and_then(|s| s.chars().next())
                        .unwrap_or(' ');
                    out.push_str(FILL_PREFIX);
                    out.push(ch);
                    out.push(FILL_SUFFIX);
                    pos = close + 1;
                    continue;
                }

                if let Some((rendered, token_non_empty)) = eval_token(shell, expr, styled, ctx) {
                    has_dynamic = true;
                    if token_non_empty {
                        any_non_empty = true;
                    }
                    out.push_str(&rendered);
                } else {
                    out.push('{');
                    out.push_str(expr);
                    out.push('}');
                }
                pos = close + 1;
                continue;
            }
            out.push('{');
            pos += 1;
            continue;
        }

        out.push(ch);
        pos += ch.len_utf8();
    }

    EvalResult {
        text: out,
        has_dynamic,
        any_non_empty,
    }
}

fn find_matching_bracket(source: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut brace_depth = 0usize;
    let mut escaped = false;
    for (rel, ch) in source[open_pos..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            '[' if brace_depth == 0 => depth += 1,
            ']' if brace_depth == 0 => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_pos + rel);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_matching_brace(source: &str, open_pos: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut escaped = false;
    for (rel, ch) in source[open_pos..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open_pos + rel);
                }
            }
            _ => {}
        }
    }
    None
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

/// Evaluate one `{token}` placeholder. Returns `None` for unknown placeholders
/// so callers preserve `{unknown}` literally.
fn eval_token(
    shell: &Shell,
    name: &str,
    styled: bool,
    ctx: &mut PromptCtx,
) -> Option<(String, bool)> {
    let paint = |key: &str, raw: String| -> (String, bool) {
        if raw.is_empty() {
            return (String::new(), false);
        }
        let rendered = if styled {
            painted(shell, key, &raw)
        } else {
            raw
        };
        (rendered, true)
    };

    match name {
        "cwd" => Some(paint("prompt.cwd", display_cwd(shell))),
        "cwd_short" => Some(paint("prompt.cwd", display_cwd_short(shell))),
        "cwd_full" => Some(paint("prompt.cwd", shell.cwd.display().to_string())),
        "cwd_home" => Some(paint("prompt.cwd", display_cwd_home(shell))),
        "os_icon" => {
            let icon = shell
                .style("prompt.os.glyph")
                .map(str::to_string)
                .unwrap_or_else(|| detect_os_icon().to_string());
            Some(paint("prompt.os", icon))
        }
        "shell" => Some(paint("prompt.shell", "sharky".to_string())),
        "mem" => Some(paint("prompt.mem", memory_usage_pct())),
        "duration_ms" => Some(paint("prompt.duration", format_duration_ms(shell.last_duration_ms))),
        "user" => {
            let user = ctx.user.get_or_insert_with(|| resolve_user(shell)).clone();
            Some(paint("prompt.user", user))
        }
        "host" => {
            let host = ctx.host.get_or_insert_with(|| resolve_host(shell)).clone();
            Some(paint("prompt.host", host))
        }
        "git" => {
            let branch = ctx
                .git_branch
                .get_or_insert_with(|| git_branch(&shell.cwd))
                .clone();
            Some(paint("prompt.git", branch.unwrap_or_default()))
        }
        "git_dirty" => {
            let branch = ctx
                .git_branch
                .get_or_insert_with(|| git_branch(&shell.cwd));
            if branch.is_none() {
                return Some((String::new(), false));
            }
            let dirty = *ctx.git_dirty.get_or_insert_with(|| git_is_dirty(shell));
            if !dirty {
                return Some((String::new(), false));
            }
            let key = if shell.style("prompt.git_dirty").is_some() {
                "prompt.git_dirty"
            } else {
                "prompt.git"
            };
            Some(paint(key, "*".to_string()))
        }
        "duration" => {
            let formatted = format_duration(shell.last_duration_ms).unwrap_or_default();
            Some(paint("prompt.duration", formatted))
        }
        "symbol" => {
            let glyph = if shell.last_status != 0 {
                shell
                    .style("prompt.error.glyph")
                    .or_else(|| shell.style("prompt.symbol.glyph"))
                    .unwrap_or("❯")
            } else {
                shell.style("prompt.symbol.glyph").unwrap_or("❯")
            };
            let key = if shell.last_status != 0 {
                if shell.style("prompt.error").is_some() {
                    "prompt.error"
                } else if shell.style("error").is_some() {
                    "error"
                } else {
                    "prompt.symbol"
                }
            } else {
                "prompt.symbol"
            };
            Some(paint(key, glyph.to_string()))
        }
        "status" => Some(paint("prompt.status", shell.last_status.to_string())),
        "error_status" => {
            if shell.last_status == 0 {
                Some((String::new(), false))
            } else {
                let key = if shell.style("prompt.error").is_some() {
                    "prompt.error"
                } else {
                    "error"
                };
                Some(paint(key, shell.last_status.to_string()))
            }
        }
        "venv" => Some(paint("prompt.venv", resolve_venv(shell).unwrap_or_default())),
        "nix" => {
            let nix_val = if !shell.var("IN_NIX_SHELL").is_empty() {
                "nix".to_string()
            } else {
                String::new()
            };
            Some(paint("prompt.nix", nix_val))
        }
        "ssh" => {
            let ssh_val = if !shell.var("SSH_CONNECTION").is_empty() || !shell.var("SSH_TTY").is_empty() {
                "ssh".to_string()
            } else {
                String::new()
            };
            Some(paint("prompt.ssh", ssh_val))
        }
        "jobs" => {
            let count = shell.jobs_count();
            if count == 0 {
                Some((String::new(), false))
            } else {
                Some(paint("prompt.jobs", count.to_string()))
            }
        }
        "time" => Some(paint("prompt.time", local_time_hms())),
        _ => {
            if let Some(var_name) = name.strip_prefix("env:") {
                let val = shell.var(var_name);
                if val.is_empty() {
                    Some((String::new(), false))
                } else {
                    Some(paint("prompt.env", val))
                }
            } else {
                None
            }
        }
    }
}

fn resolve_user(shell: &Shell) -> String {
    for var in ["USER", "LOGNAME"] {
        let value = shell.var(var);
        if !value.is_empty() {
            return value;
        }
    }
    nix::unistd::User::from_uid(nix::unistd::geteuid())
        .ok()
        .flatten()
        .map(|user| user.name)
        .unwrap_or_default()
}

fn resolve_host(shell: &Shell) -> String {
    let var = shell.var("HOSTNAME");
    if !var.is_empty() {
        return var.split('.').next().unwrap_or(&var).to_string();
    }
    let mut buf = [0u8; 256];
    // SAFETY: stack buffer with explicit byte capacity passed to POSIX gethostname.
    let rc = unsafe { nix::libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc != 0 {
        return String::new();
    }
    buf[buf.len() - 1] = 0;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let lossy = String::from_utf8_lossy(&buf[..len]);
    lossy.split('.').next().unwrap_or(&lossy).to_string()
}

fn resolve_venv(shell: &Shell) -> Option<String> {
    let raw = shell.var("VIRTUAL_ENV");
    if !raw.is_empty() {
        return Path::new(&raw)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty());
    }
    let conda = shell.var("CONDA_DEFAULT_ENV");
    if !conda.is_empty() {
        return Some(conda);
    }
    None
}

fn local_time_hms() -> String {
    // SAFETY: stack-allocated `time_t` and `tm` passed cleanly to POSIX C helpers.
    unsafe {
        let mut now: nix::libc::time_t = 0;
        nix::libc::time(&mut now);
        let mut tm: nix::libc::tm = std::mem::zeroed();
        if nix::libc::localtime_r(&now, &mut tm).is_null() {
            return String::new();
        }
        format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
    }
}

fn git_is_dirty(shell: &Shell) -> bool {
    let bin = resolve_in_shell_path(shell, "git");
    let mut cmd = Command::new(bin);
    cmd.args([
        "-C",
        &shell.cwd.display().to_string(),
        "status",
        "--porcelain",
        "--untracked-files=no",
        "--ignore-submodules=dirty",
    ])
    .env("GIT_OPTIONAL_LOCKS", "0");
    let path = shell.var("PATH");
    if !path.is_empty() {
        cmd.env("PATH", path);
    }
    run_prompt_command(&mut cmd, 50).is_some()
}

fn resolve_in_shell_path(shell: &Shell, bin: &str) -> OsString {
    let path_var = shell.var("PATH");
    for dir in path_var.split(':').filter(|entry| !entry.is_empty()) {
        let candidate = Path::new(dir).join(bin);
        if let Ok(meta) = std::fs::metadata(&candidate)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return candidate.into_os_string();
        }
    }
    OsString::from(bin)
}

fn expand_tilde(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return format!("{home}/{rest}");
    }
    path.to_string()
}

fn run_prompt_command(cmd: &mut Command, timeout_ms: u64) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let fd = stdout.as_raw_fd();
    // SAFETY: valid owned pipe file descriptor during `stdout`'s lifetime.
    unsafe {
        let flags = nix::libc::fcntl(fd, nix::libc::F_GETFL);
        if flags >= 0 {
            let _ = nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK);
        }
    }

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        };
        let timeout = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut pfd = nix::libc::pollfd {
            fd,
            events: nix::libc::POLLIN,
            revents: 0,
        };
        // SAFETY: single valid pollfd pointing at open non-blocking pipe.
        let ready = unsafe { nix::libc::poll(&mut pfd, 1, timeout) };
        if ready < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        if ready == 0 {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }

        let mut reached_eof = false;
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    reached_eof = true;
                    break;
                }
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        if reached_eof {
            break;
        }
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    if !status.success() || bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn starship_prompt(shell: &Shell, right: bool) -> Option<String> {
    let bin = resolve_in_shell_path(shell, "starship");
    let mut cmd = Command::new(bin);
    cmd.arg("prompt");
    if right {
        cmd.arg("--right");
    }
    cmd.args([
        "--status",
        &shell.last_status.to_string(),
        "--cmd-duration",
        &shell.last_duration_ms.to_string(),
        "--jobs",
        &shell.jobs_count().to_string(),
        "--terminal-width",
        &term::width().to_string(),
    ])
    .env("STARSHIP_SHELL", "sharky")
    .env("PWD", &shell.cwd);

    let path = shell.var("PATH");
    if !path.is_empty() {
        cmd.env("PATH", path);
    }
    let config_path = shell.var("STARSHIP_CONFIG");
    if !config_path.is_empty() {
        cmd.env("STARSHIP_CONFIG", config_path);
    }

    run_prompt_command(&mut cmd, 200)
}

fn omp_config_path(shell: &Shell) -> Option<String> {
    if let Some(configured) = shell.setting(config::TIDE_OMP_CONFIG) {
        let trimmed = configured.trim();
        if !trimmed.is_empty() {
            return Some(expand_tilde(trimmed, &shell.var("HOME")));
        }
    }
    let env_theme = shell.var("POSH_THEME");
    if !env_theme.trim().is_empty() {
        return Some(expand_tilde(env_theme.trim(), &shell.var("HOME")));
    }
    None
}

fn omp_prompt(shell: &Shell, right: bool) -> Option<String> {
    let bin = resolve_in_shell_path(shell, "oh-my-posh");
    let mut cmd = Command::new(bin);
    cmd.args([
        "print",
        if right { "right" } else { "primary" },
        "--shell=universal",
        "--escape=false",
        "--status",
        &shell.last_status.to_string(),
        "--execution-time",
        &shell.last_duration_ms.to_string(),
        "--job-count",
        &shell.jobs_count().to_string(),
        "--terminal-width",
        &term::width().to_string(),
        "--pwd",
        &shell.cwd.display().to_string(),
    ]);
    if let Some(cfg) = omp_config_path(shell) {
        cmd.args(["--config", &cfg]);
    }
    if std::env::var_os("NO_COLOR").is_some() {
        cmd.arg("--plain");
    }
    cmd.env("PWD", &shell.cwd);
    let path = shell.var("PATH");
    if !path.is_empty() {
        cmd.env("PATH", path);
    }
    let posh_theme = shell.var("POSH_THEME");
    if !posh_theme.is_empty() {
        cmd.env("POSH_THEME", posh_theme);
    }

    let rendered = run_prompt_command(&mut cmd, 300)?;
    let cleaned = strip_shell_escapes(&rendered);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn strip_shell_escapes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            continue;
        }
        if ch == '\\' && matches!(chars.peek(), Some('[' | ']')) {
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

fn detect_os_icon() -> &'static str {
    static OS_ICON: OnceLock<&'static str> = OnceLock::new();
    OS_ICON.get_or_init(|| {
        let Ok(content) = std::fs::read_to_string("/etc/os-release") else {
            return "\u{F17C}";
        };
        let mut id = "";
        for line in content.lines() {
            if let Some(raw) = line.strip_prefix("ID=") {
                id = raw.trim().trim_matches('"').trim_matches('\'');
                break;
            }
        }
        match id.to_ascii_lowercase().as_str() {
            "nixos" => "\u{F313}",
            "arch" | "archarm" | "endeavouros" | "cachyos" => "\u{F303}",
            "fedora" => "\u{F30A}",
            "ubuntu" => "\u{F31B}",
            "debian" => "\u{F306}",
            "manjaro" => "\u{F312}",
            "alpine" => "\u{F300}",
            "gentoo" => "\u{F30D}",
            "void" => "\u{F32E}",
            _ => "\u{F17C}",
        }
    })
}

fn memory_usage_pct() -> String {
    let Ok(text) = std::fs::read_to_string("/proc/meminfo") else {
        return String::new();
    };
    let mut total: Option<u64> = None;
    let mut avail: Option<u64> = None;
    let mut free: Option<u64> = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(val) = parts.next().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        match key {
            "MemTotal:" => total = Some(val),
            "MemAvailable:" => avail = Some(val),
            "MemFree:" => free = Some(val),
            _ => {}
        }
    }
    let Some(total) = total.filter(|&v| v > 0) else {
        return String::new();
    };
    let available = avail.or(free).unwrap_or(0).min(total);
    let pct = ((total - available) as f64 / total as f64) * 100.0;
    format!("{pct:.2}%")
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

pub fn display_cwd_short(shell: &Shell) -> String {
    let display = display_cwd(shell);
    if display == "/" || display == "~" || !display.contains('/') {
        return display;
    }
    let leading_slash = display.starts_with('/');
    let parts: Vec<&str> = display.split('/').filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return display;
    }
    let last_idx = parts.len() - 1;
    let mut out = String::with_capacity(display.len());
    if leading_slash {
        out.push('/');
    }
    for (idx, part) in parts.iter().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        if idx == last_idx || *part == "~" || *part == "." || *part == ".." {
            out.push_str(part);
        } else if let Some(after_dot) = part.strip_prefix('.') {
            out.push('.');
            if let Some(ch) = after_dot.chars().next() {
                out.push(ch);
            }
        } else if let Some(ch) = part.chars().next() {
            out.push(ch);
        }
    }
    out
}

pub fn display_cwd_home(shell: &Shell) -> String {
    let short = display_cwd_short(shell);
    if short == "~" {
        "\u{F015}".to_string()
    } else if let Some(rest) = short.strip_prefix("~/") {
        format!("\u{F015}/{rest}")
    } else {
        short
    }
}

pub fn git_branch(cwd: &Path) -> Option<String> {
    let mut current = cwd.to_path_buf();
    loop {
        let dot_git = current.join(".git");
        if dot_git.is_dir() {
            return read_head_ref(&dot_git.join("HEAD"));
        }
        if dot_git.is_file() {
            let content = std::fs::read_to_string(&dot_git).ok()?;
            let gitdir = content.trim().strip_prefix("gitdir:")?.trim();
            let dir_path = Path::new(gitdir);
            let head_path = if dir_path.is_absolute() {
                dir_path.join("HEAD")
            } else {
                current.join(dir_path).join("HEAD")
            };
            return read_head_ref(&head_path);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn read_head_ref(head_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(head_path).ok()?;
    let trimmed = text.trim();
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        return Some(branch.to_string());
    }
    if let Some(other) = trimmed.strip_prefix("ref: ") {
        return Some(other.rsplit('/').next().unwrap_or(other).to_string());
    }
    if trimmed.len() >= 7 {
        Some(trimmed[..7].to_string())
    } else if !trimmed.is_empty() {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn format_duration(ms: u64) -> Option<String> {
    if ms < 1000 {
        return None;
    }
    let total_secs = ms / 1000;
    if total_secs >= 60 {
        let mins = total_secs / 60;
        let secs = total_secs % 60;
        Some(format!("{mins}m {secs}s"))
    } else {
        let tenths = (ms % 1000) / 100;
        Some(format!("{total_secs}.{tenths}s"))
    }
}

fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format_duration(ms).unwrap_or_else(|| format!("{ms}ms"))
    }
}

pub(crate) fn painted(shell: &Shell, key: &str, text: &str) -> String {
    if text.is_empty() || std::env::var_os("NO_COLOR").is_some() {
        return text.to_string();
    }

    // Without a `suggestion` style the ghost would use the terminal's normal
    // foreground, indistinguishable from the text the user typed.
    if key == "suggestion" && shell.style(key).is_none() {
        return format!("\x1b[2m{text}\x1b[0m");
    }

    if !shell.colour_enabled() {
        return text.to_string();
    }

    let Some(style) = shell.style(key) else {
        return text.to_string();
    };
    let Ok(spec) = StyleSpec::parse(style) else {
        return text.to_string();
    };
    paint_spec(shell, &spec, text)
}

fn paint_spec(shell: &Shell, spec: &StyleSpec, text: &str) -> String {
    if text.is_empty() || std::env::var_os("NO_COLOR").is_some() || !shell.colour_enabled() {
        return text.to_string();
    }

    let resolve_rgb = |color: &ColorRef| -> Option<(u8, u8, u8)> {
        match color {
            ColorRef::Named(name) => shell.colour(name).and_then(parse_hex_rgb),
            ColorRef::Rgb(r, g, b) => Some((*r, *g, *b)),
        }
    };

    let mut params: Vec<String> = Vec::new();
    if spec.bold {
        params.push("1".to_string());
    }
    if spec.dim {
        params.push("2".to_string());
    }
    if spec.italic {
        params.push("3".to_string());
    }
    if spec.underline {
        params.push("4".to_string());
    }
    if spec.reverse {
        params.push("7".to_string());
    }
    if let Some(fg) = &spec.fg
        && let Some((r, g, b)) = resolve_rgb(fg)
    {
        params.push(format!("38;2;{r};{g};{b}"));
    }
    if let Some(bg) = &spec.bg
        && let Some((r, g, b)) = resolve_rgb(bg)
    {
        params.push(format!("48;2;{r};{g};{b}"));
    }

    if params.is_empty() {
        text.to_string()
    } else {
        format!("\x1b[{}m{text}\x1b[0m", params.join(";"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PromptEngine, active_engine, display_cwd_home, display_cwd_short, expand_fills,
        format_duration, format_duration_ms, git_branch, memory_usage_pct, omp_config_path,
        read_head_ref, render, render_right, resolve_host, strip_shell_escapes,
    };
    use crate::shell::Shell;
    use sharky_config as config;
    use std::path::PathBuf;

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' && chars.next() == Some('[') {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    fn cwd_short_abbreviates_intermediate_segments() {
        let mut shell = Shell::new();
        shell.set_var("HOME", "/home/tester".to_string());
        shell.cwd = PathBuf::from("/home/tester/projects/sharky/rust/src");
        assert_eq!(display_cwd_short(&shell), "~/p/s/r/src");

        shell.cwd = PathBuf::from("/home/tester");
        assert_eq!(display_cwd_short(&shell), "~");

        shell.cwd = PathBuf::from("/var/log/nginx");
        assert_eq!(display_cwd_short(&shell), "/v/l/nginx");

        shell.cwd = PathBuf::from("/home/tester/.config/sharky");
        assert_eq!(display_cwd_short(&shell), "~/.c/sharky");
    }

    #[test]
    fn duration_formats_above_one_second() {
        assert_eq!(format_duration(0), None);
        assert_eq!(format_duration(999), None);
        assert_eq!(format_duration(1500), Some("1.5s".to_string()));
        assert_eq!(format_duration(65000), Some("1m 5s".to_string()));
    }

    #[test]
    fn git_branch_reads_head_and_gitdir_file() {
        let root = std::env::temp_dir().join(format!("sharky-prompt-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sub = root.join("repo/a/b");
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join("repo/.git/HEAD"), "ref: refs/heads/feat/fish\n").unwrap();
        assert_eq!(git_branch(&sub).as_deref(), Some("feat/fish"));

        std::fs::write(
            root.join("repo/.git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(read_head_ref(&root.join("repo/.git/HEAD")).as_deref(), Some("0123456"));

        let wt = root.join("worktree");
        let real_git = root.join("actual_git");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&real_git).unwrap();
        std::fs::write(real_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../actual_git\n").unwrap();
        assert_eq!(git_branch(&wt).as_deref(), Some("main"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_prompt_renders_cwd_and_symbol() {
        let mut shell = Shell::new();
        shell.set_var("HOME", "/home/tester".to_string());
        shell.cwd = PathBuf::from("/home/tester/sub");
        let rendered = render(&shell);
        assert!(rendered.contains("~/sub"), "rendered: {rendered:?}");
        assert!(rendered.contains('❯'), "rendered: {rendered:?}");
    }

    #[test]
    fn neolambda_preset_renders_exact_two_line_layout_and_error_style() {
        let mut shell = Shell::new();
        let cfg = config::parse_theme("wave preset neolambda\n", "theme.sharky").unwrap();
        shell.apply_config(cfg);
        shell.set_var("USER", "julia".to_string());
        shell.set_var("HOSTNAME", "jiggle-station.local".to_string());
        shell.set_var("HOME", "/home/julia".to_string());
        shell.set_var("VIRTUAL_ENV", "".to_string());
        shell.set_var("CONDA_DEFAULT_ENV", "".to_string());
        shell.set_var("IN_NIX_SHELL", "".to_string());
        shell.cwd = PathBuf::from("/home/julia");
        shell.last_duration_ms = 0;
        shell.last_status = 0;

        let ok = render(&shell);
        assert_eq!(strip_ansi(&ok), "╭─julia at jiggle-station in /home/julia\n╰─λ ");
        // User colour #DF005F -> 223;0;95, Host #DF5F00 -> 223;95;0, CWD #87FF00 -> 135;255;0.
        assert!(ok.contains("\x1b[38;2;223;0;95mjulia\x1b[0m"), "{ok:?}");
        assert!(ok.contains("\x1b[38;2;223;95;0mjiggle-station\x1b[0m"), "{ok:?}");
        assert!(ok.contains("\x1b[38;2;135;255;0m/home/julia\x1b[0m"), "{ok:?}");
        assert!(ok.contains("\x1b[38;2;255;255;255mλ\x1b[0m"), "{ok:?}");

        shell.last_status = 1;
        let err = render(&shell);
        assert!(err.contains("\x1b[1;38;2;255;95;95mλ\x1b[0m"), "{err:?}");
    }

    #[test]
    fn conditional_groups_and_inline_spans_prevent_sgr_bleed() {
        let mut shell = Shell::new();
        let cfg = config::parse_theme(
            "coral ink #071923\ncoral sea #35B8C5\ncoral muted #8A8A8A\nwave prompt \"{@ink on @sea bold: {cwd_short} }[ ({venv})][ {error_status}] \\\\[{symbol}\\\\] \"\nwave prompt.symbol.glyph \"λ\"\n",
            "theme.sharky",
        )
        .unwrap();
        shell.apply_config(cfg);
        shell.set_var("HOME", "/home/mafu".to_string());
        shell.set_var("VIRTUAL_ENV", "/tmp/.venv".to_string());
        shell.set_var("IN_NIX_SHELL", "".to_string());
        shell.cwd = PathBuf::from("/home/mafu/projects/sharky");
        shell.last_status = 0;

        let out = render(&shell);
        // Pill wraps the whole ` ~/p/sharky ` in one SGR sequence with zero inner resets.
        assert!(
            out.contains("\x1b[1;38;2;7;25;35;48;2;53;184;197m ~/p/sharky \x1b[0m"),
            "{out:?}"
        );
        assert_eq!(strip_ansi(&out), " ~/p/sharky  (.venv) [λ] ");

        shell.set_var("VIRTUAL_ENV", "".to_string());
        shell.set_var("CONDA_DEFAULT_ENV", "".to_string());
        shell.last_status = 42;
        let out_err = render(&shell);
        assert_eq!(strip_ansi(&out_err), " ~/p/sharky  42 [λ] ");
    }

    #[test]
    fn context_tokens_render_when_set_and_collapse_when_unset() {
        let mut shell = Shell::new();
        let cfg = config::parse(
            "wave prompt \"[{venv} ][{nix} ][{ssh} ][{error_status} ]{cwd_full} {symbol} \"\n",
            "config.sharky",
        )
        .unwrap();
        shell.apply_config(cfg);
        shell.set_var("VIRTUAL_ENV", "/opt/ml-env".to_string());
        shell.set_var("IN_NIX_SHELL", "impure".to_string());
        shell.set_var("SSH_CONNECTION", "10.0.0.1 1234 10.0.0.2 22".to_string());
        shell.cwd = PathBuf::from("/tmp/work");
        shell.last_status = 2;

        let out = render(&shell);
        assert_eq!(strip_ansi(&out), "ml-env nix ssh 2 /tmp/work ❯ ");

        shell.set_var("VIRTUAL_ENV", "".to_string());
        shell.set_var("CONDA_DEFAULT_ENV", "".to_string());
        shell.set_var("IN_NIX_SHELL", "".to_string());
        shell.set_var("SSH_CONNECTION", "".to_string());
        shell.set_var("SSH_TTY", "".to_string());
        shell.last_status = 0;
        let plain = render(&shell);
        assert_eq!(strip_ansi(&plain), "/tmp/work ❯ ");
    }

    #[test]
    fn host_falls_back_to_system_hostname_when_unset() {
        let mut shell = Shell::new();
        shell.set_var("HOSTNAME", "".to_string());
        assert!(!resolve_host(&shell).is_empty());
    }

    #[test]
    fn render_right_respects_rprompt_template_and_flag() {
        let mut shell = Shell::new();
        let cfg = config::parse(
            "coral muted #888888\nwave rprompt \"[{duration} ]{status}\"\nwave prompt.duration @muted\n",
            "config.sharky",
        )
        .unwrap();
        shell.apply_config(cfg);
        shell.last_duration_ms = 2300;
        shell.last_status = 0;

        let right = render_right(&shell).expect("native rprompt should render");
        assert_eq!(strip_ansi(&right), "2.3s 0");

        let off_cfg = config::parse(
            "tide rprompt off\nwave rprompt \"{status}\"\n",
            "config.sharky",
        )
        .unwrap();
        shell.apply_config(off_cfg);
        assert!(render_right(&shell).is_none());
    }

    #[test]
    fn engine_selection_omp_config_expansion_and_escape_cleanup() {
        let mut shell = Shell::new();
        assert_eq!(active_engine(&shell), PromptEngine::Native);

        shell.apply_config(config::parse("tide starship on\n", "config.sharky").unwrap());
        assert_eq!(active_engine(&shell), PromptEngine::Starship);

        shell.apply_config(
            config::parse(
                "tide prompt omp\ntide omp_config \"~/themes/cat.omp.json\"\n",
                "config.sharky",
            )
            .unwrap(),
        );
        assert_eq!(active_engine(&shell), PromptEngine::Omp);
        shell.set_var("HOME", "/home/julia".to_string());
        assert_eq!(
            omp_config_path(&shell).as_deref(),
            Some("/home/julia/themes/cat.omp.json")
        );

        assert_eq!(
            strip_shell_escapes("\\[\x1b[32m\\]~\\[\x1b[0m\\] \r"),
            "\x1b[32m~\x1b[0m "
        );
    }

    #[test]
    fn all_presets_render_without_unresolved_tokens() {
        for name in config::presets::NAMES {
            let mut shell = Shell::new();
            let cfg =
                config::parse_theme(&format!("wave preset {name}\n"), "theme.sharky").unwrap();
            shell.apply_config(cfg);
            shell.set_var("HOME", "/home/tester".to_string());
            shell.set_var("USER", "tester".to_string());
            shell.set_var("HOSTNAME", "box".to_string());
            shell.cwd = PathBuf::from("/home/tester/work");

            let rendered = strip_ansi(&render(&shell));
            assert!(!rendered.is_empty(), "{name} rendered empty");
            assert!(!rendered.contains('{'), "{name} leaked brace: {rendered:?}");
            assert!(!rendered.contains('['), "{name} leaked bracket: {rendered:?}");
        }
    }

    #[test]
    fn reverse_style_fill_and_system_tokens_work_in_declarative_theme() {
        let mut shell = Shell::new();
        let cfg = config::parse_theme(
            r#"
coral cyan #5BCFFA
coral accent #F5ABB9
coral ink #1D151D
coral light #E6EDF3
wave prompt "{@cyan:\u256D\u2500}{@ink on @cyan: {os_icon} }{@accent reverse:\uE0B0}{@ink on @accent: {cwd_home} }{fill}{@light on @ink: {duration_ms} {shell} {mem} }\n{@cyan:\u2570\u2500}{symbol} "
wave prompt.os.glyph "\uF17C"
wave prompt.symbol.glyph ">>"
wave prompt.symbol "@cyan bold"
"#,
            "theme.sharky",
        )
        .unwrap();
        shell.apply_config(cfg);
        shell.set_var("HOME", "/home/charlie".to_string());
        shell.cwd = PathBuf::from("/home/charlie");
        shell.last_duration_ms = 92;
        shell.last_status = 0;

        assert_eq!(display_cwd_home(&shell), "\u{F015}");
        shell.cwd = PathBuf::from("/home/charlie/projects/sharky");
        assert_eq!(display_cwd_home(&shell), "\u{F015}/p/sharky");
        shell.cwd = PathBuf::from("/home/charlie");

        assert_eq!(format_duration_ms(92), "92ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
        assert!(memory_usage_pct().ends_with('%'));

        let rendered = render(&shell);
        // SGR 7 reverse cutout for #F5ABB9 -> `\x1b[7;38;2;245;171;185m\x1b[0m`
        assert!(
            rendered.contains("\x1b[7;38;2;245;171;185m\u{E0B0}\x1b[0m"),
            "{rendered:?}"
        );
        // Right/left fill expands cleanly to target column width and omits right side when too narrow.
        let aligned = expand_fills("LEFT\x00SHARKY_FILL: \x00RIGHT", 20, &shell);
        assert_eq!(crate::editor::visual_width(&aligned), 20);
        assert_eq!(aligned, "LEFT           RIGHT");
        let exact = expand_fills("LEFT\x00SHARKY_FILL: \x00RIGHT", 10, &shell);
        assert_eq!(exact, "LEFT RIGHT");
        let drop_right = expand_fills("LEFT\x00SHARKY_FILL: \x00RIGHT", 9, &shell);
        assert_eq!(drop_right, "LEFT");
        let narrow = expand_fills("LEFT\x00SHARKY_FILL: \x00RIGHT", 6, &shell);
        assert_eq!(narrow, "LEFT");

        let line1_width = crate::editor::visual_width(rendered.lines().next().unwrap());
        assert_eq!(line1_width, crate::term::width().max(1));
        assert!(rendered.contains("92ms"));
        assert!(rendered.contains("sharky"));
        assert!(rendered.contains(">>"));
    }

    #[test]
    fn renders_env_token_transient_and_continuation_prompts() {
        let mut shell = Shell::new();
        shell.set_var("MY_ENV_TAG", "prod".to_string());
        let raw = r#"{cwd} [{env:MY_ENV_TAG}][MISSING:{env:EMPTY_TAG}] {symbol} "#;
        let mut ctx = super::PromptCtx::default();
        let evaluated = super::eval_template(&shell, raw, false, &mut ctx).text;
        assert!(evaluated.contains("prod"), "{evaluated:?}");
        assert!(!evaluated.contains("MISSING:"), "{evaluated:?}");

        assert_eq!(super::render_transient(&shell), "❯ ");
        assert_eq!(super::render_continuation(&shell), "... ");

        let cfg = sharky_config::parse(
            "style prompt.transient \"> \"\nstyle prompt.continuation \".. \"\n",
            "theme.sharky",
        )
        .unwrap();
        shell.apply_config(cfg);
        assert_eq!(super::render_transient(&shell), "> ");
        assert_eq!(super::render_continuation(&shell), ".. ");
    }
}
