use std::io::Write;
use std::os::fd::AsRawFd;

use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, fork};

use crate::shell::Shell;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns true when an OS string would be decoded lossily.
pub(crate) fn is_non_utf8(value: &std::ffi::OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    !value.as_bytes().is_ascii() && std::str::from_utf8(value.as_bytes()).is_err()
}

/// Emit one explicit warning when an OS string would be decoded lossily.
/// The offending bytes are never printed: only the affected item kind is
/// named, so a hostile path or value cannot inject text into diagnostics.
pub(crate) fn warn_non_utf8(kind: &str) {
    eprintln!(
        "sharky: warning: non-UTF-8 {kind} was converted lossily; bytes are preserved on disk but not in shell strings"
    );
}

/// Convenience helper for call sites with a single value.
pub(crate) fn warn_if_non_utf8(value: &std::ffi::OsStr, kind: &str) -> bool {
    if is_non_utf8(value) {
        warn_non_utf8(kind);
        return true;
    }
    false
}

pub fn run() -> i32 {
    let mut raw = std::env::args_os();
    let arg0 = raw.next().unwrap_or_default();
    let login_arg0 = std::path::Path::new(&arg0)
        .file_name()
        .map(|name| name.to_string_lossy().starts_with('-'))
        .unwrap_or(false);
    let mut arg_warned = false;
    let args = raw
        .map(|arg| {
            if !arg_warned && is_non_utf8(&arg) {
                warn_non_utf8("command-line argument");
                arg_warned = true;
            }
            arg.to_string_lossy().into_owned()
        })
        .collect::<Vec<_>>();

    // `config explain`, `status` and `doctor` are dispatched from `cli::run`
    // before the normal shell startup path, so none loads configuration into a
    // shell, applies `pearl`, evaluates profiles, writes files or prints
    // configuration values. `doctor` builds a bare `Shell` only to resolve tool
    // names, and parses config/theme files read-only as part of its checks.
    if let Some(code) = try_config_explain(&args) {
        return code;
    }
    if let Some(code) = try_capabilities(&args) {
        return code;
    }
    if let Some(code) = try_completions(&args) {
        return code;
    }
    if let Some(code) = crate::agent::try_cli(&args) {
        return code;
    }
    if let Some(code) = crate::agent::try_code_cli(&args) {
        return code;
    }
    if let Some(code) = crate::diagnostics::try_run(&args) {
        return code;
    }
    if let Some(code) = crate::help::try_cli(&args) {
        return code;
    }

    let mut args = args.into_iter();
    let mut login = login_arg0;
    let mut command = None;
    let mut command_args: Vec<String> = Vec::new();
    let mut script: Option<String> = None;
    let mut script_args = Vec::new();
    let mut no_config = false;
    let mut json = false;
    let mut state = false;
    let mut state_env = false;
    let mut interactive = false;
    let mut private = false;
    let mut check = false;
    let mut end_of_options = false;
    let mut short_options: Vec<char> = Vec::new();
    let mut named_options: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        if command.is_some() {
            command_args.push(arg);
            continue;
        }
        if script.is_some() {
            script_args.push(arg);
            continue;
        }
        if end_of_options {
            script = Some(arg);
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "--version" => {
                println!("sharky {VERSION}");
                return 0;
            }
            "--help" | "-h" => {
                print_help();
                return 0;
            }
            "--no-config" => no_config = true,
            "--check" => check = true,
            "--json" => json = true,
            "--env" => state_env = true,
            "-i" => interactive = true,
            "-P" | "--private" => private = true,
            "-l" | "--login" => login = true,
            "-c" => match args.next() {
                Some(source) => command = Some(source),
                None => {
                    eprintln!("sharky: -c: option requires an argument");
                    return 2;
                }
            },
            other if other.starts_with("--") => {
                eprintln!("sharky: unknown option: {other}");
                return 2;
            }
            other if other.starts_with('-') && other.len() > 1 => {
                let mut chars = other[1..].chars();
                while let Some(flag) = chars.next() {
                    match flag {
                        'i' => interactive = true,
                        'P' => private = true,
                        'l' => login = true,
                        'e' | 'u' | 'x' | 'C' | 'H' | 'E' | 'T' => short_options.push(flag),
                        'o' => match args.next() {
                            Some(name) => named_options.push(name),
                            None => {
                                eprintln!("sharky: -o: option requires an argument");
                                return 2;
                            }
                        },
                        'c' => match args.next() {
                            Some(source) => command = Some(source),
                            None => {
                                eprintln!("sharky: -c: option requires an argument");
                                return 2;
                            }
                        },
                        _ => {
                            eprintln!("sharky: unknown option: {other}");
                            return 2;
                        }
                    }
                }
            }
            other => {
                if other == "state" && script.is_none() && !state {
                    state = true;
                    continue;
                }
                script = Some(other.to_string());
            }
        }
    }

    if state && command.is_some() {
        eprintln!("sharky: cannot use state with -c");
        return 2;
    }
    if state && script.is_some() {
        eprintln!("sharky: cannot use state with a script");
        return 2;
    }
    if state && !json {
        eprintln!("sharky: state requires --json");
        return 2;
    }
    if state_env && !state {
        eprintln!("sharky: --env is only supported with state");
        return 2;
    }
    if json && script.is_some() {
        eprintln!("sharky: --json is only supported with -c or state");
        return 2;
    }
    if json && command.is_none() && !state {
        eprintln!("sharky: --json is only supported with -c or state");
        return 2;
    }

    if check {
        if interactive {
            eprintln!("sharky: --check cannot use -i");
            return 2;
        }
        if login {
            eprintln!("sharky: --check cannot use login mode");
            return 2;
        }
        if state {
            eprintln!("sharky: --check cannot use state");
            return 2;
        }
        if json {
            eprintln!("sharky: --check cannot use --json");
            return 2;
        }
        return match (command.as_deref(), script.as_deref()) {
            (Some(source), None) => check_source("-c", source),
            (None, Some(path)) => match std::fs::read_to_string(path) {
                Ok(source) => check_source(path, &source),
                Err(err) => {
                    eprintln!("sharky: {path}: {err}");
                    2
                }
            },
            _ => {
                eprintln!("sharky: --check requires -c or a file");
                2
            }
        };
    }

    if state {
        return run_state(no_config, state_env);
    }

    let mut shell = Shell::new();
    shell.load_config(no_config);
    for flag in short_options {
        shell.set_short_option(flag, true);
    }
    for name in named_options {
        if !shell.set_named_option(&name, true) {
            eprintln!("sharky: set: {name}: invalid option name");
            return 2;
        }
    }
    shell.set_command_string(command.is_some());
    if let Some(arg0) = command_args.first().cloned() {
        shell.set_command_args(Some(arg0), command_args[1..].to_vec());
    }
    if login && !(json && command.is_some()) {
        crate::profiles::login(&mut shell);
        if let Some(code) = honor_login_exit(&mut shell) {
            return code;
        }
    }

    match (command, script) {
        (Some(source), _) => {
            if json {
                return run_json_command(&mut shell, &source, login);
            }
            shell.run(&source)
        }
        (None, Some(path)) => shell.run_script(&path, &script_args),
        (None, None) => {
            let tty = crate::term::is_tty(0) && crate::term::is_tty(1);
            if interactive && !tty {
                eprintln!("sharky: interactive mode requires a terminal");
                return 1;
            }
            if tty {
                crate::repl::run(&mut shell, private)
            } else {
                match crate::term::read_stdin_lines() {
                    Ok(text) => shell.run(&text),
                    Err(err) => {
                        eprintln!("sharky: {err}");
                        1
                    }
                }
            }
        }
    }
}

