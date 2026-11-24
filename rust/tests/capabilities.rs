//! Integration tests for `sharky capabilities --json` and `sharky help [TOPIC] --json`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::agent::{NO_CONFIG, Session, json};

fn sharky(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(args)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("run sharky")
}

fn tree(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
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
fn capabilities_json_matches_live_agent_ready_protocol_and_creates_no_files() {
    let root = std::env::temp_dir().join(format!("sharky-cap-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let config_dir = root.join(".config/sharky");
    std::fs::create_dir_all(&config_dir).unwrap();
    // Unparsable config must not be read or flagged by `capabilities --json`.
    std::fs::write(config_dir.join("config.sharky"), "broken syntax {{\n").unwrap();

    let xdg_config = root.join(".config");
    let xdg_data = root.join(".local/share");
    let before = tree(&root);

    let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["capabilities", "--json"])
        .env("HOME", &root)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("XDG_DATA_HOME", &xdg_data)
        .output()
        .expect("run sharky capabilities --json");

    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "capabilities --json must keep stderr empty: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(before, tree(&root), "capabilities created files");

    let caps = json::parse_line(
        String::from_utf8(out.stdout)
            .expect("UTF-8 stdout")
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("valid capabilities JSON");

    assert_eq!(
        caps.get("schema_version"), Some(&json::Value::Int(1))
    );

    // The additive disclosure lists the redirection forms implemented by the
    // lexer/parser and the two agent transports accepted by the CLI.
    let redirection_forms = caps
        .get("redirection_forms")
        .and_then(json::Value::as_array)
        .expect("redirection_forms array");
    for form in ["|&", ">&-", "<&-", "<&N", ">&N", ">|"] {
        assert!(
            redirection_forms
                .iter()
                .any(|value| value.as_str() == Some(form)),
            "missing redirection form {form}"
        );
    }
    let transports = caps
        .get("agent_transports")
        .and_then(json::Value::as_array)
        .expect("agent_transports array");
    for transport in ["stdio", "socket", "mcp"] {
        assert!(
            transports
                .iter()
                .any(|value| value.as_str() == Some(transport)),
            "missing agent transport {transport}"
        );
    }

    assert!(
        caps.get("shell_version")
            .and_then(json::Value::as_str)
            .is_some_and(|v| !v.is_empty())
    );

    // Compare `capabilities.protocol` directly against `ready.protocol` of a live session.
    let mut session = Session::spawn(&["--no-config"]);
    let ready = session.json();
    assert_eq!(ready.get("type").and_then(json::Value::as_str), Some("ready"));
    assert_eq!(
        caps.get("protocol"),
        ready.get("protocol"),
        "capabilities.protocol drifted from ready.protocol"
    );
    let (status, _, _) = session.finish();
    assert!(status.success());

    let builtins = caps
        .get("builtins")
        .and_then(json::Value::as_array)
        .expect("builtins array");
    for expected in ["cd", "readonly", "getopts", "help"] {
        assert!(
            builtins.iter().any(|b| b.as_str() == Some(expected)),
            "missing builtin {expected}"
        );
    }

    let subcommands = caps
        .get("cli_subcommands")
        .and_then(json::Value::as_array)
        .expect("cli_subcommands array");
    for expected in [
        "agent",
        "capabilities",
        "code",
        "config explain",
        "doctor",
        "help",
        "state",
        "status",
    ] {
        assert!(
            subcommands.iter().any(|s| s.as_str() == Some(expected)),
            "missing subcommand {expected}"
        );
    }

    let syntax = caps.get("syntax_support").expect("syntax_support");
    assert!(syntax.get("supported").and_then(json::Value::as_array).is_some_and(|a| !a.is_empty()));
    let unsupported = syntax
        .get("unsupported")
        .and_then(json::Value::as_array)
        .expect("unsupported array");
    assert!(unsupported.iter().any(|s| s.as_str() == Some("coproc")));
    assert!(unsupported.iter().any(|s| s.as_str() == Some("filesystem_sandbox")));

    let iso = caps.get("isolation_guarantees").expect("isolation_guarantees");
    assert_eq!(iso.get("interpreter_context_isolated"), Some(&json::Value::Bool(true)));
    assert_eq!(iso.get("stdio_protocol_isolated"), Some(&json::Value::Bool(true)));
    assert_eq!(iso.get("process_group_supervision"), Some(&json::Value::Bool(true)));
    assert_eq!(iso.get("filesystem_sandbox"), Some(&json::Value::Bool(false)));
    assert_eq!(iso.get("opt_in_sandbox"), Some(&json::Value::Bool(true)));
    assert_eq!(iso.get("cross_session_locks"), Some(&json::Value::Bool(true)));
    assert_eq!(iso.get("env_redaction_opt_in"), Some(&json::Value::Bool(true)));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn capabilities_cli_validation_and_help() {
    let missing = sharky(&["capabilities"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(missing.stdout.is_empty());

    let bad = sharky(&["capabilities", "--unknown"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(bad.stdout.is_empty());

    let extra = sharky(&["capabilities", "--json", "extra"]);
    assert_eq!(extra.status.code(), Some(2));

    let help_flag = sharky(&["capabilities", "--help"]);
    let help_topic = sharky(&["help", "capabilities"]);
    assert_eq!(help_flag.status.code(), Some(0));
    assert_eq!(help_topic.status.code(), Some(0));
    assert_eq!(help_flag.stdout, help_topic.stdout);
    assert!(String::from_utf8_lossy(&help_flag.stdout).contains("capabilities --json"));
}

#[test]
fn help_json_index_topics_and_unknown_topic() {
    let plain_index = sharky(&["help"]);
    let json_index_out = sharky(&["help", "--json"]);
    assert_eq!(json_index_out.status.code(), Some(0));
    let json_index = json::parse_line(
        String::from_utf8(json_index_out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("valid help --json");
    assert_eq!(json_index.get("schema_version"), Some(&json::Value::Int(1)));
    assert_eq!(
        json_index.get("kind").and_then(json::Value::as_str),
        Some("index")
    );
    assert_eq!(
        json_index.get("text").and_then(json::Value::as_str),
        Some(String::from_utf8_lossy(&plain_index.stdout).as_ref())
    );
    assert!(
        json_index
            .get("builtins")
            .and_then(json::Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("cd")))
    );
    assert!(
        json_index
            .get("cli_topics")
            .and_then(json::Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("capabilities")))
    );
    assert!(
        json_index
            .get("config_topics")
            .and_then(json::Value::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("set history")))
    );

    for (args, expected_kind, expected_topic) in [
        (&["help", "cd", "--json"][..], "builtin", "cd"),
        (&["help", "--json", "cd"][..], "builtin", "cd"),
        (&["help", "agent", "--json"][..], "cli", "agent"),
        (&["help", "config", "explain", "--json"][..], "cli", "config explain"),
        (&["help", "set", "history", "--json"][..], "config", "set history"),
        (&["help", "tide", "history", "--json"][..], "config", "tide history"),
        (&["help", "--json", "--json"][..], "cli", "--json"),
    ] {
        let out = sharky(args);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{args:?}: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let doc = json::parse_line(
            String::from_utf8(out.stdout)
                .unwrap()
                .trim_end_matches('\n')
                .as_bytes(),
        )
        .expect("valid topic JSON");
        assert_eq!(doc.get("schema_version"), Some(&json::Value::Int(1)));
        assert_eq!(
            doc.get("kind").and_then(json::Value::as_str),
            Some(expected_kind),
            "{args:?} -> {doc:?}"
        );
        assert_eq!(
            doc.get("topic").and_then(json::Value::as_str),
            Some(expected_topic),
            "{args:?} -> {doc:?}"
        );
        assert!(
            doc.get("text")
                .and_then(json::Value::as_str)
                .is_some_and(|t| !t.trim().is_empty())
        );
    }

    let missing = sharky(&["help", "nosuchtopic", "--json"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stderr.is_empty());
    let err = json::parse_line(
        String::from_utf8(missing.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .as_bytes(),
    )
    .expect("valid error JSON");
    assert_eq!(
        err.get("error")
            .and_then(|e| e.get("code"))
            .and_then(json::Value::as_str),
        Some("UNKNOWN_TOPIC")
    );
}
