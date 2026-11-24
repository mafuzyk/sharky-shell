//! Tests for the static `help` registry shared by the CLI and the builtin.
//!
//! Every invocation runs the real binary: the help paths must work without a
//! shell, without configuration and without touching the filesystem.

use std::path::{Path, PathBuf};
use std::process::Command;

const NO_CONFIG: &str = "/nonexistent/sharky-help-test";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new("/tmp/opencode").join(format!("sharky-help-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sharky(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

/// Run the binary with caller-supplied environment overrides.
///
/// Deliberately does *not* set `SHARKY_CONFIG`: that variable takes precedence
/// over `XDG_CONFIG_HOME`/`HOME` in `config_dir()`, so setting it here would
/// mask any config planted in the XDG location and make config-related
/// assertions vacuous. Callers that want the legacy isolation pass
/// `SHARKY_CONFIG` explicitly in `envs`.
fn sharky_env(args: &[&str], envs: &[(&str, &Path)]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sharky"));
    command.args(args);
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("failed to run sharky")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Recursively list every path under `root`, sorted, so a test can prove the
/// help paths created no files.
fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path.clone());
            }
            paths.push(path);
        }
    }
    paths.sort();
    paths
}

#[test]
fn help_without_topic_matches_dash_dash_help_byte_for_byte() {
    let help = sharky(&["help"]);
    let flag = sharky(&["--help"]);
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
    assert_eq!(flag.status.code(), Some(0));
    assert_eq!(help.stdout, flag.stdout);
    assert!(!help.stdout.is_empty(), "help printed nothing");
}

#[test]
fn builtin_help_prints_the_same_index_as_the_cli() {
    let builtin = sharky(&["-c", "help"]);
    let flag = sharky(&["--help"]);
    assert_eq!(builtin.status.code(), Some(0), "{}", stderr(&builtin));
    assert_eq!(builtin.stdout, flag.stdout);
}

#[test]
fn builtin_topic_prints_the_same_text_as_the_cli() {
    let cli = sharky(&["help", "cd"]);
    let builtin = sharky(&["-c", "help cd"]);
    assert_eq!(cli.status.code(), Some(0), "{}", stderr(&cli));
    assert_eq!(builtin.status.code(), Some(0), "{}", stderr(&builtin));
    assert_eq!(cli.stdout, builtin.stdout);
    let text = stdout(&cli);
    assert!(text.contains("cd"), "cd topic missing its name: {text}");
    assert!(text.ends_with('\n'), "cd topic must end with a newline");
}