/// Handle the static `config explain` subcommand. Returns `Some(exit_code)`
/// when the invocation starts with `config explain` (including its error
/// cases), or `None` when normal argument parsing should continue.
fn try_config_explain(args: &[String]) -> Option<i32> {
    // The subcommand occupies the first two arguments only. A later pair is a
    // script name followed by `explain` (or a `-c` command's trailing words)
    // and belongs to the normal argument parser.
    if args.first().map(String::as_str) != Some("config")
        || args.get(1).map(String::as_str) != Some("explain")
    {
        return None;
    }
    let tail = &args[2..];

    if let Some(option) = tail.iter().find(|arg| arg.starts_with('-')) {
        eprintln!("sharky: config explain: unexpected option '{option}'");
        return Some(2);
    }

    if tail.is_empty() {
        print_text(&sharky_config::guide::index_text());
        return Some(0);
    }

    let id = tail.join(" ");
    match sharky_config::guide::topic(&id) {
        Some(text) => {
            print_text(&text);
            Some(0)
        }
        None => {
            eprintln!("sharky: config explain: unknown topic '{id}'");
            Some(2)
        }
    }
}

/// Write plain text and flush it: `main` uses `process::exit`, which does not
/// run destructors, so buffered bytes after the last newline must be forced out.
fn print_text(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

// Syntax-only first slice: validates top-level syntax and does not promise
// checks inside nested bodies; nothing is executed, loaded or recorded.
fn check_source(name: &str, source: &str) -> i32 {
    match crate::parser::parse(source) {
        Ok(_) => 0,
        Err(err) => {
            eprintln!("{}", err.format_diagnostic(&format!("sharky: {name}")));
            2
        }
    }
}

fn run_state(no_config: bool, include_env: bool) -> i32 {
    let mut shell = Shell::new();
    shell.load_config(no_config);

    let mut fields = vec![
        ("schema_version".to_string(), "1".to_string()),
        ("cwd".to_string(), json_string(&shell.cwd.to_string_lossy())),
        ("last_status".to_string(), shell.last_status.to_string()),
        ("pid".to_string(), std::process::id().to_string()),
    ];
    if include_env {
        let mut env = String::from("{");
        let mut names: Vec<&(String, String)> = shell.env.iter().collect();
        names.sort_by(|left, right| left.0.cmp(&right.0));
        for (index, (key, value)) in names.iter().enumerate() {
            if index > 0 {
                env.push(',');
            }
            env.push_str(&json_string(key));
            env.push(':');
            env.push_str(&json_string(value));
        }
        env.push('}');
        fields.push(("env".to_string(), env));
    }

    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(key));
        out.push(':');
        out.push_str(value);
    }
    out.push('}');
    println!("{out}");
    0
}

