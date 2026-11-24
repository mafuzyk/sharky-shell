//! Machine-readable and plain-text onboarding guide for coding agents.
//!
//! Dispatched from `sharky agent guide [--json]` before shell startup so
//! agents that do not know Sharky can learn its CLI entrypoints, JSONL
//! protocol, native Code Mode, MCP tools, and safety boundaries in one call.

use std::io::Write;

use super::json::{
    CROSS_SESSION_LOCKS_AVAILABLE, OPT_IN_SANDBOX_AVAILABLE, ProtocolInfo, Value,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// All native MCP tool names served by `sharky agent --mcp`.
pub(crate) const MCP_TOOLS: &[&str] = &[
    "sharky_capabilities",
    "sharky_exec",
    "sharky_session_open",
    "sharky_session_exec",
    "sharky_parallel",
    "sharky_workflow",
    "sharky_code",
    "sharky_read_capture",
    "sharky_cancel",
    "sharky_session_close",
    "sharky_snapshot",
    "sharky_restore",
    "sharky_job_start",
    "sharky_job_poll",
    "sharky_job_send",
    "sharky_job_stop",
];

/// Native Code Mode global functions (`op:"code"` and `sharky code`).
pub(crate) const CODE_FUNCTIONS: &[&str] = &[
    "sh",
    "run",
    "parallel",
    "map_parallel",
    "workflow",
    "state",
    "read_capture",
    "len",
    "str",
    "int",
    "json_parse",
    "json_encode",
    "range",
];

/// Native Code Mode built-in methods across arrays, strings, and objects.
pub(crate) const CODE_METHODS: &[&str] = &[
    "map",
    "filter",
    "reduce",
    "sort",
    "sort_by",
    "group_by",
    "any",
    "all",
    "find",
    "unique",
    "trim",
    "lines",
    "split",
    "contains",
    "starts_with",
    "ends_with",
    "replace",
    "keys",
    "values",
    "get",
    "has",
];

/// Commands that expose machine-readable contracts before shell startup.
pub(crate) const DISCOVERY_COMMANDS: &[&str] = &[
    "sharky capabilities --json",
    "sharky agent guide --json",
    "sharky help --json",
    "sharky help <topic> --json",
    "sharky completions list --json",
];

fn str_list(items: &[&str]) -> Value {
    Value::Array(
        items
            .iter()
            .map(|item| Value::String((*item).to_string()))
            .collect(),
    )
}

fn kv(key: &str, value: Value) -> (String, Value) {
    (key.to_string(), value)
}

fn ks(key: &str, value: &str) -> (String, Value) {
    (key.to_string(), Value::String(value.to_string()))
}

/// Runs `sharky agent guide [--json]` before shell startup.
pub(crate) fn run(args: &[String]) -> i32 {
    if args.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        write_stdout(&plain_text());
        return 0;
    }

    let mut json_mode = false;
    for arg in args {
        match arg.as_str() {
            "--json" => json_mode = true,
            other if other.starts_with('-') => {
                eprintln!("sharky: agent guide: unexpected option '{other}'");
                return 2;
            }
            other => {
                eprintln!("sharky: agent guide: unexpected argument '{other}'");
                return 2;
            }
        }
    }

    if json_mode {
        let mut line = build_json_value().encode();
        line.push('\n');
        write_stdout(&line);
    } else {
        write_stdout(&plain_text());
    }
    0
}