#[test]
fn cli_flag_topic_prints_text() {
    let out = sharky(&["help", "--check"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("--check"), "missing flag name: {text}");
    assert!(text.contains("syntax"), "missing flag meaning: {text}");
}

#[test]
fn subcommand_topics_print_text() {
    for topic in [
        "state",
        "config explain",
        "status",
        "doctor",
        "agent",
        "parallel",
        "workflow",
    ] {
        let out = sharky(&["help", topic]);
        assert_eq!(out.status.code(), Some(0), "{topic}: {}", stderr(&out));
        assert!(
            stdout(&out).contains(topic),
            "{topic} topic does not mention its name"
        );
    }
}

#[test]
fn agent_and_parallel_topics_describe_the_current_contract() {
    let agent = sharky(&["help", "agent"]);
    let parallel = sharky(&["help", "parallel"]);
    let workflow = sharky(&["help", "workflow"]);
    let builtin = sharky(&["-c", "help parallel"]);
    let builtin_wf = sharky(&["-c", "help workflow"]);
    for out in [&agent, &parallel, &workflow, &builtin, &builtin_wf] {
        assert_eq!(out.status.code(), Some(0), "{}", stderr(out));
        assert!(stdout(out).ends_with('\n'));
    }
    assert_eq!(parallel.stdout, builtin.stdout);
    assert_eq!(workflow.stdout, builtin_wf.stdout);

    let agent_text = stdout(&agent);
    for fragment in [
        "exec",
        "state",
        "cancel",
        "close",
        "parallel",
        "workflow",
        "help parallel",
        "help workflow",
        "sharky agent --help",
        "outcome:\"closed\"",
        "null id",
        "duration_us",
        "fd 0",
        "background job",
    ] {
        assert!(
            agent_text.contains(fragment),
            "help agent is missing {fragment:?}: {agent_text}"
        );
    }

    let parallel_text = stdout(&parallel);
    for fragment in [
        "help agent",
        "help workflow",
        "wait-all",
        "fail-fast",
        "max_concurrency",
        "timeout_ms",
        "REQUEST ORDER",
        "output_truncated",
        "duration_us",
        "target\":\"p1",
        "\"task\":\"a\"",
        "SAME filesystem",
    ] {
        assert!(
            parallel_text.contains(fragment),
            "help parallel is missing {fragment:?}: {parallel_text}"
        );
    }

    let workflow_text = stdout(&workflow);
    for fragment in [
        "help agent",
        "help parallel",
        "depends_on",
        "failure_policy",
        "retry",
        "idempotent",
        "condition",
        "lock",
        "attempts_detail",
        "REQUEST ORDER",
        "SAME filesystem",
    ] {
        assert!(
            workflow_text.contains(fragment),
            "help workflow is missing {fragment:?}: {workflow_text}"
        );
    }
}

#[test]
fn configuration_topics_delegate_to_the_config_guide() {
    let cli = sharky(&["help", "tide history"]);
    let guide = sharky(&["config", "explain", "tide history"]);
    let builtin = sharky(&["-c", "help tide history"]);
    assert_eq!(cli.status.code(), Some(0), "{}", stderr(&cli));
    assert_eq!(guide.status.code(), Some(0), "{}", stderr(&guide));
    assert_eq!(builtin.status.code(), Some(0), "{}", stderr(&builtin));
    assert_eq!(cli.stdout, guide.stdout);
    assert_eq!(builtin.stdout, guide.stdout);
    assert!(!cli.stdout.is_empty());
}

#[test]
fn configuration_index_topic_delegates_to_the_config_guide() {
    let cli = sharky(&["help", "files"]);
    let guide = sharky(&["config", "explain", "files"]);
    assert_eq!(cli.status.code(), Some(0), "{}", stderr(&cli));
    assert_eq!(cli.stdout, guide.stdout);
    assert!(stdout(&cli).contains("config.sharky"));
}

#[test]
fn unknown_topic_exits_one_in_both_paths() {
    // bash: `help nosuchtopic` exits 1 and says no help topics match.
    let cli = sharky(&["help", "nosuchtopic"]);
    let builtin = sharky(&["-c", "help nosuchtopic"]);
    for out in [&cli, &builtin] {
        assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
        assert!(out.stdout.is_empty(), "unknown topic printed to stdout");
        let err = stderr(out);
        assert!(err.contains("no help topics match"), "{err}");
        assert!(err.contains("nosuchtopic"), "{err}");
    }
}

#[test]
fn help_is_recognised_by_command_and_type() {
    let out = sharky(&["-c", "command -v help"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out).trim(), "help");

    let out = sharky(&["-c", "type help"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("shell builtin"), "{}", stdout(&out));
}

/// Extract every double-quoted token from `block`: the registry and the
/// builtin predicate list names that way.
fn quoted(block: &str) -> Vec<String> {
    block
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn shell_builtin_names() -> Vec<String> {
    let source = include_str!("../src/shell.rs");
    let start = source
        .find("fn is_builtin_name")
        .expect("shell.rs must define is_builtin_name");
    let rest = &source[start..];
    let end = rest.find("\n}").expect("is_builtin_name must have a body");
    quoted(&rest[..end])
}

fn registry_builtin_names() -> Vec<String> {
    let source = include_str!("../src/help.rs");
    let start = source
        .find("pub const BUILTINS")
        .expect("help.rs must define BUILTINS");
    let rest = &source[start..];
    let end = rest.find("];").expect("BUILTINS must be a slice literal");
    quoted(&rest[..end])
}

#[test]
fn help_topics_cover_exactly_the_builtin_names() {
    let mut builtins = shell_builtin_names();
    builtins.sort();
    builtins.dedup();
    assert!(!builtins.is_empty(), "no builtin names found in shell.rs");

    let mut registry = registry_builtin_names();
    registry.sort();
    registry.dedup();

    assert_eq!(
        registry, builtins,
        "help.rs BUILTINS drifted from shell.rs is_builtin_name"
    );

    for name in &builtins {
        let out = sharky(&["help", name]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "help {name} failed: {}",
            stderr(&out)
        );
        assert!(
            !stdout(&out).trim().is_empty(),
            "help {name} printed no text"
        );
    }
}

#[test]
fn cli_flags_and_subcommands_are_help_topics() {
    let source = include_str!("../src/help.rs");
    let start = source
        .find("pub const CLI_TOPICS")
        .expect("help.rs must define CLI_TOPICS");
    let rest = &source[start..];
    let end = rest.find("];").expect("CLI_TOPICS must be a slice literal");
    let topics = quoted(&rest[..end]);
    for topic in [
        "-c",
        "--check",
        "--json",
        "-i",
        "state",
        "config explain",
        "status",
        "doctor",
    ] {
        assert!(
            topics.iter().any(|entry| entry == topic),
            "CLI_TOPICS is missing {topic}"
        );
        let out = sharky(&["help", topic]);
        assert_eq!(out.status.code(), Some(0), "{topic}: {}", stderr(&out));
    }
}

/// Extract the flag tokens the help index advertises. Brackets around optional
/// flags (`[--env]`) and comma-separated short/long pairs (`-l, --login`) are
/// normalised, so every advertised flag becomes a plain `-x` or `--long` token.
fn advertised_flags(index: &str) -> Vec<String> {
    let mut flags: Vec<String> = index
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|char| matches!(char, '[' | ']' | ',' | '.' | '(' | ')'))
        })
        .filter(|token| token.starts_with('-') && token.len() > 1)
        .map(str::to_string)
        .collect();
    flags.sort();
    flags.dedup();
    flags
}