/// Consumes an `exit` requested by a login profile before the mode dispatch.
///
/// bash leaves a login shell from its startup files: the `-c` string, script or
/// REPL never runs, the EXIT trap still fires and the profile's exports stay in
/// the shell's environment.
fn honor_login_exit(shell: &mut Shell) -> Option<i32> {
    shell.exit_requested.map(|code| shell.finish(code))
}

fn run_json_command(shell: &mut Shell, source: &str, login: bool) -> i32 {
    let before = shell.env.clone();
    let stdout_file = match crate::agent::capture::capture_file("out") {
        Ok(file) => file,
        Err(err) => {
            eprintln!("sharky: capture: {err}");
            return 1;
        }
    };
    let stderr_file = match crate::agent::capture::capture_file("err") {
        Ok(file) => file,
        Err(err) => {
            eprintln!("sharky: capture: {err}");
            return 1;
        }
    };
    let delta_file = match crate::agent::capture::capture_file("delta") {
        Ok(file) => file,
        Err(err) => {
            eprintln!("sharky: capture: {err}");
            return 1;
        }
    };

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    let started = std::time::Instant::now();
    let pid = match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            // SAFETY: the child owns these descriptors and exits without
            // unwinding, so it never touches the parent's file handles.
            unsafe {
                nix::libc::dup2(stdout_file.as_raw_fd(), 1);
                nix::libc::dup2(stderr_file.as_raw_fd(), 2);
            }
            shell.attach_json_exec(delta_file.as_raw_fd(), before.clone());
            if login {
                crate::profiles::login(shell);
            }
            if let Some(status) = honor_login_exit(shell) {
                write_delta(&delta_file, shell, &before);
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                unsafe { nix::libc::_exit(status) }
            }
            let status = shell.run(source);
            write_delta(&delta_file, shell, &before);
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            unsafe { nix::libc::_exit(status) }
        }
        Ok(ForkResult::Parent { child }) => child,
        Err(err) => {
            eprintln!("sharky: capture: fork: {err}");
            return 1;
        }
    };

    let status = match waitpid(pid, None) {
        Ok(WaitStatus::Exited(_, code)) => code,
        Ok(WaitStatus::Signaled(_, signal, _)) => 128 + signal as i32,
        _ => 1,
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let stdout = read_capture(&stdout_file);
    let stderr = read_capture(&stderr_file);
    let (cwd, env_delta) = match read_delta(&delta_file) {
        Some((cwd, delta)) => (cwd, delta),
        None => (json_string(&shell.cwd.to_string_lossy()), "{}".to_string()),
    };

    let object = format!(
        "{{\"schema_version\":1,\"status\":{status},\"stdout\":{},\"stderr\":{},\"duration_ms\":{duration_ms},\"cwd\":{cwd},\"env_delta\":{env_delta}}}",
        json_string(&stdout),
        json_string(&stderr),
    );
    println!("{object}");
    status
}