/// Canonical onboarding document serialized by `sharky agent guide --json`.
pub(crate) fn build_json_value() -> Value {
    let protocol = ProtocolInfo::default();
    let entrypoints = Value::Object(vec![
        ks("one_shot_command", "sharky --json -c '<COMMAND>' --no-config"),
        ks("one_shot_code", "sharky code -e '<PROGRAM>' --no-config"),
        ks(
            "session_stdio",
            "sharky agent --stdio --no-config [--redact-env] [--state-file PATH]",
        ),
        ks(
            "session_socket",
            "sharky agent --socket <PATH> --no-config [--redact-env] [--state-file PATH]",
        ),
        ks("mcp_stdio", "sharky agent --mcp --no-config [--redact-env]"),
    ]);

    let golden_rules = Value::Array(vec![
        Value::String(
            "Prefer typed argv (exec.argv, parallel/workflow argv, or code run([...])) over interpolated shell strings for paths and dynamic arguments."
                .to_string(),
        ),
        Value::String(
            "Set isolated:true (with optional cwd/env) on one-off exec probes so cd, exports, traps, or exit never mutate or close the persistent session."
                .to_string(),
        ),
        Value::String(
            "Attach output projection (strip_ansi, grep, context, head_lines, tail_lines, max_bytes) to noisy commands and page raw logs via read_capture using capture_id."
                .to_string(),
        ),
        Value::String(
            "Use op:\"parallel\", op:\"workflow\" (depends_on/retry/condition/lock), or op:\"code\" (sh/run/parallel/map_parallel/workflow) to collapse multi-turn investigation into one request."
                .to_string(),
        ),
        Value::String(
            "Treat audit.failed_calls and audit.first_failure_status on op:\"code\" as ground truth: script values cannot hide non-zero child outcomes."
                .to_string(),
        ),
        Value::String(
            "Enable lock (cross-session flock), tty:true (PTY), stream:true (incremental chunks), or sandbox (true or {no_network, read_only_root, max_file_bytes, max_procs, cpu_seconds}) explicitly when required."
                .to_string(),
        ),
    ]);

    let operations = Value::Array(vec![
        op_doc(
            "exec",
            "Run shell source (`command`) or direct `argv` in the session or an isolated fork.",
            &["command", "argv", "stdin", "stdin_encoding", "timeout_ms", "env_delta", "isolated", "cwd", "env", "output", "tty", "stream", "lock", "sandbox"],
            "result",
        ),
        op_doc(
            "parallel",
            "Execute independent tasks concurrently with isolated interpreter state.",
            &["tasks", "policy", "max_concurrency", "timeout_ms"],
            "parallel",
        ),
        op_doc(
            "workflow",
            "Execute a validated task DAG with dependencies, retries, conditions, and locks.",
            &["tasks", "policy", "failure_policy", "max_concurrency", "timeout_ms"],
            "workflow",
        ),
        op_doc(
            "code",
            "Run a bounded native orchestration script with step/call budgets and an unforgeable audit envelope.",
            &["code", "args", "timeout_ms", "max_steps", "max_calls"],
            "code",
        ),
        op_doc(
            "read_capture",
            "Page raw stored stdout/stderr bytes from a prior attempt by capture_id without re-running.",
            &["capture_id", "stream", "line_start", "line_count", "byte_offset", "max_bytes", "grep", "context", "strip_ansi"],
            "capture",
        ),
        op_doc(
            "state",
            "Inspect read-only session state: cwd, last_status, options, and pid are always returned; optional include accepts env, variables, functions, aliases, traps, jobs.",
            &["include"],
            "state",
        ),
        op_doc(
            "cancel",
            "Signal an in-flight request or one parallel/workflow task's process group (TERM then KILL).",
            &["target", "task", "grace_ms"],
            "ack",
        ),
        op_doc(
            "close",
            "Cleanly shut down process groups, run EXIT trap once, optionally persist --state-file, and exit.",
            &["wait_ms"],
            "ack",
        ),
    ]);

    let code_mode_builtins = Value::Object(vec![
        kv("functions", str_list(CODE_FUNCTIONS)),
        kv("methods", str_list(CODE_METHODS)),
    ]);

    let recipes = Value::Object(vec![
        ks(
            "exec_argv_projected",
            r#"{"schema_version":1,"id":"r1","op":"exec","argv":["cargo","test","--no-fail-fast"],"output":{"strip_ansi":true,"grep":"FAILED|error\\[","context":2,"tail_lines":40,"max_bytes":8192}}"#,
        ),
        ks(
            "exec_isolated_probe",
            r#"{"schema_version":1,"id":"r2","op":"exec","isolated":true,"cwd":"/repo","env":{"CARGO_TERM_COLOR":"never"},"command":"git status --short"}"#,
        ),
        ks(
            "exec_pty_stream_sandbox",
            r#"{"schema_version":1,"id":"r3","op":"exec","tty":true,"stream":true,"sandbox":{"max_file_bytes":1048576,"cpu_seconds":30},"command":"printf ready"}"#,
        ),
        ks(
            "read_capture_page",
            r#"{"schema_version":1,"id":"r4","op":"read_capture","capture_id":"cap-1","stream":"stdout","line_start":1,"line_count":50}"#,
        ),
        ks(
            "parallel_fanout",
            r#"{"schema_version":1,"id":"r5","op":"parallel","policy":"fail-fast","max_concurrency":4,"tasks":[{"id":"fmt","argv":["cargo","fmt","--check"]},{"id":"clippy","argv":["cargo","clippy","--all-targets"]}]}"#,
        ),
        ks(
            "workflow_dag_with_lock",
            r#"{"schema_version":1,"id":"r6","op":"workflow","failure_policy":"skip","tasks":[{"id":"build","command":"cargo build","lock":"cargo-target"},{"id":"test","depends_on":["build"],"condition":{"task":"build","exit_code":{"eq":0}},"command":"cargo test","lock":"cargo-target"}]}"#,
        ),
        ks(
            "code_mode_dynamic_fanout",
            r#"{"schema_version":1,"id":"r7","op":"code","args":{"dir":"."},"code":"let files = run([\"find\", args.dir, \"-maxdepth\", \"2\", \"-name\", \"*.rs\"]).stdout.trim().lines().sort(); let out = map_parallel(files, 4, f => ({ argv: [\"wc\", \"-l\", f] })); { count: files.len(), lines: out.map(r => r.stdout.trim()) }"}"#,
        ),
    ]);

    let boundaries = Value::Object(vec![
        kv("interpreter_context_isolated", Value::Bool(true)),
        kv("stdio_protocol_isolated", Value::Bool(true)),
        kv("process_group_supervision", Value::Bool(true)),
        kv(
            "cross_session_locks",
            Value::Bool(CROSS_SESSION_LOCKS_AVAILABLE),
        ),
        kv("opt_in_sandbox", Value::Bool(OPT_IN_SANDBOX_AVAILABLE)),
        kv("default_filesystem_sandbox", Value::Bool(false)),
        ks("non_utf8_input_conversion", "lossy_with_stderr_warning"),
        ks("binary_output_encoding", "base64"),
    ]);

    Value::Object(vec![
        kv("schema_version", Value::Int(1)),
        ks("kind", "agent_guide"),
        ks("shell_version", VERSION),
        ks(
            "overview",
            "Sharky is a Bash-compatible Rust shell with an isolated JSONL Agent Runtime, fork-isolated parallel/DAG executors, bounded native Code Mode, and an optional native MCP stdio server.",
        ),
        kv("entrypoints", entrypoints),
        kv("golden_rules", golden_rules),
        kv("protocol", protocol.to_value()),
        kv("operations", operations),
        kv("code_mode_builtins", code_mode_builtins),
        kv("mcp_tools", str_list(MCP_TOOLS)),
        kv("recipes", recipes),
        kv("boundaries", boundaries),
        kv("discovery_commands", str_list(DISCOVERY_COMMANDS)),
    ])
}