#[test]
fn every_flag_advertised_in_the_index_has_a_help_topic() {
    let help = sharky(&["--help"]);
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
    let flags = advertised_flags(&stdout(&help));

    // Guard against a parsing regression that would make the test vacuous.
    for known in ["-c", "--check", "--json", "--version", "--help"] {
        assert!(
            flags.iter().any(|flag| flag == known),
            "index flag parser missed {known}; parsed flags: {flags:?}"
        );
    }

    for flag in &flags {
        let out = sharky(&["help", flag]);
        assert_eq!(
            out.status.code(),
            Some(0),
            "index advertises {flag} but it has no help topic: {}",
            stderr(&out)
        );
        assert!(
            !stdout(&out).trim().is_empty(),
            "help {flag} printed no text"
        );
    }
}

#[test]
fn cli_help_reads_and_creates_no_configuration() {
    let root = scratch("cli-no-config");
    let config_dir = root.join(".config/sharky");
    std::fs::create_dir_all(&config_dir).unwrap();
    // Deliberately unparsable configuration in the XDG location. No
    // SHARKY_CONFIG is set, so config_dir() resolves to this directory.
    std::fs::write(
        config_dir.join("config.sharky"),
        "reef = broken\nnot a directive at all\n",
    )
    .unwrap();
    std::fs::write(config_dir.join("theme.sharky"), "coral\nwave {\n").unwrap();
    let xdg_config = root.join(".config");
    let xdg_data = root.join(".local/share");
    let envs = [
        ("HOME", root.as_path()),
        ("XDG_CONFIG_HOME", xdg_config.as_path()),
        ("XDG_DATA_HOME", xdg_data.as_path()),
    ];

    // Prove the fixture is live: the builtin `-c help` path *does* load
    // configuration, so the same broken file must surface a config error on
    // stderr there. If this fails, the "help must not read configuration"
    // assertion below could pass simply because nothing would be detected.
    let loaded = sharky_env(&["-c", "help"], &envs);
    assert!(
        stderr(&loaded).contains("config error"),
        "broken config was not detected when loaded: {}",
        stderr(&loaded)
    );

    let before = tree(&root);
    let out = sharky_env(&["help"], &envs);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(out.stdout, sharky(&["--help"]).stdout);
    assert!(
        stderr(&out).is_empty(),
        "help must not read configuration: {}",
        stderr(&out)
    );
    assert_eq!(before, tree(&root), "help created files");

    let out = sharky_env(&["help", "cd"], &envs);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(before, tree(&root), "help TOPIC created files");
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn builtin_help_works_with_an_unparsable_config_and_creates_no_files() {
    let root = scratch("builtin-no-config");
    let data = root.join(".local/share");
    std::fs::create_dir_all(&data).unwrap();
    let broken_dir = root.join("broken");
    std::fs::create_dir_all(&broken_dir).unwrap();
    std::fs::write(
        broken_dir.join("config.sharky"),
        "reef = broken\nnot a directive at all\n",
    )
    .unwrap();
    let before = tree(&root);

    let out = sharky_env(
        &["-c", "help"],
        &[
            ("HOME", &root),
            ("SHARKY_CONFIG", &broken_dir),
            ("XDG_DATA_HOME", &data),
            ("XDG_CONFIG_HOME", &root.join(".config")),
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(out.stdout, sharky(&["--help"]).stdout);
    assert_eq!(before, tree(&root), "builtin help created files");
    std::fs::remove_dir_all(&root).ok();
}