fn env_delta(shell: &Shell, before: &[(String, String)]) -> String {
    let delta = shell.env_delta(before);

    let mut out = String::from("{");
    for (index, (key, value)) in delta.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&json_string(key));
        out.push(':');
        match value {
            Some(value) => out.push_str(&json_string(value)),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    out
}

fn write_delta(file: &std::fs::File, shell: &Shell, before: &[(String, String)]) {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = file;
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(file, "{}", json_string(&shell.cwd.to_string_lossy()));
    let _ = writeln!(file, "{}", env_delta(shell, before));
}

pub(crate) fn write_delta_fd(fd: std::os::fd::RawFd, shell: &Shell, before: &[(String, String)]) {
    let text = format!(
        "{}\n{}\n",
        json_string(&shell.cwd.to_string_lossy()),
        env_delta(shell, before)
    );
    // SAFETY: the descriptor belongs to the JSON capture child and points at
    // the private delta file; a failed write only loses the delta fields.
    unsafe {
        nix::libc::lseek(fd, 0, nix::libc::SEEK_SET);
        let _ = nix::libc::write(fd, text.as_ptr() as *const _, text.len());
    }
}

fn read_delta(file: &std::fs::File) -> Option<(String, String)> {
    use std::io::{Seek, SeekFrom, Write};

    let mut file = file;
    let _ = file.seek(SeekFrom::Start(0));
    let _ = file.flush();
    let text = read_capture(file);
    let mut lines = text.lines();
    let cwd = lines.next()?.to_string();
    let delta = lines.next()?.to_string();
    Some((cwd, delta))
}

fn read_capture(file: &std::fs::File) -> String {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = file;
    let _ = file.seek(SeekFrom::Start(0));
    let mut bytes = Vec::new();
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn json_string(value: &str) -> String {
    crate::agent::json::quote(value)
}

fn cli_subcommands() -> &'static [&'static str] {
    &[
        "agent",
        "agent guide",
        "capabilities",
        "code",
        "completions",
        "config explain",
        "doctor",
        "help",
        "state",
        "status",
    ]
}

fn try_completions(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("completions") {
        return None;
    }
    let tail = &args[1..];
    if tail.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        if let Some(text) = crate::help::topic("completions") {
            print_text(&text);
        }
        return Some(0);
    }

    let mut list = false;
    let mut json = false;
    for arg in tail {
        match arg.as_str() {
            "list" if !list => list = true,
            "list" => {
                eprintln!("sharky: completions: duplicate argument 'list'");
                return Some(2);
            }
            "--json" => json = true,
            other if other.starts_with('-') => {
                eprintln!("sharky: completions: unexpected option '{other}'");
                return Some(2);
            }
            other => {
                eprintln!("sharky: completions: unexpected argument '{other}'");
                return Some(2);
            }
        }
    }
    if !list {
        eprintln!("sharky: completions: usage is sharky completions list --json");
        return Some(2);
    }
    if !json {
        eprintln!("sharky: completions: --json is required");
        return Some(2);
    }

    use crate::agent::json::{ProtocolInfo, Value};

    let str_array = |items: &[&str]| -> Value {
        Value::Array(items.iter().map(|s| Value::String((*s).to_string())).collect())
    };
    let mut builtins: Vec<&str> = crate::help::BUILTINS.to_vec();
    builtins.sort_unstable();
    let config_topics: Vec<&str> = sharky_config::guide::INDEX
        .iter()
        .map(|(name, _)| *name)
        .collect();
    let protocol = ProtocolInfo::default();
    let agent_ops: Vec<Value> = protocol.ops.into_iter().map(Value::String).collect();
    let code_builtins = Value::Object(vec![
        (
            "functions".to_string(),
            str_array(crate::agent::guide::CODE_FUNCTIONS),
        ),
        (
            "methods".to_string(),
            str_array(crate::agent::guide::CODE_METHODS),
        ),
    ]);

    let payload = Value::Object(vec![
        ("schema_version".to_string(), Value::Int(1)),
        ("shell_version".to_string(), Value::String(VERSION.to_string())),
        ("builtins".to_string(), str_array(&builtins)),
        ("cli_subcommands".to_string(), str_array(cli_subcommands())),
        ("cli_topics".to_string(), str_array(crate::help::CLI_TOPICS)),
        ("config_topics".to_string(), str_array(&config_topics)),
        ("agent_ops".to_string(), Value::Array(agent_ops)),
        ("code_builtins".to_string(), code_builtins),
        ("mcp_tools".to_string(), str_array(crate::agent::guide::MCP_TOOLS)),
    ]);

    let mut line = payload.encode();
    line.push('\n');
    print_text(&line);
    Some(0)
}

