use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, fork};

use crate::shell::Shell;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() -> i32 {
    let mut raw = std::env::args_os();
    let arg0 = raw.next().unwrap_or_default();
    let login_arg0 = std::path::Path::new(&arg0)
        .file_name()
        .map(|name| name.to_string_lossy().starts_with('-'))
        .unwrap_or(false);
    let args = raw
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();

    // `config explain`, `status` and `doctor` are static, text-only subcommands
    // handled before any Shell is built, so none of them applies `pearl`,
    // evaluates profiles or writes files. `doctor` parses config/theme files
    // read-only as part of its checks; no configuration values are printed.
    if let Some(code) = try_config_explain(&args) {
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
    let mut script: Option<String> = None;
    let mut script_args = Vec::new();
    let mut no_config = false;
    let mut json = false;
    let mut state = false;
    let mut state_env = false;
    let mut interactive = false;
    let mut private = false;
    let mut check = false;

    while let Some(arg) = args.next() {
        if script.is_some() {
            script_args.push(arg);
            continue;
        }
        match arg.as_str() {
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
            other if other.starts_with('-') => {
                eprintln!("sharky: unknown option: {other}");
                return 2;
            }
            other => {
                if other == "state" && script.is_none() && !state {
                    state = true;
                    continue;
                }
                if script.is_none() {
                    script = Some(other.to_string());
                } else {
                    script_args.push(other.to_string());
                }
            }
        }
    }

    if script.is_some() && command.is_some() {
        eprintln!("sharky: cannot use -c with a script");
        return 2;
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
    shell.set_command_string(command.is_some());
    if login && !(json && command.is_some()) {
        crate::profiles::login(&mut shell);
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
            let line = if err.line > 0 {
                format!(":{}", err.line)
            } else {
                String::new()
            };
            eprintln!("sharky: {name}{line}: {}", err.message);
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

fn run_json_command(shell: &mut Shell, source: &str, login: bool) -> i32 {
    let before = shell.env.clone();
    let stdout_file = match capture_file("out") {
        Ok(file) => file,
        Err(err) => {
            eprintln!("sharky: capture: {err}");
            return 1;
        }
    };
    let stderr_file = match capture_file("err") {
        Ok(file) => file,
        Err(err) => {
            eprintln!("sharky: capture: {err}");
            return 1;
        }
    };
    let delta_file = match capture_file("delta") {
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
    let mut delta: Vec<(String, Option<String>)> = Vec::new();
    for (key, value) in &shell.env {
        if before
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, old)| old)
            != Some(value)
        {
            delta.push((key.clone(), Some(value.clone())));
        }
    }
    for (key, _) in before {
        if !shell.env.iter().any(|(name, _)| name == key) {
            delta.push((key.clone(), None));
        }
    }
    delta.sort_by(|left, right| left.0.cmp(&right.0));

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

fn capture_file(kind: &str) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    for _ in 0..4 {
        let path = temp_path(kind);
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => {
                // The descriptor is all we need; the name is gone at once.
                let _ = std::fs::remove_file(&path);
                return Ok(file);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "capture file already exists",
    ))
}

fn temp_path(kind: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sharky-json-{}-{counter}-{nanos}-{kind}",
        std::process::id()
    ))
}

fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for char in value.chars() {
        match char {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            char if char < '\u{20}' => out.push_str(&format!("\\u{:04x}", char as u32)),
            char => out.push(char),
        }
    }
    out.push('"');
    out
}

fn print_help() {
    print_text(&crate::help::index());
}
