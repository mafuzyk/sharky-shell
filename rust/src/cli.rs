use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::PathBuf;

use crate::shell::Shell;

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() -> i32 {
    let mut args = std::env::args().skip(1);
    let mut command = None;
    let mut script: Option<String> = None;
    let mut script_args = Vec::new();
    let mut no_config = false;
    let mut json = false;
    let mut state = false;
    let mut state_env = false;
    let mut interactive = false;

    while let Some(arg) = args.next() {
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
            "--json" => json = true,
            "--env" => state_env = true,
            "-i" => interactive = true,
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

    if state {
        return run_state(no_config, state_env);
    }

    let mut shell = Shell::new();
    shell.load_config(no_config);

    match (command, script) {
        (Some(source), _) => {
            if json {
                return run_json_command(&mut shell, &source);
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
                crate::repl::run(&mut shell)
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

fn run_json_command(shell: &mut Shell, source: &str) -> i32 {
    let before = shell.env.clone();
    let (status, stdout, stderr, duration_ms) = capture(|| shell.run(source));
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
    for (key, _) in &before {
        if !shell.env.iter().any(|(name, _)| name == key) {
            delta.push((key.clone(), None));
        }
    }
    delta.sort_by(|left, right| left.0.cmp(&right.0));

    let mut env_delta = String::from("{");
    for (index, (key, value)) in delta.iter().enumerate() {
        if index > 0 {
            env_delta.push(',');
        }
        env_delta.push_str(&json_string(key));
        env_delta.push(':');
        match value {
            Some(value) => env_delta.push_str(&json_string(value)),
            None => env_delta.push_str("null"),
        }
    }
    env_delta.push('}');

    let object = format!(
        "{{\"schema_version\":1,\"status\":{status},\"stdout\":{},\"stderr\":{},\"duration_ms\":{duration_ms},\"cwd\":{},\"env_delta\":{env_delta}}}",
        json_string(&stdout),
        json_string(&stderr),
        json_string(&shell.cwd.to_string_lossy()),
    );
    println!("{object}");
    status
}

fn capture(run: impl FnOnce() -> i32) -> (i32, String, String, u64) {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();

    let stdout_path = temp_path("out");
    let stderr_path = temp_path("err");
    let stdout_file = std::fs::File::create(&stdout_path).ok();
    let stderr_file = std::fs::File::create(&stderr_path).ok();

    let saved_stdout = unsafe { nix::libc::dup(1) };
    let saved_stderr = unsafe { nix::libc::dup(2) };
    if let Some(file) = &stdout_file {
        unsafe {
            nix::libc::dup2(file.as_raw_fd(), 1);
        }
    }
    if let Some(file) = &stderr_file {
        unsafe {
            nix::libc::dup2(file.as_raw_fd(), 2);
        }
    }

    let started = std::time::Instant::now();
    let status = run();
    let duration_ms = started.elapsed().as_millis() as u64;

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe {
        if saved_stdout >= 0 {
            nix::libc::dup2(saved_stdout, 1);
            nix::libc::close(saved_stdout);
        }
        if saved_stderr >= 0 {
            nix::libc::dup2(saved_stderr, 2);
            nix::libc::close(saved_stderr);
        }
    }
    drop(stdout_file);
    drop(stderr_file);

    let stdout = std::fs::read(&stdout_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let stderr = std::fs::read(&stderr_path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&stdout_path);
    let _ = std::fs::remove_file(&stderr_path);

    (status, stdout, stderr, duration_ms)
}

fn temp_path(kind: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "sharky-json-{}-{counter}-{kind}",
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
    println!("Usage: sharky [OPTIONS] [FILE]");
    println!("       sharky state --json [--env] [--no-config]");
    println!();
    println!("  -c COMMAND   run COMMAND and exit");
    println!("  -i           force interactive mode");
    println!("  --json       print one JSON object for -c or state");
    println!("  --env        include the environment in state JSON");
    println!("  state        print shell state as JSON (requires --json)");
    println!("  --no-config  do not load the configuration file");
    println!("  --version    print the version");
    println!("  --help       print this help");
}