fn try_capabilities(args: &[String]) -> Option<i32> {
    if args.first().map(String::as_str) != Some("capabilities") {
        return None;
    }
    let tail = &args[1..];
    if tail.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        if let Some(text) = crate::help::topic("capabilities") {
            print_text(&text);
        }
        return Some(0);
    }

    let mut json = false;
    for arg in tail {
        match arg.as_str() {
            "--json" => json = true,
            other if other.starts_with('-') => {
                eprintln!("sharky: capabilities: unexpected option '{other}'");
                return Some(2);
            }
            other => {
                eprintln!("sharky: capabilities: unexpected argument '{other}'");
                return Some(2);
            }
        }
    }
    if !json {
        eprintln!("sharky: capabilities: --json is required");
        return Some(2);
    }

    let mut line = capabilities_value().encode();
    line.push('\n');
    print_text(&line);
    Some(0)
}

pub(crate) fn capabilities_value() -> crate::agent::json::Value {
    use crate::agent::json::{
        CROSS_SESSION_LOCKS_AVAILABLE, OPT_IN_SANDBOX_AVAILABLE, ProtocolInfo, Value,
    };
    use crate::ast::{Command, RedirectOp};
    use crate::lexer::{self, TokenKind};

    let str_array = |items: &[&str]| -> Value {
        Value::Array(items.iter().map(|s| Value::String((*s).to_string())).collect())
    };
    let mut builtins: Vec<&str> = crate::help::BUILTINS.to_vec();
    builtins.sort_unstable();

    // Probe the shipped lexer/parser rather than hardcoding a guess: a form is
    // advertised only when its exact operator round-trips to the expected AST
    // node. Transport names come from the agent CLI's own transport parser.
    let forms = [
        ("|&", "echo x |& cat", "pipe_err"),
        (">&-", "echo x >&-", "close_out"),
        ("<&-", "echo x <&-", "close_in"),
        ("<&N", "echo x <&9", "dup_in"),
        (">&N", "echo x >&9", "dup_out"),
        (">|", "echo x >| out", "force_out"),
    ];
    let mut redirection_forms = Vec::new();
    for (spelling, source, expected) in forms {
        let supported = match expected {
            "pipe_err" => lexer::tokenize(source).is_ok_and(|tokens| {
                tokens
                    .iter()
                    .any(|token| token.kind == TokenKind::PipeErr)
            }) && crate::parser::parse(source).is_ok_and(|program| {
                program.lines.first().and_then(|line| line.entries.first()).is_some_and(|entry| {
                    entry.and_or.first.merge_stderr.first() == Some(&true)
                })
            }),
            "close_out" | "close_in" | "dup_in" | "dup_out" | "force_out" => {
                crate::parser::parse(source).is_ok_and(|program| {
                    program.lines.iter().any(|line| {
                        line.entries.iter().any(|entry| {
                            entry.and_or.first.commands.iter().any(|command| {
                                let Command::Simple(simple) = command else {
                                    return false;
                                };
                                simple.redirects.iter().any(|redirect| match (expected, &redirect.op) {
                                    ("close_out", RedirectOp::Close) => redirect.fd == 1,
                                    ("close_in", RedirectOp::Close) => redirect.fd == 0,
                                    ("dup_in", RedirectOp::DupIn(9)) => redirect.fd == 0,
                                    ("dup_out", RedirectOp::Dup(9)) => redirect.fd == 1,
                                    ("force_out", RedirectOp::OutForce) => redirect.fd == 1,
                                    _ => false,
                                })
                            })
                        })
                    })
                })
            }
            _ => false,
        };
        if supported {
            redirection_forms.push(spelling);
        }
    }

    let mut unsupported = vec![
        "coproc",
        "nameref_declare_n",
        "loadable_builtins",
        "restricted_shell",
        "select_loop",
        "filesystem_sandbox",
    ];
    if !CROSS_SESSION_LOCKS_AVAILABLE {
        unsupported.push("cross_session_locks");
    }
    let onboarding = Value::Object(vec![
        (
            "guide_json".to_string(),
            Value::String("sharky agent guide --json".to_string()),
        ),
        (
            "guide_text".to_string(),
            Value::String("sharky agent guide".to_string()),
        ),
        (
            "help_json".to_string(),
            Value::String("sharky help --json".to_string()),
        ),
        (
            "completions_json".to_string(),
            Value::String("sharky completions list --json".to_string()),
        ),
        (
            "mcp_stdio".to_string(),
            Value::String("sharky agent --mcp --no-config".to_string()),
        ),
        (
            "session_stdio".to_string(),
            Value::String("sharky agent --stdio --no-config".to_string()),
        ),
        (
            "session_socket".to_string(),
            Value::String("sharky agent --socket <PATH> --no-config".to_string()),
        ),
        (
            "one_shot_code".to_string(),
            Value::String("sharky code -e '<PROGRAM>' --no-config".to_string()),
        ),
        (
            "one_shot_json".to_string(),
            Value::String("sharky --json -c '<COMMAND>' --no-config".to_string()),
        ),
    ]);

    Value::Object(vec![
        ("schema_version".to_string(), Value::Int(1)),
        ("shell_version".to_string(), Value::String(VERSION.to_string())),
        ("redirection_forms".to_string(), str_array(&redirection_forms)),
        (
            "agent_transports".to_string(),
            str_array(crate::agent::transport_names()),
        ),
        ("onboarding".to_string(), onboarding),
        ("protocol".to_string(), ProtocolInfo::default().to_value()),
        ("builtins".to_string(), str_array(&builtins)),
        ("cli_subcommands".to_string(), str_array(cli_subcommands())),
        (
            "mcp_tools".to_string(),
            str_array(crate::agent::guide::MCP_TOOLS),
        ),
        (
            "syntax_support".to_string(),
            Value::Object(vec![
                (
                    "supported".to_string(),
                    str_array(&[
                        "pipelines",
                        "and_or_lists",
                        "subshells",
                        "command_groups",
                        "functions",
                        "conditionals_if_elif_else",
                        "case_statements",
                        "for_c_for_loops",
                        "while_until_loops",
                        "arithmetic_eval",
                        "indexed_arrays",
                        "associative_arrays",
                        "process_substitution",
                        "here_documents",
                        "here_strings",
                        "extglob_patterns",
                        "globstar",
                        "job_control",
                        "traps",
                        "local_scopes",
                        "readonly_variables",
                        "noclobber",
                        "pipefail",
                        "errexit",
                    ]),
                ),
                (
                    "partial".to_string(),
                    str_array(&[
                        "parameter_expansion_flags",
                        "declare_attributes",
                        "interactive_compgen_less",
                    ]),
                ),
                ("unsupported".to_string(), str_array(&unsupported)),
            ]),
        ),
        (
            "isolation_guarantees".to_string(),
            Value::Object(vec![
                ("interpreter_context_isolated".to_string(), Value::Bool(true)),
                ("stdio_protocol_isolated".to_string(), Value::Bool(true)),
                ("process_group_supervision".to_string(), Value::Bool(true)),
                ("filesystem_sandbox".to_string(), Value::Bool(false)),
                ("opt_in_sandbox".to_string(), Value::Bool(OPT_IN_SANDBOX_AVAILABLE)),
                ("cross_session_locks".to_string(), Value::Bool(CROSS_SESSION_LOCKS_AVAILABLE)),
                ("env_redaction_opt_in".to_string(), Value::Bool(true)),
            ]),
        ),
    ])
}

fn print_help() {
    print_text(&crate::help::index());
}