fn op_doc(op: &str, summary: &str, fields: &[&str], response_type: &str) -> Value {
    Value::Object(vec![
        ks("op", op),
        ks("summary", summary),
        kv("key_fields", str_list(fields)),
        ks("response_type", response_type),
    ])
}

/// Concise human/LLM-readable onboarding guide printed by `sharky agent guide`.
pub(crate) fn plain_text() -> String {
    format!(
        "sharky agent guide (sharky {VERSION})\n\
         \n\
         Use Sharky directly via one-shot CLI, persistent JSONL sessions, Code Mode, or native MCP:\n\
         \n\
         1. Discovery (read-only, runs before shell startup):\n\
            sharky capabilities --json      live ops, features, limits, builtins, transports\n\
            sharky agent guide --json       structured recipes, golden rules, Code Mode & MCP guide\n\
            sharky help <topic> --json      topic text plus parsed flag schemas\n\
            sharky completions list --json  machine-readable list of builtins, ops, topics & MCP tools\n\
         \n\
         2. Entrypoints:\n\
            sharky --json -c 'COMMAND' --no-config\n\
            sharky code -e 'PROGRAM' [--args JSON] --no-config\n\
            sharky agent --stdio --no-config [--redact-env] [--state-file PATH]\n\
            sharky agent --socket PATH --no-config [--redact-env] [--state-file PATH]\n\
            sharky agent --mcp --no-config [--redact-env]\n\
         \n\
         3. Golden Rules for Coding Agents:\n\
            - Prefer typed `argv` (`exec.argv`, task `argv`, or `run([...])`) over shell string interpolation.\n\
            - Set `\"isolated\": true` (with `cwd`/`env`) for probes that must not mutate session state.\n\
            - Use `output` projection (`strip_ansi`, `grep`, `context`, `head_lines`, `tail_lines`, `max_bytes`)\n\
              and page raw logs on demand via `op:\"read_capture\"` using `capture_id`.\n\
            - Collapse multi-step file/test investigations into `parallel`, `workflow`, or `op:\"code\"`.\n\
            - Rely on `audit.failed_calls` / `audit.first_failure_status` in `type:\"code\"` responses.\n\
            - Query state (`op:\"state\"`): `cwd`, `last_status`, `options`, `pid` are always returned; `include` accepts `env`, `variables`, `functions`, `aliases`, `traps`, `jobs`.\n\
            - Request `lock` (cross-session flock), `tty: true` (PTY), `stream: true`, or `sandbox` (`true` or `{{no_network, read_only_root, max_file_bytes, max_procs, cpu_seconds}}`) when needed.\n\
         \n\
         4. Boundaries:\n\
            Isolated fork copies isolate shell interpreter state, not disk writes, unless `sandbox` is set.\n\
            All commands stay in supervised process groups; non-UTF-8 inputs warn once and binary streams use base64.\n"
    )
}

fn write_stdout(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_guide_recipes_decode_against_protocol() {
        let doc = build_json_value();
        let recipes = doc
            .get("recipes")
            .and_then(Value::as_object)
            .expect("recipes object");
        let limits = super::super::json::Limits::default();
        for (name, raw) in recipes {
            let line = raw
                .as_str()
                .unwrap_or_else(|| panic!("recipe {name} is not a string"));
            let val = super::super::json::parse_line(line.as_bytes())
                .unwrap_or_else(|err| panic!("recipe {name} invalid JSON: {err}"));
            super::super::protocol::decode(&val, &limits)
                .unwrap_or_else(|err| panic!("recipe {name} rejected by protocol::decode: {}", err.message));
        }
    }
}
